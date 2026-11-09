use std::time::Instant;

use winisland_platform::{
    InputState, Key, MouseButton, MouseWheelDelta, PlatformEvent, TouchPhase,
};
use winisland_plugin_api::*;
use winisland_plugin_host::extensions::{Event, HostRequest};

use super::App;
use crate::ui::expanded::pager::ExpandedPage;

#[derive(Default)]
pub(super) struct PluginUiState {
    modifiers: u32,
    pub(super) touch_id: Option<u64>,
    pub(super) pressed_buttons: u32,
    pub(super) media_selection: Option<(PluginToken, u64)>,
    pub(super) media_owner: Option<PluginToken>,
    pages: Vec<ExpandedPage>,
    commands: Vec<winisland_platform::PluginCommand>,
    tray_ready: bool,
}

impl App {
    pub(super) fn update_plugin_services(&mut self) {
        let Some(host) = self.plugin_host.clone() else {
            return;
        };
        let extensions = &host.runtime().extensions;
        self.update_plugin_media();
        let commands = extensions.commands();
        let bindings: Vec<_> = commands
            .iter()
            .map(|command| winisland_platform::PluginCommand {
                id: command.resource,
                title: command.title.clone(),
                enabled: command.flags & COMMAND_ENABLED != 0,
                menu: command.flags & COMMAND_TRAY != 0,
                hotkey: char::from_u32(command.key)
                    .filter(|key| key.is_ascii_alphanumeric())
                    .map(|key| winisland_platform::Hotkey {
                        ctrl: command.modifiers & MOD_CONTROL != 0,
                        alt: command.modifiers & MOD_ALT != 0,
                        shift: command.modifiers & MOD_SHIFT != 0,
                        win: command.modifiers & MOD_SUPER != 0,
                        key,
                    }),
            })
            .collect();
        let mut bindings = bindings;
        bindings.extend(self.chat_command());
        if bindings != self.plugin_ui.commands || self.tray_installed != self.plugin_ui.tray_ready {
            for (id, error) in crate::platform::shell().set_plugin_commands(&bindings) {
                log::warn!("Plugin command {id}: {error}");
                if let Ok(token) = host
                    .runtime()
                    .resources
                    .owner(winisland_plugin_host::resources::ResourceKind::Command, id)
                {
                    extensions.complete(token, id, PluginStatus::IoError);
                }
            }
            self.plugin_ui.commands = bindings;
            self.plugin_ui.tray_ready = self.tray_installed;
        }
        for id in crate::platform::shell().poll_plugin_commands() {
            if id == super::lingosoul::CHAT_COMMAND {
                self.open_chat_panel();
                continue;
            }
            if let Some(command) = commands.iter().find(|command| command.resource == id) {
                let _ = extensions.invoke(&command.id, &[]);
            }
        }
        for request in extensions.drain_requests() {
            if !host
                .runtime()
                .registry
                .get(request.token)
                .is_ok_and(|plugin| !plugin.stopping)
            {
                continue;
            }
            let status = match request.action {
                HostRequest::ShowPage(id) => {
                    if self.expanded_pages().contains(&ExpandedPage::Plugin(id)) {
                        self.expanded = true;
                        self.current_page = ExpandedPage::Plugin(id);
                        self.idle_timer = Instant::now();
                        PluginStatus::Ok
                    } else {
                        PluginStatus::StaleHandle
                    }
                }
                HostRequest::Toast { title, body } => {
                    if self.fullscreen_hide_active() {
                        // A plugin notification is the same surface as any other toast: it stays
                        // quiet while the island is hidden for a fullscreen session.
                        log::info!("Plugin toast suppressed while hidden for fullscreen");
                    } else {
                        Self::show_toast(&title, &body);
                    }
                    PluginStatus::Ok
                }
                HostRequest::Command { name, .. } => match name.as_str() {
                    "winisland.expand" => {
                        self.expanded = true;
                        self.idle_timer = Instant::now();
                        PluginStatus::Ok
                    }
                    "winisland.collapse" => {
                        self.expanded = false;
                        self.reset_page();
                        PluginStatus::Ok
                    }
                    "winisland.settings" => {
                        self.open_settings();
                        PluginStatus::Ok
                    }
                    "winisland.media.toggle"
                    | "winisland.media.next"
                    | "winisland.media.previous" => {
                        let command = match name.as_str() {
                            "winisland.media.next" => 3,
                            "winisland.media.previous" => 2,
                            _ => 1,
                        };
                        self.dispatch_media_command(command, 0);
                        PluginStatus::Ok
                    }
                    _ => PluginStatus::InvalidArgument,
                },
                HostRequest::Media {
                    session,
                    command,
                    position_ms,
                } => {
                    self.request_plugin_media(
                        request.token,
                        request.sequence,
                        session,
                        command,
                        position_ms,
                    );
                    continue;
                }
            };
            extensions.complete(request.token, request.sequence, status);
        }
        let (alert_revision, alert) = host.alert_snapshot();
        if alert_revision != self.plugin_alert_revision {
            self.plugin_alert_revision = alert_revision;
            match alert {
                Some(alert) => {
                    if self.fullscreen_hide_active() {
                        // The island keeps out of the way in fullscreen, so the alert is dropped
                        // rather than turned into a system toast the setting asked us to avoid.
                        log::info!("Plugin alert suppressed while hidden for fullscreen");
                        host.dismiss_alert();
                    } else if !self.visible {
                        Self::show_toast(&alert.title, &alert.body);
                        host.dismiss_alert();
                    } else {
                        self.compact_overlay.show_alert(alert.title, &alert.icon);
                        self.expanded = false;
                        self.reset_page();
                        if self.is_hidden() {
                            self.reveal_island();
                        }
                    }
                    self.idle_timer = Instant::now();
                }
                None => {
                    self.compact_overlay.dismiss_alert();
                }
            }
        }
        let pages = self.expanded_pages();
        if pages != self.plugin_ui.pages {
            if !pages.contains(&self.current_page) {
                self.reset_page();
            }
            self.snap_to_current_page();
            self.plugin_ui.pages = pages;
        }
        extensions.set_island(IslandStateV2 {
            struct_size: std::mem::size_of::<IslandStateV2>() as u32,
            expanded: u8::from(self.expanded),
            visible: u8::from(self.visible && !self.is_hidden()),
            light_theme: u8::from(self.is_light_theme),
            page: match self.current_page {
                ExpandedPage::Music => 1,
                ExpandedPage::Widgets => 2,
                ExpandedPage::Plugin(id) => {
                    if host.surface_id(
                        crate::plugin::calendar::ID,
                        crate::plugin::calendar::PAGE_KEY,
                    ) == Some(id)
                    {
                        3
                    } else {
                        id
                    }
                }
            },
            width: self.springs.w.value,
            height: self.springs.h.value,
            scale: if self.expanded {
                self.config.expanded_scale
            } else {
                self.config.compact_scale
            },
            ..Default::default()
        });
        if !self.visible || self.is_hidden() {
            extensions.clear_presentations();
        }
        if let Some(window) = self.window {
            let input = extensions.input_state();
            crate::platform::window().set_plugin_input(window.id(), input.captured, input.keyboard);
            if extensions.take_changed() {
                self.next_frame_deadline = Instant::now();
                let size = self.required_window_size();
                if window.inner_size() != size {
                    let _ = window.request_inner_size(size);
                }
                window.request_redraw();
            }
        }
    }

