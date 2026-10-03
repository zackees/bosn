//! The widget's window state machine: which windows exist, which are
//! visible, and the native steps a daemon command turns into.
//!
//! Pure: [`Layout::plan`] decides and [`Layout::record`] remembers what took
//! effect, so the controller only executes steps. Windows are opened once and
//! then reused: the panel toggles with hide/show, the bubble is re-shown, and
//! the full view navigates in place, so the window count stays constant.

use bosn_service::ci::widget::WidgetCommand;

use crate::windows::Window;

/// What one window is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presence {
    /// No live window (never opened, or closed by the user or the system).
    #[default]
    Absent,
    /// Open and shown.
    Shown,
    /// Open but hidden; its page keeps running.
    Hidden,
}

/// One native action, in the order a plan lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Open the window on a daemon path (a fresh single-use grant).
    Open {
        window: Window,
        path: String,
    },
    Show(Window),
    Hide(Window),
    Focus(Window),
    /// Point the existing full view at a daemon path.
    Navigate {
        path: String,
    },
    /// Hand an allowlisted URL to the OS opener.
    External {
        url: String,
    },
    /// The deliberate quit: dismiss for this session and exit the process
    /// (which closes every window, so no window step precedes it).
    Quit,
}

impl Step {
    /// The window the step acts on, if any.
    pub const fn window(&self) -> Option<Window> {
        match self {
            Self::Open { window, .. } => Some(*window),
            Self::Show(window) | Self::Hide(window) | Self::Focus(window) => Some(*window),
            Self::Navigate { .. } => Some(Window::Full),
            Self::External { .. } | Self::Quit => None,
        }
    }
}

/// The presence of each window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    bubble: Presence,
    panel: Presence,
    full: Presence,
}

impl Layout {
    #[cfg(test)]
    pub fn presence(&self, window: Window) -> Presence {
        match window {
            Window::Bubble => self.bubble,
            Window::Panel => self.panel,
            Window::Full => self.full,
        }
    }

    fn slot(&mut self, window: Window) -> &mut Presence {
        match window {
            Window::Bubble => &mut self.bubble,
            Window::Panel => &mut self.panel,
            Window::Full => &mut self.full,
        }
    }

    /// The window is gone (closed, or a step on it failed): the next command
    /// that needs it opens a new one.
    pub fn lost(&mut self, window: Window) {
        *self.slot(window) = Presence::Absent;
    }

    /// Reconcile status presence without taking focus during host loss/recovery.
    pub fn status_steps(&self, hosted: bool) -> Vec<Step> {
        match (hosted, self.bubble) {
            (true, Presence::Shown) => vec![Step::Hide(Window::Bubble)],
            (false, Presence::Absent) => vec![open_page(Window::Bubble)],
            (false, Presence::Hidden) => vec![Step::Show(Window::Bubble)],
            _ => vec![],
        }
    }

    /// An explicit Show reveals details without toggling an already open panel.
    pub fn show_panel_steps(&self) -> Vec<Step> {
        match self.panel {
            Presence::Absent => vec![open_page(Window::Panel)],
            Presence::Hidden | Presence::Shown => {
                vec![Step::Show(Window::Panel), Step::Focus(Window::Panel)]
            }
        }
    }

    /// The steps that carry out `command` from the current state.
    pub fn plan(&self, command: WidgetCommand) -> Vec<Step> {
        match command {
            WidgetCommand::Toggle => match self.panel {
                Presence::Shown => vec![Step::Hide(Window::Panel)],
                Presence::Hidden => vec![Step::Show(Window::Panel), Step::Focus(Window::Panel)],
                Presence::Absent => vec![open_page(Window::Panel)],
            },
            WidgetCommand::Show => match self.bubble {
                Presence::Absent => vec![open_page(Window::Bubble)],
                Presence::Shown | Presence::Hidden => {
                    vec![Step::Show(Window::Bubble), Step::Focus(Window::Bubble)]
                }
            },
            WidgetCommand::Open { path } => match self.full {
                Presence::Absent => vec![Step::Open {
                    window: Window::Full,
                    path,
                }],
                Presence::Shown | Presence::Hidden => vec![
                    Step::Navigate { path },
                    Step::Show(Window::Full),
                    Step::Focus(Window::Full),
                ],
            },
            WidgetCommand::OpenExternal { url } => vec![Step::External { url }],
            WidgetCommand::Quit => vec![Step::Quit],
        }
    }

    /// Remember a step that took effect.
    pub fn record(&mut self, step: &Step) {
        match step {
            Step::Open { window, .. } | Step::Show(window) => *self.slot(*window) = Presence::Shown,
            Step::Hide(window) => *self.slot(*window) = Presence::Hidden,
            Step::Focus(_) | Step::Navigate { .. } | Step::External { .. } | Step::Quit => {}
        }
    }
}

