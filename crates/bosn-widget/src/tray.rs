//! Prefer desktop-owned status area; keep the bubble when no host displays it.
use bosn_service::ci::{Conclusion, ListReply, RunState, widget::WidgetCommand};
use kernal_api::system_tray::{TrayEvent, TrayHandle, TrayOptions};

const ICON_SIZE: u16 = 32;

pub async fn register() -> Option<TrayHandle> {
    let options =
        TrayOptions::new("dev.bosn.widget", "Bosn CI", ICON_SIZE, icon(ICON_SIZE)).ok()?;
    TrayHandle::register(options).await.ok()
}

/// A blue ring around a dot, anti-aliased, as ARGB32 rows.
fn icon(size: u16) -> Vec<u8> {
    let size = f32::from(size);
    let centre = size / 2.0;
    let (blue, white) = ([0x2b, 0x8c, 0xd6], [0xff, 0xff, 0xff]);
    let mut pixels = Vec::with_capacity((size * size) as usize * 4);
    for y in 0..size as u32 {
        for x in 0..size as u32 {
            let dx = x as f32 + 0.5 - centre;
            let dy = y as f32 + 0.5 - centre;
            let distance = dx.hypot(dy);
            // Coverage of a disc of radius `r` at this pixel, 1 px soft edge.
            let disc = |r: f32| (r - distance + 0.5).clamp(0.0, 1.0);
            let ring = disc(centre - 1.0) - disc(centre - 6.0);
            let dot = disc(centre - 10.0);
            // The gap between ring and dot is a white fill, so the icon
            // reads on dark and light panels alike.
            let fill = disc(centre - 6.0) - dot;
            let alpha = ring + fill + dot;
            let colour = if fill > ring.max(dot) { white } else { blue };
            pixels.push((alpha * 255.0).round() as u8);
            pixels.extend(colour);
        }
    }
    pixels
}

/// Clicking the icon (or "Show activity") pops the activity panel up or
/// down; the dashboard and quit stay in its menu.
pub fn command(event: TrayEvent) -> WidgetCommand {
    match event {
        TrayEvent::Activate => WidgetCommand::Toggle,
        TrayEvent::OpenDashboard => WidgetCommand::Open { path: "/".into() },
        TrayEvent::Quit => WidgetCommand::Quit,
    }
}

/// The tooltip title: what is happening now, and the last result.
fn title(list: &ListReply) -> String {
    let runners = &list.runners;
    let mut parts = Vec::new();
    if runners.running > 0 {
        parts.push(format!("{} running", runners.running));
    }
    if runners.queued > 0 {
        parts.push(format!("{} queued", runners.queued));
    }
    let last = list
        .runs
        .iter()
        .find(|run| run.record.state == RunState::Done);
    match last.and_then(|run| run.record.conclusion) {
        _ if !parts.is_empty() => {}
        Some(Conclusion::Success) => parts.push("idle · last run passed".into()),
        Some(conclusion) => parts.push(format!("idle · last run {}", conclusion.as_str())),
        None => parts.push("idle".into()),
    }
    if runners.drained {
        parts.push("drained".into());
    }
    format!("Bosn CI: {}", parts.join(" · "))
}

/// Keep status useful while the icon is owned by the desktop panel.
pub async fn update(tray: &TrayHandle, client: &bosn_service::Client) {
    let title = match client.ci_list(None, None, Some(20)).await {
        Ok(list) => title(&list),
        Err(_) => "Bosn CI: reconnecting to daemon".into(),
    };
    let _ = tray.set_title(&title).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_actions_open_details_and_allow_deliberate_quit() {
        assert_eq!(command(TrayEvent::Activate), WidgetCommand::Toggle);
        assert_eq!(
            command(TrayEvent::OpenDashboard),
            WidgetCommand::Open { path: "/".into() }
        );
        assert_eq!(command(TrayEvent::Quit), WidgetCommand::Quit);
    }

    #[test]
    fn the_icon_is_square_argb_with_an_opaque_ring_and_clear_corners() {
        let pixels = icon(ICON_SIZE);
        let side = usize::from(ICON_SIZE);
        assert_eq!(pixels.len(), side * side * 4);
        let alpha = |x: usize, y: usize| pixels[(y * side + x) * 4];
        assert_eq!(alpha(0, 0), 0, "corners are transparent");
        assert_eq!(alpha(side / 2, 3), 255, "the ring is opaque");
        assert_eq!(alpha(side / 2, side / 2), 255, "the dot is opaque");
    }

    fn list(running: usize, queued: usize, drained: bool, last: Option<&str>) -> ListReply {
        let runs = last.map_or_else(Vec::new, |conclusion| {
            let mut run: serde_json::Value =
                serde_json::from_str(include_str!("../tests/run_view.json")).unwrap();
            run["conclusion"] = conclusion.into();
            vec![run]
        });
        serde_json::from_value(serde_json::json!({
            "runs": runs,
            "runners": {
                "limit": 2, "running": running, "queued": queued, "drained": drained,
                "engine": "act", "act_version": "x", "widget": "connected"
            }
        }))
        .unwrap()
    }

    #[test]
    fn the_title_says_what_runs_now_or_how_the_last_run_ended() {
        assert_eq!(
            title(&list(2, 1, false, Some("failure"))),
            "Bosn CI: 2 running · 1 queued"
        );
        assert_eq!(
            title(&list(0, 0, false, Some("failure"))),
            "Bosn CI: idle · last run failure"
        );
        assert_eq!(
            title(&list(0, 0, true, Some("success"))),
            "Bosn CI: idle · last run passed · drained"
        );
        assert_eq!(title(&list(0, 0, false, None)), "Bosn CI: idle");
    }
}
