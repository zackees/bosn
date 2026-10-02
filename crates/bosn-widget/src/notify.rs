//! Desktop notifications and the external-link opener, from the user
//! session (the daemon is headless). Notify on every failure, and on the
//! completion of runs a human started; agent successes stay silent.

use std::collections::BTreeSet;

use bosn_service::{
    Client,
    ci::{Conclusion, RunState, RunView},
};

#[derive(Default)]
pub struct Notifier {
    /// Finished runs already seen (the first poll only records, never notifies).
    seen: BTreeSet<String>,
    primed: bool,
}

/// Whether a finished run deserves a notification.
pub fn worth_notifying(run: &RunView) -> bool {
    match run.record.conclusion {
        Some(Conclusion::Success) => run.record.actor == "human",
        Some(_) => true,
        None => false,
    }
}

impl Notifier {
    pub async fn check(&mut self, client: &Client) {
        let Ok(list) = client.ci_list(None, Some(RunState::Done), Some(20)).await else {
            return;
        };
        for run in list.runs {
            if self.seen.insert(run.record.id.clone()) && self.primed && worth_notifying(&run) {
                notify(&run);
            }
        }
        self.primed = true;
    }
}

fn notify(run: &RunView) {
    let conclusion = run.record.conclusion.map_or("done", Conclusion::as_str);
    let summary = format!("bosn ci: {conclusion}");
    let body = format!(
        "{} · {} · {}",
        run.record.workflow.trim_start_matches(".github/workflows/"),
        &run.record.sha[..12.min(run.record.sha.len())],
        run.record.actor
    );
    // org.freedesktop.Notifications through libnotify's CLI until a
    // kernal-api notification facade exists.
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=bosn", &summary, &body])
        .spawn();
}

/// Open an allowlisted URL (the daemon already checked the allowlist).
pub fn open_external(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_always_notify_successes_only_for_humans() {
        let view = |conclusion, actor: &str| {
            let mut value: serde_json::Value =
                serde_json::from_str(include_str!("../tests/run_view.json")).unwrap();
            value["conclusion"] = serde_json::to_value(conclusion).unwrap();
            value["actor"] = actor.into();
            serde_json::from_value::<RunView>(value).unwrap()
        };
        assert!(worth_notifying(&view(Some(Conclusion::Failure), "agent:x")));
        assert!(worth_notifying(&view(Some(Conclusion::Success), "human")));
        assert!(!worth_notifying(&view(
            Some(Conclusion::Success),
            "agent:x"
        )));
        assert!(!worth_notifying(&view(None::<Conclusion>, "human")));
    }
}