/// Open a window on its fixed page.
fn open_page(window: Window) -> Step {
    let path = window
        .page()
        .expect("the bubble and panel have fixed pages");
    Step::Open {
        window,
        path: path.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plan and record every step as if it succeeded; return the steps.
    fn apply(layout: &mut Layout, command: WidgetCommand) -> Vec<Step> {
        let steps = layout.plan(command);
        for step in &steps {
            layout.record(step);
        }
        steps
    }

    fn opens(steps: &[Step]) -> usize {
        steps
            .iter()
            .filter(|step| matches!(step, Step::Open { .. }))
            .count()
    }

    #[test]
    fn repeated_explicit_show_keeps_existing_details_visible() {
        let mut layout = Layout::default();
        for _ in 0..3 {
            for step in layout.show_panel_steps() {
                layout.record(&step);
            }
            assert_eq!(layout.presence(Window::Panel), Presence::Shown);
        }
    }

    #[test]
    fn host_loss_and_recovery_swap_status_presence_without_stealing_focus() {
        let mut layout = Layout::default();
        assert!(layout.status_steps(true).is_empty());
        let fallback = layout.status_steps(false);
        assert_eq!(opens(&fallback), 1);
        for step in fallback {
            layout.record(&step);
        }
        let hosted = layout.status_steps(true);
        assert_eq!(hosted, vec![Step::Hide(Window::Bubble)]);
        for step in hosted {
            layout.record(&step);
        }
        assert!(layout.status_steps(true).is_empty());
        assert_eq!(layout.status_steps(false), vec![Step::Show(Window::Bubble)]);
    }

    #[test]
    fn the_first_toggle_opens_the_panel_then_toggles_hide_and_show() {
        let mut layout = Layout::default();
        assert_eq!(
            apply(&mut layout, WidgetCommand::Toggle),
            vec![Step::Open {
                window: Window::Panel,
                path: "/widget/panel".into()
            }]
        );
        assert_eq!(layout.presence(Window::Panel), Presence::Shown);
        assert_eq!(
            apply(&mut layout, WidgetCommand::Toggle),
            vec![Step::Hide(Window::Panel)]
        );
        assert_eq!(layout.presence(Window::Panel), Presence::Hidden);
        assert_eq!(
            apply(&mut layout, WidgetCommand::Toggle),
            vec![Step::Show(Window::Panel), Step::Focus(Window::Panel)]
        );
        assert_eq!(layout.presence(Window::Panel), Presence::Shown);
    }

    #[test]
    fn a_hundred_toggles_open_the_panel_once() {
        let mut layout = Layout::default();
        let mut steps = Vec::new();
        for _ in 0..100 {
            steps.extend(apply(&mut layout, WidgetCommand::Toggle));
        }
        assert_eq!(opens(&steps), 1);
        // An even number of toggles after the opening one leaves it hidden.
        assert_eq!(layout.presence(Window::Panel), Presence::Hidden);
    }

    #[test]
    fn a_panel_closed_by_the_user_reopens_on_the_next_toggle() {
        let mut layout = Layout::default();
        apply(&mut layout, WidgetCommand::Toggle);
        layout.lost(Window::Panel);
        assert_eq!(opens(&apply(&mut layout, WidgetCommand::Toggle)), 1);
        assert_eq!(layout.presence(Window::Panel), Presence::Shown);
    }

    #[test]
    fn repeated_opens_reuse_one_full_view() {
        let mut layout = Layout::default();
        let first = apply(
            &mut layout,
            WidgetCommand::Open {
                path: "/ci/runs/a".into(),
            },
        );
        assert_eq!(
            first,
            vec![Step::Open {
                window: Window::Full,
                path: "/ci/runs/a".into()
            }]
        );
        for run in ["b", "c", "d"] {
            let path = format!("/ci/runs/{run}");
            assert_eq!(
                apply(&mut layout, WidgetCommand::Open { path: path.clone() }),
                vec![
                    Step::Navigate { path },
                    Step::Show(Window::Full),
                    Step::Focus(Window::Full),
                ]
            );
        }
        assert_eq!(layout.presence(Window::Full), Presence::Shown);
    }

    #[test]
    fn show_re_presents_the_existing_bubble() {
        let mut layout = Layout::default();
        assert_eq!(opens(&apply(&mut layout, WidgetCommand::Show)), 1);
        for _ in 0..3 {
            assert_eq!(
                apply(&mut layout, WidgetCommand::Show),
                vec![Step::Show(Window::Bubble), Step::Focus(Window::Bubble)]
            );
        }
    }

    #[test]
    fn quit_exits_from_any_state_without_touching_a_window() {
        let mut layout = Layout::default();
        assert_eq!(apply(&mut layout, WidgetCommand::Quit), vec![Step::Quit]);
        assert_eq!(layout, Layout::default());
        apply(&mut layout, WidgetCommand::Show);
        apply(&mut layout, WidgetCommand::Toggle);
        let open = layout;
        assert_eq!(apply(&mut layout, WidgetCommand::Quit), vec![Step::Quit]);
        assert_eq!(layout, open, "the exit closes the windows, not a step");
        assert_eq!(Step::Quit.window(), None);
    }

    #[test]
    fn external_links_touch_no_window() {
        let mut layout = Layout::default();
        let url = "https://github.com/zackees/bosn".to_string();
        assert_eq!(
            apply(
                &mut layout,
                WidgetCommand::OpenExternal { url: url.clone() }
            ),
            vec![Step::External { url }]
        );
        assert_eq!(layout, Layout::default());
    }
}