    pub(super) fn route_plugin_input(&mut self, event: &PlatformEvent) -> bool {
        let Some(host) = self.plugin_host.clone() else {
            return false;
        };
        if let PlatformEvent::ModifiersChanged { modifiers, .. } = event {
            self.plugin_ui.modifiers = *modifiers;
            return false;
        }
        let (px, py) = crate::utils::mouse::get_global_cursor_pos();
        let mut input = Event {
            x: (px - self.geom.win_x) as f32,
            y: (py - self.geom.win_y) as f32,
            modifiers: self.plugin_ui.modifiers,
            ..Default::default()
        };
        let mut key_text = None;
        let mut owned_touch = false;
        let mut owned_button = false;
        match event {
            PlatformEvent::CursorMoved { position, .. } => {
                input.detail = INPUT_MOVE;
                input.x = position.x as f32;
                input.y = position.y as f32;
            }
            PlatformEvent::CursorLeft { .. } => input.detail = INPUT_LEAVE,
            PlatformEvent::MouseInput { state, button, .. } => {
                if self.touch_id.is_some()
                    || self
                        .last_touch_at
                        .is_some_and(|at| at.elapsed().as_millis() < 250)
                {
                    return false;
                }
                input.detail = if *state == InputState::Pressed {
                    INPUT_DOWN
                } else {
                    INPUT_UP
                };
                input.code = match button {
                    MouseButton::Left => 1,
                    MouseButton::Right => 2,
                    MouseButton::Other => 3,
                };
                owned_button = self.plugin_ui.pressed_buttons & (1 << input.code) != 0;
            }
            PlatformEvent::MouseWheel { delta, .. } => {
                input.detail = INPUT_WHEEL;
                match delta {
                    MouseWheelDelta::Lines { x, y } => {
                        input.delta_x = *x;
                        input.delta_y = *y;
                        input.code = 0;
                    }
                    MouseWheelDelta::Pixels { x, y } => {
                        input.delta_x = *x as f32;
                        input.delta_y = *y as f32;
                        input.code = 1;
                    }
                }
            }
            PlatformEvent::Focused { focused: false, .. } => {
                input.detail = INPUT_BLUR;
                self.plugin_ui.modifiers = 0;
                self.plugin_ui.pressed_buttons = 0;
            }
            PlatformEvent::KeyInput {
                key,
                state,
                text,
                repeat,
                ..
            } => {
                input.detail = if *state == InputState::Pressed {
                    INPUT_KEY_DOWN
                } else {
                    INPUT_KEY_UP
                };
                input.code = match key {
                    Key::Backspace => KEY_BACKSPACE,
                    Key::Enter => KEY_ENTER,
                    Key::Escape => KEY_ESCAPE,
                    Key::ArrowLeft => KEY_LEFT,
                    Key::ArrowRight => KEY_RIGHT,
                    Key::ArrowUp => KEY_UP,
                    Key::ArrowDown => KEY_DOWN,
                    Key::Tab => KEY_TAB,
                    Key::Delete => KEY_DELETE,
                    Key::Character(value) => {
                        input.data = value.as_bytes().to_vec();
                        value.chars().next().map_or(0, |c| c as u32)
                    }
                    _ => 0,
                };
                input.delta_x = f32::from(*repeat);
                if *state == InputState::Pressed
                    && let Some(text) = text
                    && !text.chars().any(char::is_control)
                {
                    key_text = Some(text.as_bytes().to_vec());
                }
            }
            PlatformEvent::ImeCommit { text, .. } => {
                input.detail = INPUT_TEXT;
                input.data = text.as_bytes().to_vec();
            }
            PlatformEvent::ImeComposition { text, cursor, .. } => {
                input.detail = INPUT_COMPOSITION;
                input.data = text.as_bytes().to_vec();
                input.delta_x = cursor.map_or(-1.0, |c| c.0 as f32);
                input.delta_y = cursor.map_or(-1.0, |c| c.1 as f32);
            }
            PlatformEvent::DroppedFile { path, .. } => {
                input.detail = INPUT_DROP;
                input.data = path.to_string_lossy().as_bytes().to_vec();
            }
            PlatformEvent::Touch {
                phase,
                position,
                touch_id,
                ..
            } => {
                if let Some(active) = self.plugin_ui.touch_id {
                    if active != *touch_id {
                        return true;
                    }
                    owned_touch = true;
                } else if self.touch_id.is_some() || *phase != TouchPhase::Started {
                    return false;
                }
                self.last_touch_at = Some(Instant::now());
                self.touch_pos = *position;
                input.detail = match phase {
                    TouchPhase::Started => INPUT_DOWN,
                    TouchPhase::Moved => INPUT_MOVE,
                    TouchPhase::Ended => INPUT_UP,
                    TouchPhase::Cancelled => INPUT_CANCEL,
                };
                input.x = position.x as f32;
                input.y = position.y as f32;
                input.code = 1;
            }
            _ => return false,
        }
        if !self.visible
            || self.is_hidden()
            || (self.fullscreen_hide_active() && !self.hide.overlay_reveal)
        {
            input.detail = INPUT_CANCEL;
            key_text = None;
        }
        let detail = input.detail;
        let button = input.code;
        let result = host.runtime().extensions.route_input(input);
        if matches!(event, PlatformEvent::MouseInput { .. }) {
            if detail == INPUT_DOWN && result.handled {
                self.plugin_ui.pressed_buttons |= 1 << button;
            } else if detail == INPUT_UP {
                self.plugin_ui.pressed_buttons &= !(1 << button);
            }
        }
        if let Some(data) = key_text {
            host.runtime().extensions.route_input(Event {
                detail: INPUT_TEXT,
                data,
                modifiers: self.plugin_ui.modifiers,
                ..Default::default()
            });
        }
        if let Some(window) = self.window {
            crate::platform::window().set_plugin_input(
                window.id(),
                result.captured,
                result.keyboard,
            );
            if result.handled {
                self.idle_timer = Instant::now();
                window.request_redraw();
            }
        }
        if (result.handled || owned_touch)
            && let PlatformEvent::Touch {
                phase, touch_id, ..
            } = event
        {
            if *phase == TouchPhase::Started {
                self.touch_id = Some(*touch_id);
                self.plugin_ui.touch_id = Some(*touch_id);
            }
            if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.touch_id = None;
                self.plugin_ui.touch_id = None;
            }
        }
        if matches!(
            event,
            PlatformEvent::MouseInput {
                state: InputState::Released,
                ..
            }
        ) {
            owned_button
        } else {
            result.handled || owned_touch
        }
    }
}
