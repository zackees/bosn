//! The embedded dashboard: one HTML page, one script, one stylesheet, all
//! compiled into the binary (no CDN, fonts or analytics; it works with no
//! network beyond loopback). Deep links (`/ci/runs/<id>`) load the same page.

use kernal_api::http_server::Response;

const HTML: &str = include_str!("assets/index.html");
const SCRIPT: &str = include_str!("assets/app.js");
const STYLE: &str = include_str!("assets/app.css");

pub fn response(path: &str) -> Response {
    let (body, kind) = match path {
        "/app.js" => (SCRIPT, "text/javascript; charset=utf-8"),
        "/app.css" => (STYLE, "text/css; charset=utf-8"),
        _ => (HTML, "text/html; charset=utf-8"),
    };
    Response::new(200, body.as_bytes().to_vec())
        .and_then(|r| r.with_header("content-type", kind))
        .and_then(|r| r.with_header("cache-control", "no-store"))
        .unwrap_or_else(|_| Response::new(500, Vec::new()).expect("500 is valid"))
}
