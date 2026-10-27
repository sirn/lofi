use super::theme::ThemeEvent;
use super::{tty_events, App};

impl App {
    /// Keep cache invalidation and terminal effects out of the theme state.
    pub(super) fn update_theme(&mut self, event: ThemeEvent) -> bool {
        let queries = self.theme_manager.update(event);
        tty_events::apply_theme_queries(queries);
        let next = self.theme_manager.theme();
        let changed = self.theme != next;
        if changed {
            self.theme = next;
            self.layout.frozen_render.clear();
        }
        self.debug_sample(&format!("theme:{event:?}"));
        changed
    }
}
