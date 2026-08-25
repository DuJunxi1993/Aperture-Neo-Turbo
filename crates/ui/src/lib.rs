//! UI layer — egui-based chrome (title bar, toolbar, thumbnail grid, tree, settings)

pub mod chrome;
pub mod viewer_panel;
pub mod thumb_panel;
pub mod tree_panel;
pub mod settings;
pub mod shortcuts;

pub use chrome::{TitleBar, FloatingBar};
pub use viewer_panel::ViewerPanel;
pub use thumb_panel::ThumbPanel;
pub use tree_panel::TreePanel;
pub use settings::SettingsPanel;
pub use shortcuts::{ShortcutManager, Action};