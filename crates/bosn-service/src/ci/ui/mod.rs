//! The daemon's opt-in UI listener (`[ui] enabled = true`): the run dashboard,
//! the typed `/v1` API and a live server-sent-event feed, on `127.0.0.1`
//! only. See [`auth`] for the grant/cookie/Host/Origin rules and [`routes`]
//! for the closed route set (snapshot and control routes are typed requests).

pub mod auth;
mod page;
pub mod routes;
mod status_sse;

use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};

use kernal_api::{async_engine, http_server};

use self::{
    auth::Auth,
    routes::{Route, RouteError},
};
use super::config::UiConfig;
use super::{CiError, CiRuntime, events::RunEvent, reply::JsonReply};

/// What the CI runtime needs to issue grants: the listener's origin and auth.
pub struct UiHandle {
    pub origin: String,
    pub auth: Arc<Auth>,
}

/// A running listener; dropping it stops serving.
pub struct UiServer {
    pub handle: Arc<UiHandle>,
    /// Where the listener is bound (always loopback).
    pub local_addr: SocketAddr,
    _task: async_engine::Task<io::Result<()>>,
}

const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// Bind and serve when enabled; `Ok(None)` (and no port) when disabled.
pub async fn start(config: UiConfig, ci: CiRuntime) -> io::Result<Option<UiServer>> {
    if !config.enabled {
        return Ok(None);
    }
    let limits = http_server::Limits {
        max_connections: 64,
        max_request_body_bytes: 64 * 1024,
        max_response_body_bytes: 8 * 1024 * 1024,
        handler_timeout: Duration::from_secs(30),
        connection_timeout: Duration::from_secs(12 * 60 * 60),
        ..http_server::Limits::default()
    };
    let server = http_server::Server::bind(
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, config.port)),
        limits,
    )
    .await?
    .with_response_header("x-content-type-options", "nosniff")?
    .with_response_header("x-frame-options", "DENY")?
    .with_response_header("referrer-policy", "no-referrer")?
    .with_response_header(
        "content-security-policy",
        "default-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'",
    )?;
    let local_addr = server.local_addr()?;
    let port = local_addr.port();
    let auth = Arc::new(Auth::new(port));
    let handle = Arc::new(UiHandle {
        origin: auth.origin(),
        auth: auth.clone(),
    });
    let task = async_engine::launch(server.serve(move |request| {
        let (auth, ci) = (auth.clone(), ci.clone());
        async move { respond(&auth, &ci, request).await }
    }));
    Ok(Some(UiServer {
        handle,
        local_addr,
        _task: task,
    }))
}

fn text(status: u16, body: &str) -> http_server::Response {
    http_server::Response::new(status, body.as_bytes().to_vec())
        .and_then(|r| r.with_header("content-type", "text/plain; charset=utf-8"))
        .unwrap_or_else(|_| http_server::Response::new(500, Vec::new()).expect("500 is valid"))
}

fn json(status: u16, body: String) -> http_server::Response {
    http_server::Response::new(status, body.into_bytes())
        .and_then(|r| r.with_header("content-type", "application/json"))
        .and_then(|r| r.with_header("cache-control", "no-store"))
        .unwrap_or_else(|_| text(500, "response encoding failed"))
}

fn header<'a>(request: &'a http_server::Request, name: &str) -> Option<&'a str> {
    request
        .header(name)
        .and_then(|value| std::str::from_utf8(value).ok())
}

fn api_error(error: &CiError) -> http_server::Response {
    let status = match error.code {
        "not_found" => 404,
        "refused" | "invalid_request" => 400,
        _ => 500,
    };
    let body = crate::ci::ErrorReply {
        code: error.code.into(),
        message: error.message.clone(),
    };
    json(status, body.to_json())
}

/// Host check, route, session, Origin (for writes), then typed dispatch.
async fn respond(
    auth: &Auth,
    ci: &CiRuntime,
    request: http_server::Request,
) -> http_server::Response {
    if !auth.host_ok(header(&request, "host")) {
        return text(403, "forbidden host");
    }
    let query: Result<Vec<(String, String)>, _> = request.query_pairs().collect();
    let Ok(query) = query else {
        return text(400, "malformed query");
    };
    let route = match Route::parse(request.method(), request.path(), &query, request.body()) {
        Ok(route) => route,
        Err(RouteError::NotFound) => return text(404, "not found"),
        Err(RouteError::MethodNotAllowed) => return text(405, "method not allowed"),
        Err(RouteError::BadRequest(message)) => return text(400, &message),
    };
    if route.needs_session() && !auth.session_ok(header(&request, "cookie")) {
        return text(401, "sign in with `bosn ui` (a single-use link)");
    }
    if route.is_write() && !auth.origin_ok(header(&request, "origin")) {
        return text(403, "cross-origin write refused");
    }
    match route {
        Route::Page => page::response(request.path()),
        Route::Redeem { token, next } => match auth.redeem(&token).await {
            None => text(
                401,
                "this link was already used or has expired; run `bosn ui` again",
            ),
            Some(session) => http_server::Response::new(303, Vec::new())
                .and_then(|r| r.with_header("location", &next))
                .and_then(|r| r.with_header("set-cookie", &Auth::set_cookie(&session)))
                .unwrap_or_else(|_| text(500, "redirect failed")),
        },
        Route::Events => events(ci),
        Route::RunEvents { run, from_seq } => {
            let last = header(&request, "last-event-id");
            let last = match last.map(str::parse::<u64>).transpose() {
                Ok(last) => last,
                Err(_) => return text(400, "invalid Last-Event-ID"),
            };
            status_sse::response(ci, &run, from_seq.unwrap_or(0).max(last.unwrap_or(0)))
        }
        Route::Api(request) => match ci.handle(*request).await {
            Ok(value) => json(200, value.to_string()),
            Err(error) => api_error(&error),
        },
    }
}

/// The live feed. A reader that lags is sent `resync` and keeps going from
/// the newest retained event; it never slows the daemon or other readers.
fn events(ci: &CiRuntime) -> http_server::Response {
    let stream = ci.feed().subscribe().into_stream_with(|item| {
        let event = item.unwrap_or_else(|lagged| RunEvent::Resync {
            skipped: lagged.skipped,
        });
        Some(Ok(event.to_json()))
    });
    http_server::Response::event_stream(stream, SSE_KEEPALIVE)
        .unwrap_or_else(|_| text(500, "event stream unavailable"))
}

#[cfg(test)]
mod tests;
