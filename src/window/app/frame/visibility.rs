use std::time::Instant;

use crate::platform::WindowRef;
use crate::ui::compact::CompactOverlayState;
use crate::utils::mouse::{is_cursor_hidden, is_foreground_fullscreen};

use super::super::App;

impl App {
    pub(super) fn update_fullscreen_suppression(&mut self, window: &WindowRef, now: Instant) {
        let prev_fullscreen = self.is_fullscreen_suppressed;
        self.is_fullscreen_suppressed = is_foreground_fullscreen(
            self.geom.monitor_pos.0,
            self.geom.monitor_pos.1,
            self.geom.monitor_size.0,
            self.geom.monitor_size.1,
        );
        self.is_cursor_suppressed = is_cursor_hidden();
        if self.is_fullscreen_suppressed && !prev_fullscreen {
            self.fullscreen_hide_paused = false;
        }
        self.apply_fullscreen_hide(window, now);
        if self.is_fullscreen_suppressed != prev_fullscreen {
            log::info!(
                "Fullscreen state: {}",
                if self.is_fullscreen_suppressed {
                    "active"
                } else {
                    "normal"
                }
            );
        }
    }

    pub(super) fn poll_clipboard_link(&mut self, window: &WindowRef) {
        const MAX_CLIPBOARD_CHARS: usize = 2048;
        let sequence = crate::platform::shell().clipboard_sequence();
        if sequence == self.clipboard_sequence {
            return;
        }
        self.clipboard_sequence = sequence;
        if !self.config.clipboard_link_prompt
            || !self.visible
            || self.expanded
            || self.is_hidden()
            || self.fullscreen_hide_active()
        {
            return;
        }
        let Some(url) = crate::platform::shell()
            .clipboard_text(MAX_CLIPBOARD_CHARS + 1)
            .and_then(|text| crate::ui::compact::extract_link(&text))
        else {
            return;
        };
        self.compact_overlay.show_link(url);
        self.idle_timer = Instant::now();
        window.request_redraw();
    }

    pub(super) fn handle_hide_hotkey(&mut self, window: &WindowRef, now: Instant) {
        if crate::platform::shell()
            .poll_hotkey_presses()
            .is_multiple_of(2)
        {
            return;
        }
        if self.fullscreen_hide_active() {
            self.fullscreen_hide_paused = true;
            log::info!("Fullscreen auto-hide paused by hotkey");
            self.apply_fullscreen_hide(window, now);
        } else if self.fullscreen_hide_paused
            && self.config.fullscreen_auto_hide
            && self.is_fullscreen_suppressed
        {
            self.fullscreen_hide_paused = false;
            log::info!("Fullscreen auto-hide resumed by hotkey");
            self.apply_fullscreen_hide(window, now);
        } else if self.is_hidden() {
            self.reveal_island();
            log::info!("Island revealed by hotkey");
        } else if self.prepare_hide(window) {
            self.expanded = false;
            self.reset_page();
            self.hide.manual = true;
            self.hide.auto = false;
            self.hide.assistant_reveal = false;
            log::info!("Island hidden by hotkey");
        }
        window.request_redraw();
    }

    fn apply_fullscreen_hide(&mut self, window: &WindowRef, now: Instant) {
        let should_hide_for_fullscreen = self.fullscreen_hide_active();
        if should_hide_for_fullscreen == self.hide.fullscreen {
            return;
        }
        self.hide.fullscreen = should_hide_for_fullscreen;
        if should_hide_for_fullscreen {
            if self.hide.origin.is_some() || self.prepare_hide(window) {
                self.expanded = false;
                self.reset_page();
                self.hide.notification_reveal = false;
                self.hide.overlay_reveal = false;
                self.attention_pulse_started = Some(now);
            } else {
                self.hide.fullscreen = false;
            }
        } else {
            self.attention_pulse_started = None;
            self.idle_timer = Instant::now();
            if !self.is_hidden() {
                self.springs.hide.velocity = -0.65;
            }
        }
        window.request_redraw();
    }

