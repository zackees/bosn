//! The embedded dashboard: one HTML page, one script, one stylesheet, all
//! compiled into the binary (no CDN, fonts or analytics; it works with no
//! network beyond loopback). Deep links (`/ci/runs/<id>`) load the same page.

use kernal_api::http_server::Response;

/// Every embedded asset, from the one directory that holds them.
macro_rules! asset {
    ($name:literal) => {
        include_str!(concat!("assets/", $name))
    };
}

const HTML: &str = asset!("index.html");
const SCRIPT: &str = asset!("app.js");
const SHARED_SCRIPT: &str = asset!("shared.js");
const STYLE: &str = asset!("app.css");
const BUBBLE: &str = asset!("bubble.html");
const PANEL: &str = asset!("panel.html");
const WIDGET_SCRIPT: &str = asset!("widget.js");
const WIDGET_STYLE: &str = asset!("widget.css");

pub fn response(path: &str) -> Response {
    let (body, kind) = match path {
        "/app.js" => (SCRIPT, "text/javascript; charset=utf-8"),
        "/shared.js" => (SHARED_SCRIPT, "text/javascript; charset=utf-8"),
        "/app.css" => (STYLE, "text/css; charset=utf-8"),
        "/widget/bubble" => (BUBBLE, "text/html; charset=utf-8"),
        "/widget/panel" => (PANEL, "text/html; charset=utf-8"),
        "/widget/widget.js" => (WIDGET_SCRIPT, "text/javascript; charset=utf-8"),
        "/widget/widget.css" => (WIDGET_STYLE, "text/css; charset=utf-8"),
        _ => (HTML, "text/html; charset=utf-8"),
    };
    Response::new(200, body.as_bytes().to_vec())
        .and_then(|r| r.with_header("content-type", kind))
        .and_then(|r| r.with_header("cache-control", "no-store"))
        .unwrap_or_else(|_| Response::new(500, Vec::new()).expect("500 is valid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_loads_the_shared_helpers_before_its_own_script() {
        for (page, own) in [
            (HTML, "/app.js"),
            (PANEL, "/widget/widget.js"),
            (BUBBLE, "/widget/widget.js"),
        ] {
            let shared = page
                .find(r#"<script src="/shared.js">"#)
                .expect("loads /shared.js");
            let script = page.find(&format!(r#"<script src="{own}">"#)).expect(own);
            assert!(shared < script, "{own} needs the helpers defined first");
        }
        assert!(SHARED_SCRIPT.contains("function confirmButton"));
    }
}
