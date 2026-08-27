//! Translate winit events into an accumulated `egui::RawInput`.
//!
//! Events are collected between frames into a RawInput which render_frame
//! hands to `Context::begin_frame`. Pushing directly into
//! `ctx.input_mut()` does NOT work across frames — begin_frame replaces
//! the InputState, silently dropping anything we injected there. That was
//! the reason buttons never received clicks.

use egui::{Event, Key, MouseWheelUnit, Pos2, RawInput, Vec2};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{KeyCode, PhysicalKey};

pub struct RouterState {
    /// Physical-pixel cursor position (used for viewer hit-testing).
    pub cursor_pos: Pos2,
    /// Scale factor used to convert physical px → egui logical points.
    pub pixels_per_point: f32,
    /// PERSISTENT modifier state — must survive `take_pending`, which
    /// drains the event batch (and its modifiers) every frame. Without
    /// this, Ctrl held across a frame boundary reads as released.
    pub modifiers: egui::Modifiers,
    #[allow(dead_code)]
    pub last_click_pos: Option<Pos2>,
    #[allow(dead_code)]
    pub last_click_time: std::time::Instant,
    /// Accumulated egui input, consumed once per frame by begin_frame.
    pub pending: RawInput,
}

impl RouterState {
    pub fn new() -> Self {
        Self {
            cursor_pos: Pos2::ZERO,
            pixels_per_point: 1.0,
            modifiers: egui::Modifiers::default(),
            last_click_pos: None,
            last_click_time: std::time::Instant::now(),
            pending: RawInput::default(),
        }
    }

    /// Take the accumulated input for this frame (leaves empty behind).
    pub fn take_pending(&mut self) -> RawInput {
        std::mem::take(&mut self.pending)
    }

    fn logical_pos(&self) -> Pos2 {
        let ppp = self.pixels_per_point.max(0.1);
        Pos2::new(self.cursor_pos.x / ppp, self.cursor_pos.y / ppp)
    }
}

pub fn forward_to_egui(state: &mut RouterState, event: &WindowEvent) {
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            // Keep the physical position for viewer hit-testing, but feed
            // egui logical points (physical / pixels_per_point) — egui's
            // screen_rect is in logical units.
            state.cursor_pos = Pos2::new(position.x as f32, position.y as f32);
            state.pending.events.push(Event::PointerMoved(state.logical_pos()));
        }
        WindowEvent::CursorLeft { .. } => {
            state.pending.events.push(Event::PointerGone);
        }
        WindowEvent::MouseInput { state: button_state, button, .. } => {
            let btn = match button {
                MouseButton::Left => Some(egui::PointerButton::Primary),
                MouseButton::Right => Some(egui::PointerButton::Secondary),
                MouseButton::Middle => Some(egui::PointerButton::Middle),
                _ => None,
            };
            if let Some(btn) = btn {
                let pressed = matches!(button_state, ElementState::Pressed);
                let pos = state.logical_pos();
                state.pending.events.push(Event::PointerButton {
                    pos,
                    button: btn,
                    pressed,
                    modifiers: state.pending.modifiers,
                });
            }
        }
        WindowEvent::MouseWheel { delta: scroll_delta, .. } => {
            let (x, y) = match scroll_delta {
                MouseScrollDelta::LineDelta(x, y) => (*x * 24.0, *y * 24.0),
                MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 100.0, p.y as f32 / 100.0),
            };
            state.pending.events.push(Event::MouseWheel {
                unit: MouseWheelUnit::Point,
                delta: Vec2::new(x, y),
                modifiers: state.pending.modifiers,
            });
        }
        WindowEvent::KeyboardInput { event, .. } => {
            forward_key_event(state, event);
        }
        WindowEvent::ModifiersChanged(mods) => {
            let m = mods.state();
            let em = egui::Modifiers {
                alt: m.alt_key(),
                ctrl: m.control_key(),
                shift: m.shift_key(),
                mac_cmd: false,
                command: m.control_key(),
            };
            // Persistent copy (read by the app between frames) + batch copy
            // (consumed by egui's begin_frame).
            state.modifiers = em;
            state.pending.modifiers = em;
        }
        WindowEvent::Focused(focused) => {
            if !focused {
                // Drop stale modifier/pointer state when losing focus.
                state.modifiers = egui::Modifiers::default();
                state.pending.modifiers = egui::Modifiers::default();
            }
        }
        _ => {}
    }
}

pub fn forward_key_event(state: &mut RouterState, event: &KeyEvent) {
    if let Some(text) = &event.text {
        if !text.is_empty() {
            state.pending.events.push(Event::Text(text.to_string()));
        }
    }
    if let PhysicalKey::Code(code) = event.physical_key {
        if let Some(key) = map_key(code) {
            let pressed = matches!(event.state, ElementState::Pressed);
            state.pending.events.push(Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: event.repeat,
                modifiers: state.pending.modifiers,
            });
        }
    }
}

fn map_key(code: KeyCode) -> Option<Key> {
    Some(match code {
        KeyCode::ArrowLeft => Key::ArrowLeft,
        KeyCode::ArrowRight => Key::ArrowRight,
        KeyCode::ArrowUp => Key::ArrowUp,
        KeyCode::ArrowDown => Key::ArrowDown,
        KeyCode::Enter => Key::Enter,
        KeyCode::Escape => Key::Escape,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Space => Key::Space,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        _ => return None,
    })
}
