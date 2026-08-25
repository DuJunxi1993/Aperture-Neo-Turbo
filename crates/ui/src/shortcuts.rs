//! Global keyboard shortcuts

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    NextImage,
    PreviousImage,
    ZoomIn,
    ZoomOut,
    FitToScreen,
    ActualSize,
    Fullscreen,
    ToggleThumbPanel,
    ToggleTreePanel,
    ShowSettings,
    DeleteCurrent,
}

// Virtual key codes (subset of Win32 VK_* constants)
const VK_RIGHT: u32 = 0x27;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_DOWN: u32 = 0x28;
const VK_F: u32 = 0x46;
const VK_F11: u32 = 0x7A;
const VK_T: u32 = 0x54;
const VK_E: u32 = 0x45;
const VK_OEM_COMMA: u32 = 0xBC;
const VK_DELETE: u32 = 0x2E;
const VK_OEM_PLUS: u32 = 0xBB;
const VK_OEM_MINUS: u32 = 0xBD;
const VK_CONTROL: u32 = 0x11;

pub struct ShortcutManager {
    pub actions: Vec<(Action, Vec<u32>)>,
}

impl ShortcutManager {
    pub fn new() -> Self {
        let actions = vec![
            (Action::NextImage, vec![VK_RIGHT]),
            (Action::PreviousImage, vec![VK_LEFT]),
            (Action::ZoomIn, vec![VK_OEM_PLUS]),
            (Action::ZoomOut, vec![VK_OEM_MINUS]),
            (Action::FitToScreen, vec![VK_F]),
            (Action::ActualSize, vec![VK_F, VK_CONTROL]),
            (Action::Fullscreen, vec![VK_F11]),
            (Action::ToggleThumbPanel, vec![VK_T, VK_CONTROL]),
            (Action::ToggleTreePanel, vec![VK_E, VK_CONTROL]),
            (Action::ShowSettings, vec![VK_OEM_COMMA, VK_CONTROL]),
            (Action::DeleteCurrent, vec![VK_DELETE]),
        ];
        Self { actions }
    }

    pub fn dispatch(&self, vk_code: u32, ctrl: bool) -> Option<Action> {
        for (action, codes) in &self.actions {
            let needs_ctrl = codes.len() > 1 && codes.last().copied() == Some(VK_CONTROL);
            if needs_ctrl != ctrl { continue; }
            let main_key = codes[0];
            if main_key == vk_code {
                return Some(*action);
            }
        }
        None
    }
}