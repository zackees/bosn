//! What each of the widget's three windows looks like when it opens.
//!
//! Pure data: no window is created here. Each window is sized at open and
//! never resized, because a resize after the first frame is not applied on
//! KDE Plasma Wayland (zackees/kernal-api#390).

use kernal_api::webview::{
    BestEffort, WebviewWindowOptions, WebviewWindowSupport, WindowOptionsError,
};

/// The application id every widget window carries: the Wayland
/// `xdg_toplevel` app id a KWin window rule matches (`docs/ci.md`).
pub const APP_ID: &str = "dev.bosn.widget";

/// Where the bubble asks to sit on displays that honour a requested
/// position (X11, Windows, macOS): a fixed logical offset from the top-left
/// of the virtual desktop, below a macOS menu bar. The facade cannot report
/// the work area yet, so the bottom-right corner is unknowable
/// (zackees/kernal-api#393). Wayland ignores it; a KWin rule places it.
pub const BUBBLE_FALLBACK_POSITION: (i32, i32) = (24, 64);

/// One of the widget's windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// The always-present status bubble.
    Bubble,
    /// The panel the bubble toggles.
    Panel,
    /// The one full-view dashboard window.
    Full,
}

impl Window {
    /// The window title. Every window shares the app id, so the KWin rule
    /// tells the bubble apart by its title (`docs/ci.md`).
    pub const fn title(self) -> &'static str {
        match self {
            Self::Bubble => "bosn bubble",
            Self::Panel => "bosn panel",
            Self::Full => "bosn",
        }
    }

    /// Logical client-area size, fixed at open.
    pub const fn size(self) -> (u32, u32) {
        match self {
            // The ring is 52 px plus a 4 px focus outline on each side.
            Self::Bubble => (72, 72),
            Self::Panel => (420, 640),
            Self::Full => (1280, 800),
        }
    }

    /// The daemon page the window opens on, when it has a fixed one (the full
    /// view opens wherever the command points it).
    pub const fn page(self) -> Option<&'static str> {
        match self {
            Self::Bubble => Some("/widget/bubble"),
            Self::Panel => Some("/widget/panel"),
            Self::Full => None,
        }
    }

    /// The validated presentation for this window on a display with
    /// `support`.
    pub fn options(
        self,
        support: WebviewWindowSupport,
    ) -> Result<WebviewWindowOptions, WindowOptionsError> {
        let (width, height) = self.size();
        let options = WebviewWindowOptions::new(self.title(), width, height)?;
        match self {
            Self::Bubble => {
                // Keep-above and taskbar exclusion are requests even where the
                // display ignores them (Wayland): the KWin rule supplies them
                // there, and asking costs nothing.
                let bubble = options
                    .decorations(false)
                    .transparent(true)
                    .always_on_top(true)
                    .skip_taskbar(true);
                match support.position {
                    BestEffort::Requested => {
                        let (x, y) = BUBBLE_FALLBACK_POSITION;
                        bubble.initial_position(x, y)
                    }
                    BestEffort::Unsupported => Ok(bubble),
                }
            }
            Self::Panel | Self::Full => Ok(options),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn support(outcome: BestEffort) -> WebviewWindowSupport {
        WebviewWindowSupport {
            position: outcome,
            keep_above: outcome,
            skip_taskbar: outcome,
        }
    }

    #[test]
    fn the_app_id_is_reverse_dns() {
        assert_eq!(APP_ID, "dev.bosn.widget");
    }

    #[test]
    fn every_window_has_its_own_title_for_compositor_rules() {
        let titles = [Window::Bubble, Window::Panel, Window::Full].map(Window::title);
        assert_eq!(titles, ["bosn bubble", "bosn panel", "bosn"]);
    }

    #[test]
    fn the_bubble_is_an_undecorated_transparent_keep_above_tool_window() {
        for outcome in [BestEffort::Requested, BestEffort::Unsupported] {
            let bubble = Window::Bubble.options(support(outcome)).unwrap();
            assert!(!bubble.has_decorations());
            assert!(bubble.is_transparent());
            assert!(bubble.is_always_on_top());
            assert!(bubble.skips_taskbar());
            assert_eq!(bubble.logical_size(), (72, 72));
            assert_eq!(bubble.title(), "bosn bubble");
        }
    }

    #[test]
    fn the_bubble_asks_for_a_position_only_where_the_display_honours_one() {
        let requested = Window::Bubble
            .options(support(BestEffort::Requested))
            .unwrap();
        assert_eq!(requested.logical_position(), Some(BUBBLE_FALLBACK_POSITION));
        let wayland = Window::Bubble
            .options(support(BestEffort::Unsupported))
            .unwrap();
        assert_eq!(wayland.logical_position(), None);
    }

    #[test]
    fn the_panel_and_full_view_are_ordinary_decorated_windows() {
        for (window, title, size) in [
            (Window::Panel, "bosn panel", (420, 640)),
            (Window::Full, "bosn", (1280, 800)),
        ] {
            let options = window.options(support(BestEffort::Requested)).unwrap();
            assert!(options.has_decorations());
            assert!(!options.is_transparent());
            assert!(!options.is_always_on_top());
            assert!(!options.skips_taskbar());
            assert_eq!(options.logical_position(), None);
            assert_eq!(options.title(), title);
            assert_eq!(options.logical_size(), size);
        }
    }

    #[test]
    fn only_the_bubble_and_panel_have_fixed_pages() {
        assert_eq!(Window::Bubble.page(), Some("/widget/bubble"));
        assert_eq!(Window::Panel.page(), Some("/widget/panel"));
        assert_eq!(Window::Full.page(), None);
    }
}
