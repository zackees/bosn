//! The UI listener's routes, parsed eagerly into a closed `Route`. Every API
//! route *is* a typed [`CiRequest`]: there is no other dispatch path, so the
//! listener can do nothing the daemon socket cannot.

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use crate::ci::{CiRequest, RunnerAction, widget::WidgetCommand, wire::RunState};

#[derive(Debug, PartialEq)]
pub enum Route {
    /// The dashboard page (`GET /`, and deep links under `/ci/`).
    Page,
    /// Redeem a single-use grant for a session cookie (`GET /auth?token=`).
    Redeem { token: String, next: String },
    /// The live feed (`GET /v1/events`, server-sent events).
    Events,
    /// One typed CI operation.
    Api(Box<CiRequest>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum RouteError {
    NotFound,
    MethodNotAllowed,
    BadRequest(String),
}

impl Route {
    fn api(request: CiRequest) -> Self {
        Self::Api(Box::new(request))
    }

    /// Writes need a same-origin `Origin` header.
    pub fn is_write(&self) -> bool {
        let Self::Api(request) = self else {
            return false;
        };
        match request.as_ref() {
            CiRequest::Cancel { .. }
            | CiRequest::Retry { .. }
            | CiRequest::WidgetCommand { .. } => true,
            CiRequest::Runners { action } => {
                !matches!(action, RunnerAction::List | RunnerAction::CacheUsage)
            }
            _ => false,
        }
    }

    /// Only redeeming a grant works without a session.
    pub fn needs_session(&self) -> bool {
        !matches!(self, Self::Redeem { .. })
    }

    pub fn parse(
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: &[u8],
    ) -> Result<Self, RouteError> {
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let get = method == "GET";
        let post = method == "POST";
        let route = match (segments.as_slice(), get, post) {
            ([""] | ["app.js"] | ["app.css"] | ["shared.js"], true, _) => Route::Page,
            (["ci", ..], true, _) => Route::Page,
            (["widget", "bubble" | "panel" | "widget.js" | "widget.css"], true, _) => Route::Page,
            (["v1", "widget", "toggle"], _, true) => Route::api(CiRequest::WidgetCommand {
                command: WidgetCommand::Toggle,
            }),
            (["v1", "widget", "open"], _, true) => {
                let body: OpenBody = typed_body(body)?;
                Route::api(CiRequest::WidgetCommand {
                    command: WidgetCommand::Open { path: body.path },
                })
            }
            (["v1", "widget", "open-external"], _, true) => {
                let body: ExternalBody = typed_body(body)?;
                Route::api(CiRequest::WidgetCommand {
                    command: WidgetCommand::OpenExternal { url: body.url },
                })
            }
            (["auth"], true, _) => {
                let q: RedeemQuery = typed_query(query)?;
                let next = q.next.unwrap_or_else(|| "/".into());
                // Only same-origin relative paths: never an open redirect.
                if !next.starts_with('/') || next.starts_with("//") || next.contains('\\') {
                    return Err(RouteError::BadRequest("next must be a local path".into()));
                }
                Route::Redeem {
                    token: q.token,
                    next,
                }
            }
            (["v1", "events"], true, _) => Route::Events,
            (["v1", "runs"], true, _) => {
                let q: ListQuery = typed_query(query)?;
                Route::api(CiRequest::List {
                    workspace: q.workspace,
                    state: q.state,
                    limit: q.limit,
                })
            }
            (["v1", "runs", run], true, _) => {
                let q: ShowQuery = typed_query(query)?;
                Route::api(CiRequest::Show {
                    run: run_id(run)?,
                    tree: Some(q.tree.unwrap_or(true)),
                })
            }
            (["v1", "runs", run, "logs"], true, _) => {
                let q: LogsQuery = typed_query(query)?;
                Route::api(CiRequest::Logs {
                    run: run_id(run)?,
                    job: q.job,
                    section: q.section,
                    since_seq: q.since_seq,
                    limit: q.limit,
                    max_bytes: q.max_bytes,
                })
            }
            (["v1", "runs", run, "report"], true, _) => {
                let q: ReportQuery = typed_query(query)?;
                Route::api(CiRequest::Report {
                    run: run_id(run)?,
                    tail: q.tail,
                })
            }
            (["v1", "runners"], true, _) => Route::api(CiRequest::Runners {
                action: RunnerAction::List,
            }),
            (["v1", "runs", run, "cancel"], _, true) => {
                Route::api(CiRequest::Cancel { run: run_id(run)? })
            }
            (["v1", "runs", run, "retry"], _, true) => {
                let body: RetryBody = typed_body(body)?;
                Route::api(CiRequest::Retry {
                    run: run_id(run)?,
                    job: body.job,
                })
            }
            (["v1", "runners"], _, true) => Route::api(CiRequest::Runners {
                action: typed_body(body)?,
            }),
            (
                [""]
                | ["app.js"]
                | ["app.css"]
                | ["shared.js"]
                | ["ci", ..]
                | ["widget", ..]
                | ["v1", "widget", "toggle" | "open" | "open-external"]
                | ["auth"]
                | ["v1", "events"]
                | ["v1", "runs"]
                | ["v1", "runs", _]
                | ["v1", "runs", _, "logs" | "report" | "cancel" | "retry"]
                | ["v1", "runners"],
                _,
                _,
            ) => return Err(RouteError::MethodNotAllowed),
            _ => return Err(RouteError::NotFound),
        };
        Ok(route)
    }
}

fn run_id(value: &str) -> Result<String, RouteError> {
    if crate::ci::wire::valid_uuid(value) {
        Ok(value.into())
    } else {
        Err(RouteError::BadRequest("invalid run ID".into()))
    }
}

/// Parse query pairs eagerly into a typed struct (unknown keys refused).
fn typed_query<T: DeserializeOwned>(pairs: &[(String, String)]) -> Result<T, RouteError> {
    let mut map = Map::new();
    for (key, value) in pairs {
        let typed = match value.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            v => v
                .parse::<u64>()
                .map_or_else(|_| Value::String(v.into()), |n| Value::Number(n.into())),
        };
        if map.insert(key.clone(), typed).is_some() {
            return Err(RouteError::BadRequest(format!("duplicate parameter {key}")));
        }
    }
    serde_json::from_value(Value::Object(map)).map_err(|e| RouteError::BadRequest(e.to_string()))
}

fn typed_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, RouteError> {
    let body = if body.is_empty() {
        b"{}".as_slice()
    } else {
        body
    };
    serde_json::from_slice(body).map_err(|e| RouteError::BadRequest(e.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RedeemQuery {
    token: String,
    next: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    workspace: Option<String>,
    state: Option<RunState>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowQuery {
    tree: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogsQuery {
    job: Option<String>,
    section: Option<String>,
    since_seq: Option<u64>,
    limit: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportQuery {
    tail: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenBody {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalBody {
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryBody {
    job: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    fn parse(
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: &str,
    ) -> Result<Route, RouteError> {
        let query: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        Route::parse(method, path, &query, body.as_bytes())
    }

    /// Table-driven: every route maps to exactly one typed operation; a new
    /// path cannot appear without changing this table.
    #[test]
    fn every_route_is_a_typed_operation() {
        let runs = format!("/v1/runs/{RUN}");
        type Case<'a> = (&'a str, String, Vec<(&'a str, &'a str)>, &'a str, Route);
        let table: Vec<Case> = vec![
            ("GET", "/".into(), vec![], "", Route::Page),
            ("GET", format!("/ci/runs/{RUN}"), vec![], "", Route::Page),
            ("GET", "/v1/events".into(), vec![], "", Route::Events),
            (
                "GET",
                "/v1/runs".into(),
                vec![("state", "running"), ("limit", "5")],
                "",
                Route::api(CiRequest::List {
                    workspace: None,
                    state: Some(RunState::Running),
                    limit: Some(5),
                }),
            ),
            (
                "GET",
                runs.clone(),
                vec![],
                "",
                Route::api(CiRequest::Show {
                    run: RUN.into(),
                    tree: Some(true),
                }),
            ),
            (
                "GET",
                format!("{runs}/logs"),
                vec![("since_seq", "10")],
                "",
                Route::api(CiRequest::Logs {
                    run: RUN.into(),
                    job: None,
                    section: None,
                    since_seq: Some(10),
                    limit: None,
                    max_bytes: None,
                }),
            ),
            (
                "GET",
                format!("{runs}/report"),
                vec![],
                "",
                Route::api(CiRequest::Report {
                    run: RUN.into(),
                    tail: None,
                }),
            ),
            (
                "GET",
                "/v1/runners".into(),
                vec![],
                "",
                Route::api(CiRequest::Runners {
                    action: RunnerAction::List,
                }),
            ),
            (
                "POST",
                format!("{runs}/cancel"),
                vec![],
                "",
                Route::api(CiRequest::Cancel { run: RUN.into() }),
            ),
            (
                "POST",
                format!("{runs}/retry"),
                vec![],
                r#"{"job":"lint"}"#,
                Route::api(CiRequest::Retry {
                    run: RUN.into(),
                    job: Some("lint".into()),
                }),
            ),
            (
                "POST",
                "/v1/runners".into(),
                vec![],
                r#"{"action":"set_limit","limit":2}"#,
                Route::api(CiRequest::Runners {
                    action: RunnerAction::SetLimit { limit: 2 },
                }),
            ),
            ("GET", "/shared.js".into(), vec![], "", Route::Page),
            ("GET", "/widget/bubble".into(), vec![], "", Route::Page),
            ("GET", "/widget/panel".into(), vec![], "", Route::Page),
            (
                "POST",
                "/v1/widget/toggle".into(),
                vec![],
                "",
                Route::api(CiRequest::WidgetCommand {
                    command: WidgetCommand::Toggle,
                }),
            ),
            (
                "POST",
                "/v1/widget/open".into(),
                vec![],
                r#"{"path":"/ci"}"#,
                Route::api(CiRequest::WidgetCommand {
                    command: WidgetCommand::Open { path: "/ci".into() },
                }),
            ),
            (
                "POST",
                "/v1/widget/open-external".into(),
                vec![],
                r#"{"url":"https://github.com/o/r"}"#,
                Route::api(CiRequest::WidgetCommand {
                    command: WidgetCommand::OpenExternal {
                        url: "https://github.com/o/r".into(),
                    },
                }),
            ),
        ];
        let mut served = std::collections::BTreeSet::new();
        for (method, path, query, body, expected) in table {
            let route = parse(method, &path, &query, body).unwrap();
            assert_eq!(route, expected, "{method} {path}");
            if let Route::Api(request) = route {
                served.insert(operation(&request));
            }
        }
        // Every operation is either served by a route above or only on the
        // owner-only daemon socket. `operation` has no wildcard, so a new
        // CiRequest cannot compile until it is placed in one of the two.
        let socket_only = [
            "submit",
            "ui_grant",
            "widget_hello",
            "widget_poll",
            "widget_dismiss",
        ];
        let all = [
            "submit",
            "list",
            "show",
            "logs",
            "cancel",
            "retry",
            "report",
            "runners",
            "widget_hello",
            "widget_poll",
            "widget_dismiss",
            "widget_command",
            "ui_grant",
        ];
        for name in all {
            assert!(
                served.contains(name) != socket_only.contains(&name),
                "{name}: either served over HTTP or socket-only, not both or neither"
            );
        }
    }

    fn operation(request: &CiRequest) -> &'static str {
        match request {
            CiRequest::Submit { .. } => "submit",
            CiRequest::List { .. } => "list",
            CiRequest::Show { .. } => "show",
            CiRequest::Logs { .. } => "logs",
            CiRequest::Cancel { .. } => "cancel",
            CiRequest::Retry { .. } => "retry",
            CiRequest::Report { .. } => "report",
            CiRequest::Runners { .. } => "runners",
            CiRequest::WidgetHello { .. } => "widget_hello",
            CiRequest::WidgetPoll { .. } => "widget_poll",
            CiRequest::WidgetDismiss { .. } => "widget_dismiss",
            CiRequest::WidgetCommand { .. } => "widget_command",
            CiRequest::UiGrant { .. } => "ui_grant",
        }
    }

    #[test]
    fn writes_need_origin_and_unknown_inputs_are_refused() {
        let toggle = parse("POST", "/v1/widget/toggle", &[], "").unwrap();
        assert!(toggle.is_write(), "widget commands are writes");
        assert_eq!(
            parse("GET", "/widget/bubble", &[], "").unwrap(),
            Route::Page
        );
        assert_eq!(
            parse("GET", "/v1/widget/toggle", &[], ""),
            Err(RouteError::MethodNotAllowed)
        );
        assert!(matches!(
            parse("POST", "/v1/widget/open", &[], r#"{"path":"/","extra":1}"#),
            Err(RouteError::BadRequest(_))
        ));
        let cancel = parse("POST", &format!("/v1/runs/{RUN}/cancel"), &[], "").unwrap();
        assert!(cancel.is_write() && cancel.needs_session());
        assert!(!parse("GET", "/v1/runners", &[], "").unwrap().is_write());
        let redeem = parse("GET", "/auth", &[("token", "t")], "").unwrap();
        assert!(!redeem.needs_session());
        assert_eq!(parse("GET", "/v1/nope", &[], ""), Err(RouteError::NotFound));
        assert_eq!(
            parse("DELETE", "/v1/runners", &[], ""),
            Err(RouteError::MethodNotAllowed)
        );
        assert_eq!(
            parse("GET", &format!("/v1/runs/{RUN}/cancel"), &[], ""),
            Err(RouteError::MethodNotAllowed)
        );
        assert!(matches!(
            parse("GET", "/v1/runs", &[("bogus", "1")], ""),
            Err(RouteError::BadRequest(_))
        ));
        assert!(matches!(
            parse("GET", "/v1/runs/../../etc", &[], ""),
            Err(RouteError::NotFound | RouteError::BadRequest(_))
        ));
        assert!(matches!(
            parse("GET", "/v1/runs/not-a-uuid", &[], ""),
            Err(RouteError::BadRequest(_))
        ));
        for next in ["//evil.example", "https://evil.example", "\\\\x"] {
            assert!(
                parse("GET", "/auth", &[("token", "t"), ("next", next)], "").is_err(),
                "{next}"
            );
        }
        assert!(matches!(
            parse("POST", "/v1/runners", &[], r#"{"action":"rm_rf"}"#),
            Err(RouteError::BadRequest(_))
        ));
    }
}
