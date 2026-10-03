//! Prefer desktop-owned status area; keep the bubble when no host displays it.
use bosn_service::ci::widget::WidgetCommand;
use kernal_api::system_tray::{TrayEvent, TrayHandle, TrayOptions};

pub async fn register() -> Option<TrayHandle> {
    let size = 32u16;
    let mut pixels = Vec::with_capacity(usize::from(size).pow(2) * 4);
    for y in 0..32i32 {
        for x in 0..32i32 {
            let inside = (x - 16).pow(2) + (y - 16).pow(2) < 225;
            pixels.extend(if inside { [255, 40, 160, 235] } else { [0; 4] });
        }
    }
    let options = TrayOptions::new("dev.bosn.widget", "Bosn CI activity", size, pixels).ok()?;
    TrayHandle::register(options).await.ok()
}
pub fn command(event: TrayEvent) -> WidgetCommand {
    match event {
        TrayEvent::Activate => WidgetCommand::Toggle,
        TrayEvent::OpenDashboard => WidgetCommand::Open { path: "/".into() },
        TrayEvent::Quit => WidgetCommand::Quit,
    }
}
/// Keep status useful while the icon is owned by the desktop panel.
pub async fn update(tray: &TrayHandle, client: &bosn_service::Client) {
    use bosn_service::ci::RunState;
    let title = match client
        .ci_list(None, Some(RunState::Running), Some(100))
        .await
    {
        Ok(list) => format!("Bosn CI: {} active runs", list.runs.len()),
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
        assert_eq!(command(TrayEvent::Quit), WidgetCommand::Quit);
    }
}