    pub(super) fn update_compact_and_auto_hide(
        &mut self,
        window: &WindowRef,
        is_hovering_visible: bool,
    ) -> bool {
        let overlay_present = !self.expanded && !self.is_hidden();
        let fullscreen_hidden = self.fullscreen_hide_active();
        let volume_state = if overlay_present && (!fullscreen_hidden || self.hide.overlay_reveal) {
            CompactOverlayState::Present
        } else if fullscreen_hidden || (self.hide.auto && !self.hide.manual) {
            CompactOverlayState::Defer
        } else {
            CompactOverlayState::Discard
        };
        // A notification that arrives while the island hides itself for a fullscreen session is
        // dropped instead of deferred: deferring keeps it pending until the session ends, and it
        // then pops out as a stale alert when the island returns.
        let notification_state = if fullscreen_hidden {
            CompactOverlayState::Discard
        } else if overlay_present {
            CompactOverlayState::Present
        } else if !self.expanded && self.hide.has_hidden_reason() {
            CompactOverlayState::Defer
        } else {
            CompactOverlayState::Discard
        };
        let compact_update = self.compact_overlay.update(
            volume_state,
            notification_state,
            self.config.notification_display,
        );
        if compact_update.notification_received
            && !fullscreen_hidden
            && self.hide.has_hidden_reason()
            && !self.hide.notification_reveal
        {
            self.hide.notification_reveal = true;
            self.idle_timer = Instant::now();
            self.springs.hide.velocity = -0.65;
            self.compact_overlay.update(
                CompactOverlayState::Present,
                CompactOverlayState::Present,
                self.config.notification_display,
            );
            log::info!("Island temporarily revealed for notification");
        } else if (compact_update.volume_changed || compact_update.brightness_changed)
            && (fullscreen_hidden || (self.hide.auto && !self.hide.manual))
        {
            let reveal_started = if fullscreen_hidden {
                !self.hide.overlay_reveal
            } else {
                self.hide.auto
            };
            if fullscreen_hidden {
                self.hide.overlay_reveal = true;
            } else if self.hide.auto && !self.hide.manual {
                self.hide.auto = false;
            }
            self.idle_timer = Instant::now();
            if reveal_started {
                self.springs.hide.velocity = -0.65;
            }
            self.compact_overlay.update(
                CompactOverlayState::Present,
                CompactOverlayState::Present,
                self.config.notification_display,
            );
            log::info!("Island un-hidden (compact overlay event)");
        }
        if self.hide.notification_reveal && !self.compact_overlay.is_notification_visible() {
            self.hide.notification_reveal = false;
            if self.hide.has_hidden_reason() {
                self.prepare_hide(window);
            }
            window.request_redraw();
        }
        let compact_overlay_visible = self.compact_overlay.is_visible();
        if self.hide.overlay_reveal && !compact_overlay_visible {
            self.hide.overlay_reveal = false;
            if self.hide.has_hidden_reason() {
                self.prepare_hide(window);
            }
            window.request_redraw();
        }
        let is_idle = (!is_hovering_visible || self.components_hidden)
            && !self.expanded
            && !self.is_dragging
            && !compact_overlay_visible
            && !self.lingosoul.is_active()
            && (self.components_hidden || !self.activity_mgr.keeps_visible());
        if !self.config.auto_hide {
            let was_auto_hidden = self.hide.auto;
            self.hide.auto = false;
            self.idle_timer = Instant::now();
            if was_auto_hidden && !self.is_hidden() {
                self.springs.hide.velocity = -0.65;
            }
        } else if self.activity_mgr.keeps_visible()
            && !self.components_hidden
            && self.hide.auto
            && !self.hide.manual
        {
            self.hide.auto = false;
            self.idle_timer = Instant::now();
            if !self.is_hidden() {
                self.springs.hide.velocity = -0.65;
            }
            log::info!("Island un-hidden (active activity)");
        } else if !self.is_hidden() && is_idle {
            if self.idle_timer.elapsed().as_secs_f32() > self.config.auto_hide_delay
                && self.prepare_hide(window)
            {
                self.hide.auto = true;
                log::info!(
                    "Island auto-hidden (idle {:.1}s)",
                    self.config.auto_hide_delay
                );
            }
        } else if !self.is_hidden() && !is_idle {
            self.idle_timer = Instant::now();
        }
        compact_overlay_visible
    }
}
