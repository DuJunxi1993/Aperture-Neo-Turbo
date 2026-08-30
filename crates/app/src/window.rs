//! Main window — winit + wgpu + egui (chrome)
//!
//! Layout:
//! ┌─ Title bar (egui, 28 px) ────────────────────────────┐
//! ├────────────┬──────────────────────────────────────────┤
//! │ Tree       │                                          │
//! │ (egui,    │        Viewer (wgpu image quad)           │
//! │ 200 px)    │                                          │
//! │            │                                          │
//! ├────────────┴──────────────────────────────────────────┤
//! └─────────────────────────────────────────────────────────┘
//  Thumb panel sits on the right side (egui, 120 px).

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use parking_lot::Mutex;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_SETDESKWALLPAPER, SPIF_UPDATEINIFILE, SPIF_SENDCHANGE,
};

use aperture_core::{
    NavigationService, NavigationDirection,
    SettingsStore, ThumbCache, ThumbCacheConfig, ThemeSetting as Theme,
};
use aperture_gpu::{Direct2DViewer, DecodeCoordinator, SlideDir};
use crate::event_router::{self, RouterState};

/// Phase 16: custom winit event that pops a native right-click
/// TrackPopupMenu OUTSIDE the MouseInput handler. Popping a modal
/// TrackPopupMenu synchronously inside the winit window_event callback
/// Captured by the tree right-click handler when the user right-clicks
/// a folder in the tree; consumed when the egui context menu is shown
/// inside `build_egui_ui`. The egui closure can't open the menu itself
/// (would need `&mut self` while the frame holds it), so it writes this
/// intent and the post-frame loop opens the popup.
#[derive(Debug, Clone)]
pub struct TreeCtxIntent {
    pub pos_phys: (i32, i32),
    pub path: PathBuf,
    pub root_idx: usize,
}

/// An open egui-drawn right-click context menu.
enum CtxMenu {
    /// Right-click on the viewer image.
    Image { pos: egui::Pos2 },
    /// Right-click on a folder in the tree.
    Tree {
        pos: egui::Pos2,
        path: PathBuf,
        root_idx: usize,
        is_favorite: bool,
    },
}
const TREE_WIDTH: u32 = 240;
const THUMB_WIDTH: u32 = 220;
const TOOLBAR_HEIGHT: u32 = 48;
const STATUS_BAR_HEIGHT: u32 = 48;
/// Default initial size (logical). Picked to fit a 1280×800 logical screen
/// after subtracting chrome (toolbar 36 + status 24 + panel widths 240+220).
const DEFAULT_W: u32 = 1280;
const DEFAULT_H: u32 = 800;
const MIN_W: u32 = 480;
const MIN_H: u32 = 320;
// Frames to hold a retired (replaced) image texture before releasing its
// egui handle. With Fifo present + frame-latency-1, the GPU can be at most
// one frame behind the CPU; holding for a few frames plus the non-blocking
// `device.poll(Maintain::Poll)` reap each frame guarantees the texture is no
// longer sampled by any in-flight submit before it is dropped. Must be ≥ 2.
const RETIRE_FRAME_BUDGET: u64 = 4;
// Edge-drawer trigger/handle constants. The drawer opens when the pointer
// nears the handle (a thin vertical pill at the left edge); it stays open
// while the pointer is on the handle, the drawer, or a slim left-edge strip,
// and slides back after `TREE_LEAVE_MS` outside those. Handle is inset
// `HANDLE_X` from the window edge so it's not flush against it.
const TREE_LEAVE_MS: u64 = 1000;
const TREE_DRAWER_ANIM_MS: f32 = 220.0;
const HANDLE_X: f32 = 8.0;
const HANDLE_ZONE_PAD: f32 = 10.0;
// Drawer width bounds (logical px). The upper bound is deliberately loose —
// the drawer is an overlay (never takes layout space, so it can't crowd the
// viewer), and a wide row (long folder name) should show fully rather than
// truncate. `drawer_user_width` (user drag) clamps here.
const DRAWER_MIN_W: f32 = 170.0;
const DRAWER_MAX_W: f32 = 560.0;
// Handle contrast thresholds with hysteresis: luminance > 0.62 → dark
// handle (light image), < 0.38 → light handle (dark image), between → keep
// the previous choice (no flicker at a bright/dark boundary).
const HANDLE_LIGHT_THRESHOLD: f32 = 0.62;
const HANDLE_DARK_THRESHOLD: f32 = 0.38;
const HANDLE_DEBOUNCE_MS: u64 = 200;
/// Held-arrow-step interval (fixed-rate). Tuned near the decode ceiling so
/// a fast hold "walks" the gallery at a steady, rideable pace: each step
/// cuts instantly (pre-decode cache), no animation. Too slow feels laggy;
/// too fast skips the eye's ability to track. ~90ms ≈ 11 img/s.
const HOLD_NAV_INTERVAL: std::time::Duration = std::time::Duration::from_millis(90);
/// How long the key must be held before the fixed-rate walk engages. A
/// quick tap (< this) stays a single animated navigation; holding past it
/// enters fast direct-cut paging. Prevents misfiring the walk on a normal
/// tap or fighting with single-press navigation.
const HOLD_NAV_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(220);

#[allow(dead_code)]
enum UiAction {
    OpenFolder,
    Prev,
    Next,
    Fit,
    OneToOne,
    /// Phase 3: fit↔1:1 cycle-toggle button (replaces the two
    /// separate Fit / OneToOne actions on the bottom bar; the
    /// existing two are kept for keyboard shortcut compatibility
    /// — F still maps to Fit, Ctrl+0 to OneToOne, and the
    /// new FitOrOriginal covers the bottom-bar click).
    FitOrOriginal,
    /// Phase 3: rotate the current image by `delta` quarter-turns
    /// (positive = clockwise). The action is parameterised so the
    /// bottom bar's single ↻ button can be Ctrl+R for counter-
    /// clockwise without a second variant.
    RotateImage(i32),
    /// Phase 3: start / stop the 3-second-tick slide show. The
    /// bool is the new state (true = running). MainWindow
    /// owns the timer; the action is just a request to flip.
    ToggleSlideShow,
    ToggleFullscreen,
    ToggleTree,
    ToggleThumbs,
    /// User clicked a file in the thumbs panel.
    ThumbClicked(usize),
    /// User clicked a folder in the tree (navigate into it). The usize
    /// is the tree root the folder was clicked in (0=Favorites,
    /// 1=Recent, 2=This PC) — drives Ctrl+Arrow list selection.
    FolderChosen(PathBuf, usize),
    /// Toggle the shortcut help panel.
    ToggleShortcutHelp,
    /// Toggle favorite for the current folder.
    ToggleFavorite,
    /// Add a specific folder to favorites.
    AddFavorite(PathBuf),
    /// Remove a specific folder from favorites.
    RemoveFavorite(PathBuf),
    /// Remove a specific folder from the Recent list.
    RemoveRecent(PathBuf),
    /// Open a folder's location in Windows Explorer.
    RevealInExplorer(PathBuf),
    /// Navigate the in-app viewer into a folder (context-menu "browse").
    BrowseFolder(PathBuf),
    /// Expand the This PC tree and scroll to a folder.
    RevealInTree(PathBuf),
    /// Close the application (custom titlebar ✕).
    ExitApp,
    /// Minimize the window (custom titlebar —).
    MinimizeWindow,
    /// Toggle maximize (custom titlebar ❐ / double-click).
    ToggleMaximize,
    /// Switch between dark and light themes.
    ToggleTheme,
    /// Set current image as wallpaper.
    SetWallpaper,
    /// Open current image folder in Explorer.
    OpenInExplorer,
    /// Phase 8: copy the current image's path to the clipboard.
    /// Used by the image right-click context menu so the popup
    /// can emit an action without holding a `&mut self` borrow
    /// inside the egui closure.
    CopyPath,
    /// Copy a folder path to the clipboard (used by the tree header's
    /// double-click path label).
    CopyFolderPath(PathBuf),
    /// Settings dropdown: open/close the ⚙ settings menu.
    ToggleSettings,
    /// Settings dropdown: clear all recently-opened folders.
    SettingsClearRecent,
    /// Settings dropdown: clear all favorite folders.
    SettingsClearFavorites,
    /// Settings dropdown: enable/disable follow-system theme.
    SettingsThemeSystem,
    /// Settings dropdown: toggle showing the tree sidebar on launch.
    SettingsSidebarOnLaunch,
    /// Phase 8: open the print dialog for the current image.
    Print,
}

// Screen-space rect of the "?" status-bar button — the shortcuts popover
// anchors directly above it.
thread_local! {
    static HELP_ANCHOR: std::cell::Cell<egui::Rect> = const { std::cell::Cell::new(egui::Rect::NOTHING) };
    /// Rect of the title-bar ⚙ settings button, used to anchor the
    /// settings dropdown below it (right-aligned).
    static SETTINGS_ANCHOR: std::cell::Cell<egui::Rect> = const { std::cell::Cell::new(egui::Rect::NOTHING) };
    /// Sticky vertical scroll offset of the thumbnail ScrollArea
    /// (logical px). Shared across the static draw_thumbs_panel_static
    /// helper and the window_event wheel branch.
    static THUMB_SCROLL_Y: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    /// Last-known sticky scroll offset of the thumbs panel, used to
    /// prefetch thumbnails in the direction of scroll (below = larger
    /// indices, above = smaller) so a fast scroll doesn't flash placeholders.
    static THUMB_LAST_SCROLL_Y: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

/// What the process was launched with.
pub enum LaunchTarget {
    None,
    /// A folder of images.
    Folder(PathBuf),
    /// A single image file (default-viewer launch): window sized to the
    /// image, side panels hidden for an immersive start.
    SingleImage { path: PathBuf, width: u32, height: u32 },
}

/// Glyphs for the custom caption buttons (drawn with the painter, so they
/// never fall back to missing font glyphs).
#[derive(Clone, Copy)]
enum WindowGlyph {
    Minimize,
    Maximize,
    Close,
}

/// Phase 8: state carried by the tree right-click context menu.
/// `pos` is the cursor position in logical points (where the popup
/// anchors); `path`/`root_idx`/`depth` drive the items shown (Recent
/// gets "添加到收藏" / "从 Recent 移除", Favorites gets "取消收藏",
/// This PC gets the full set including "在目录树中定位").

/// UI theme — Dark (default) and Light, per the Aperture Neo design spec.
/// `System` is reserved for future follow-the-OS support.
/// Centralized color palette — every draw path reads from here so both
/// themes stay consistent.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub panel_bg: egui::Color32,
    pub text_primary: egui::Color32,
    pub text_secondary: egui::Color32,
    pub text_tertiary: egui::Color32,
    pub text_dim: egui::Color32,
    pub accent: egui::Color32,
    #[allow(dead_code)]
    pub accent_hover: egui::Color32,
    pub selection_text: egui::Color32,
    pub hover_fill: egui::Color32,
    pub button_fill: egui::Color32,
    pub button_hover: egui::Color32,
    pub card_stroke: egui::Color32,
    pub selected_card_fill: egui::Color32,
    pub selected_card_stroke: egui::Color32,
    pub thumb_placeholder: egui::Color32,
    pub key_hint: egui::Color32,
    pub help_desc: egui::Color32,
    /// egui wgpu-surface clear color.
    pub canvas_clear: (f64, f64, f64),
    /// D2D viewer letterbox color.
    pub d2d_clear: [f32; 3],
}

/// Resolve a theme into an effective Light/Dark (follows the OS preference
/// when `System`). Free fn (no `&self`) so the egui frame build can call it
/// while `self.egui_state` is mutably borrowed.
fn resolve_theme(theme: Theme) -> Theme {
    match theme {
        Theme::System => {
            if crate::theme::system_theme_is_light() {
                Theme::Light
            } else {
                Theme::Dark
            }
        }
        other => other,
    }
}

pub fn dark_palette() -> Palette { Palette {
    panel_bg: egui::Color32::from_rgb(15, 16, 17),
    text_primary: egui::Color32::from_rgb(247, 248, 248),
    text_secondary: egui::Color32::from_rgb(200, 205, 214),
    text_tertiary: egui::Color32::from_rgb(138, 143, 152),
    text_dim: egui::Color32::from_rgb(98, 102, 109),
    accent: egui::Color32::from_rgb(0x5e, 0x6a, 0xd2),
    accent_hover: egui::Color32::from_rgb(0x71, 0x70, 0xff),
    selection_text: egui::Color32::from_rgb(140, 148, 255),
    hover_fill: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14),
    button_fill: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14),
    button_hover: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 22),
    card_stroke: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 10),
    selected_card_fill: egui::Color32::from_rgba_unmultiplied(87, 93, 188, 38),
    selected_card_stroke: egui::Color32::from_rgb(95, 106, 210),
    thumb_placeholder: egui::Color32::from_rgb(28, 30, 36),
    key_hint: egui::Color32::from_rgb(100, 180, 255),
    help_desc: egui::Color32::from_rgb(200, 200, 210),
    canvas_clear: (0.059, 0.063, 0.067),
    d2d_clear: [0.059, 0.063, 0.067],
    }
}

pub fn light_palette() -> Palette { Palette {
    panel_bg: egui::Color32::from_rgb(246, 246, 246),
    text_primary: egui::Color32::from_rgb(26, 27, 30),
    text_secondary: egui::Color32::from_rgb(60, 63, 68),
    text_tertiary: egui::Color32::from_rgb(107, 112, 120),
    text_dim: egui::Color32::from_rgb(140, 145, 152),
    accent: egui::Color32::from_rgb(0x5e, 0x6a, 0xd2),
    accent_hover: egui::Color32::from_rgb(0x71, 0x70, 0xff),
    selection_text: egui::Color32::from_rgb(0x5e, 0x6a, 0xd2),
    hover_fill: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 10),
    button_fill: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 13),
    button_hover: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 22),
    card_stroke: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 18),
    selected_card_fill: egui::Color32::from_rgba_unmultiplied(94, 106, 210, 30),
    selected_card_stroke: egui::Color32::from_rgb(94, 106, 210),
    thumb_placeholder: egui::Color32::from_rgb(226, 228, 231),
    key_hint: egui::Color32::from_rgb(0x5e, 0x6a, 0xd2),
    help_desc: egui::Color32::from_rgb(60, 63, 68),
    canvas_clear: (0.953, 0.957, 0.961),
    d2d_clear: [0.953, 0.957, 0.961],
    }
}

/// Animated, user-resizable side-panel width with a content-driven minimum.
///
/// Replaces egui's built-in resizable SidePanel state: the visible width
/// eases toward the target (macOS-style sidebar collapse animation), the
/// user drags `user_width` directly, and `content_min` (measured from the
/// tree content each frame) forces the panel wider when deep folders are
/// expanded — and lets it shrink back when they collapse.
#[derive(Debug)]
struct PanelWidth {
    user_width: f32,
    anim: f32,
    content_min: f32,
}

impl PanelWidth {
    fn new(default_w: f32) -> Self {
        // anim starts at 0 so the panel animates open from 0 on launch
        // (rather than being at full width immediately) and closes back
        // to 0 smoothly. user_width is the drag-adjusted target.
        Self { user_width: default_w, anim: 0.0, content_min: 0.0 }
    }

    /// Advance the animation and return the width to draw with this frame.
    /// Both expand (target = user_width) and collapse (target = 0) ease
    /// exponentially (~120 ms settle) so the tree/thumb panels slide open
    /// and closed instead of snapping. Snap the LAST few percent so the
    /// fully-collapsed panel isn't left with a sub-pixel sliver.
    fn tick(&mut self, expanded: bool, dt: f32) -> f32 {
        let target = if expanded {
            self.user_width.max(self.content_min)
        } else {
            0.0
        };
        let k = (dt / 0.12).clamp(0.0, 1.0);
        self.anim += (target - self.anim) * k;
        if (self.anim - target).abs() < 0.5 {
            self.anim = target;
        }
        self.anim.max(0.0)
    }

    fn apply_drag(&mut self, delta: f32, min: f32, max: f32) {
        self.user_width = (self.user_width + delta).clamp(min, max);
    }
}

/// Pre-extracted UI state for the current frame. Returned by
/// `capture_frame_state` so the egui closures can read nav counts,
/// path, size, zoom, and folder without fighting the borrow checker
/// on `&mut self`.
struct FrameState {
    nav_count: usize,
    nav_idx: usize,
    nav_count2: usize,
    current_path: Option<PathBuf>,
    current_size: Option<(u32, u32)>,
    zoom_pct: f32,
    folder: Option<PathBuf>,
    pal_bg: [f32; 3],
    actions: Vec<UiAction>,
    dt: f32,
    ppp: f32,
}

/// Egui output data produced by `build_egui_ui` and consumed by
/// `submit_wgpu_frame`. Separates the egui frame build from the
/// wgpu surface/encoder/renderpass work so the two methods don't
/// fight over the surface texture.
struct PendingEguiOutput {
    textures_delta: egui::TexturesDelta,
    paint_jobs: Vec<egui::ClippedPrimitive>,
    screen_descriptor: egui_wgpu::ScreenDescriptor,
}

pub struct MainWindow {
    // ── Window / GPU / egui state ────────────────────────────────
    window: Option<Window>,
    wgpu_state: Option<WgpuState>,
    egui_state: Option<EguiState>,
    _pending_egui_output: Option<PendingEguiOutput>,

    // ── GPU / decode / viewer state ──────────────────────────────
    viewer: Option<Arc<Mutex<Direct2DViewer>>>,
    coordinator: Option<Arc<DecodeCoordinator>>,

    // ── Navigation / data state ──────────────────────────────────
    nav: Arc<parking_lot::Mutex<NavigationService>>,
    settings: SettingsStore,
    file_tree: crate::file_tree::FileTree,
    texture_cache: Arc<crate::texture_cache::TextureCache>,

    // ── Input / action state ─────────────────────────────────────
    actions: Vec<UiAction>,
    pending_tree_intent: Option<TreeCtxIntent>,
    /// Open right-click context menu (egui-drawn). `None` when closed.
    /// Replacing it on a new right-click automatically closes the old
    /// popup (the menu is redrawn at the new position next frame).
    ctx_menu: Option<CtxMenu>,
    /// When the context menu was opened (Instant). Used to ignore the
    /// right-button-down click that OPENED the menu — otherwise the
    /// `pointer.any_click()` outside-close detection fires on the very
    /// first frame and the menu flashes away instantly.
    ctx_menu_open_at: Option<std::time::Instant>,

    // ── Slide-show state ─────────────────────────────────────────
    slide_show_running: bool,
    slide_show_last: Option<std::time::Instant>,

    // ── Layout / viewport state ──────────────────────────────────
    viewport_w: u32,
    viewport_h: u32,
    show_tree: bool,
    show_thumbs: bool,
    tree_panel: PanelWidth,
    thumb_panel: PanelWidth,
    last_thumb_idx: usize,
    is_fullscreen: bool,
    initial_folder: Option<PathBuf>,
    single_image_size: Option<(u32, u32)>,

    // ── UI chrome / animation state ──────────────────────────────
    show_shortcut_help: bool,
    /// Whether the title-bar ⚙ settings dropdown is open.
    show_settings_menu: bool,
    /// Physical-pixel rect of the floating fullscreen toolbar (in logical
    /// px via the pane), used by click-outside-to-hide in window_event.
    fullscreen_bar_rect: Option<egui::Rect>,
    chrome_visible: bool,
    chrome_hide_at: Option<std::time::Instant>,
    chrome_move_accum: f32,
    last_cursor: Option<(f32, f32)>,
    chrome_anim: f32,
    applied_visuals: Option<Theme>,
    last_frame: Option<std::time::Instant>,

    // ── GPU texture-lifetime fencing state ────────────────────────
    // Each submitted frame is assigned a monotonically increasing render
    // epoch. The viewer's retire queue tags each dead image with the epoch it
    // was last drawn in. We release a texture only once we've committed
    // `RETIRE_FRAME_BUDGET` frames past that epoch AND reaped the GPU's
    // finished work with a non-blocking `device.poll(Maintain::Poll)`. This
    // never blocks the render thread, and with Fifo + frame-latency-1 the GPU
    // is guaranteed ≤1 frame behind, so a small frame budget is provably past
    // any in-flight submit. (wgpu exposes no non-blocking "which submission
    // completed" query, so a frame fence is the correct non-blocking tool.)
    render_epoch: u64,
    /// The newest epoch we KNOW the GPU has finished rendering (advanced each
    /// frame by `RETIRE_FRAME_BUDGET` behind the current epoch).
    safe_release_epoch: u64,

    // ── Panel drag state ─────────────────────────────────────────
    drag_panel: Option<u8>,
    panel_edge_hover: Option<u8>,
    tree_rect_phys: (f32, f32, f32, f32),
    thumb_rect_phys: (f32, f32, f32, f32),

    // ── Edge drawer (left-edge auto-expand tree menu) ────────────
    // A separate floating drawer that slides out from the left edge when
    // the mouse approaches (docked), independent of the classic tree panel
    // (`show_tree`). Disabled while the classic tree panel is open. Slides
    // out on a left-edge hot zone or handle hover; slides back after the
    // mouse leaves for `TREE_LEAVE_MS`, or on any click outside (when not
    // pinned). Its handle is translucent and picks a contrasting color
    // based on the current image's overall luminance.
    drawer_open: bool,
    /// Set when the drawer is pinned open (not auto-collapsing). Unused for
    /// now — drawers are purely docked (auto), but kept for future pinning.
    #[allow(dead_code)]
    drawer_pinned: bool,
    /// When the mouse last left the drawer+hot zone, for the collapse timer.
    drawer_leave_at: Option<std::time::Instant>,
    /// Slide-out progress 0..1 (eased, starts at 0 = fully retracted).
    drawer_anim: f32,
    /// Drawer width (user-draggable / initial, logical px). Re-messured
    /// content width is kept SEPARATE (`drawer_content_min`) so a long row
    /// widens the drawer, and shrinks back to this base when the content
    /// collapses (mirrors the classic tree's `PanelWidth` content_min).
    drawer_user_width: f32,
    /// Content-driven minimum width (logical px), re-computed every frame from
    /// the tree's widest row. Temporary — the drawer temporarily widens to
    /// fit long folder names, then returns to `drawer_user_width`.
    drawer_content_min: f32,
    /// Animated effective width (logical px) that eases toward
    /// `max(drawer_user_width, drawer_content_min)` each frame (~150ms). This
    /// is the SINGLE width used for drawing, hit-testing, and wheel routing —
    /// so the visible width, click z-order, and scroll target never diverge
    /// (the old bug: render used content_min but hit-test used user_width,
    /// leaving the right ~1/3 of the drawer click-through + wheel-zoom).
    /// Also gives the widen/collapse a soft transition like the classic tree.
    drawer_width_anim: f32,
    /// Last-known average luminance of the current image (0..1), debounced.
    drawer_luminance: f32,
    /// Whether the handle should be a light (translucent-white) pill. True
    /// when the image is dark (light handle pops on dark content).
    drawer_handle_light: bool,
    /// Timestamp of the last luminance change, for the 200ms debounce.
    drawer_luminance_at: Option<std::time::Instant>,
    /// Result of `tick_drawer` for the current frame: (open_frac, width,
    /// viewer_top, viewer_h) in logical px, drawn by `build_egui_ui`.
    drawer_draw: Option<(f32, f32, f32, f32)>,

    // ── Pan / zoom state ─────────────────────────────────────────
    pan_active: bool,
    pan_last: (f32, f32),

    // ── Theme ────────────────────────────────────────────────────
    theme: Theme,

    // ── Event router ─────────────────────────────────────────────
    router: RouterState,
    last_double_click: Option<std::time::Instant>,

    // ── Arrow-key hold state ─────────────────────────────────────
    // One direction per hold. KEYDOWN of a fresh press fires a single
    // navigation immediately (tap advances on the down-stroke); the
    // per-frame dispatcher then advances on a FIXED-TIME pulse while the
    // key is held (matching a fast paced gallery "walk": each step cuts
    // to the next image directly, no animation). Pre-decode cache makes
    // these hits instant, so the interval is truly rideable.
    nav_intent: Option<(NavigationDirection, SlideDir)>,
    /// When the last hold-step fired, for fixed-rate pacing.
    nav_last_step: Option<std::time::Instant>,
    /// When the current hold started (first KEYDOWN). The fixed-rate
    /// "walk the gallery" mode only engages after the key has been held
    /// for `HOLD_NAV_THRESHOLD` — a quick tap stays a single animated
    /// navigation, and holding long enough enters fast direct-cut paging.
    nav_press_at: Option<std::time::Instant>,
}

pub struct WgpuState {
    pub surface: wgpu::Surface<'static>,
    /// wgpu device wrapped in `Arc` so the decode coordinator can hold
    /// a long-lived reference for Phase 2+ texture uploads. Phase 4
    /// (D2D removal) lets us drop the Arc, but it's cheap and forward-
    /// compatible.
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub config: wgpu::SurfaceConfiguration,
    pub surface_format: wgpu::TextureFormat,
    /// Whether `surface_format` is sRGB-encoded. Used to decide if
    /// `srgb_to_linear` must be applied to the LoadOp clear and the
    /// egui-wgpu's output — on a linear surface the GPU does NOT do
    /// linear→sRGB on store, so applying `srgb_to_linear` would
    /// over-darken the cleared region.
    pub surface_is_srgb: bool,
    pub pixels_per_point: f32,
    pub _instance: Box<wgpu::Instance>,
}

pub struct EguiState {
    pub ctx: egui::Context,
    pub renderer: egui_wgpu::Renderer,
}

impl MainWindow {

    pub fn new(target: LaunchTarget) -> Self {
        let nav = Arc::new(parking_lot::Mutex::new(NavigationService::new()));
        let settings = SettingsStore::new().expect("failed to load settings");
        let last = settings.window_size().unwrap_or((DEFAULT_W, DEFAULT_H));
        let thumb_cache = Arc::new(ThumbCache::new(ThumbCacheConfig::default()).expect("failed to init cache"));

        // Persist the default size on first run so the next launch
        // opens at the same size even if the user never resizes.
        if settings.window_size().is_none() {
            settings.set_window_size(DEFAULT_W, DEFAULT_H);
        }

        // Single-image launch: hide chrome, navigate to the parent folder
        // and select the dropped/opened file.
        // Sidebar visibility on launch respects the persisted setting
        // (default true); single-image launch forces both off below.
        let show_tree_init = settings.show_tree_on_launch();
        let mut show_tree = show_tree_init;
        let mut show_thumbs = true;
        let mut single_image_size: Option<(u32, u32)> = None;
        let folder = match target {
            LaunchTarget::SingleImage { path, width, height } => {
                show_tree = false;
                show_thumbs = false;
                single_image_size = Some((width, height));
                let parent = path.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                if nav.lock().navigate_folder(parent.clone()).is_ok() {
                    let idx = nav.lock().items().iter().position(|it| it.path == path);
                    if let Some(i) = idx {
                        nav.lock().set_index(i);
                    }
                }
                settings.set_last_folder(parent.clone());
                if Self::folder_has_images(&parent) {
                    settings.push_recent_folder(parent.clone());
                }
                Some(parent)
            }
            LaunchTarget::Folder(f) => {
                nav.lock().navigate_folder(f.clone()).ok();
                if Self::folder_has_images(&f) {
                    settings.push_recent_folder(f.clone());
                }
                settings.set_last_folder(f.clone());
                Some(f)
            }
            LaunchTarget::None => settings
                .last_folder()
                .filter(|p| p.is_dir())
                .inspect(|f| { nav.lock().navigate_folder(f.clone()).ok(); }),
        };
        let theme_setting = settings.theme();
        let file_tree = crate::file_tree::FileTree::new();
        file_tree.refresh_recent(&settings.recent_folders());
        file_tree.refresh_favorites(&settings.favorite_folders());

        Self {
            window: None,
            wgpu_state: None,
            egui_state: None,
            viewer: None,
            coordinator: None,
            nav,
            settings,
            file_tree,
            texture_cache: Arc::new(
                crate::texture_cache::TextureCache::new(thumb_cache.clone())
            ),
            actions: Vec::new(),
            pending_tree_intent: None,
            ctx_menu: None,
            ctx_menu_open_at: None,
            slide_show_running: false,
            slide_show_last: None,
            viewport_w: last.0.saturating_sub(TREE_WIDTH + THUMB_WIDTH),
            viewport_h: last.1.saturating_sub(TOOLBAR_HEIGHT + STATUS_BAR_HEIGHT),
            show_tree,
            show_thumbs,
            tree_panel: PanelWidth::new(TREE_WIDTH as f32),
            thumb_panel: PanelWidth::new(THUMB_WIDTH as f32),
            last_thumb_idx: 0,
            is_fullscreen: false,
            router: RouterState::new(),
            last_double_click: None,
            nav_intent: None,
            nav_last_step: None,
            nav_press_at: None,
            initial_folder: folder,
            show_shortcut_help: false,
            show_settings_menu: false,
            fullscreen_bar_rect: None,
            chrome_visible: true,
            chrome_hide_at: None,
            chrome_move_accum: 0.0,
            last_cursor: None,
            chrome_anim: 0.0,
            applied_visuals: None,
            drag_panel: None,
            panel_edge_hover: None,
            tree_rect_phys: (0.0, 0.0, 0.0, 0.0),
            thumb_rect_phys: (0.0, 0.0, 0.0, 0.0),
            drawer_open: false,
            drawer_pinned: false,
            drawer_leave_at: None,
            drawer_anim: 0.0,
            drawer_user_width: TREE_WIDTH as f32,
            drawer_content_min: 0.0,
            drawer_width_anim: TREE_WIDTH as f32,
            drawer_luminance: 0.5,
            drawer_handle_light: false,
            drawer_luminance_at: None,
            drawer_draw: None,
            _pending_egui_output: None,
            pan_active: false,
            pan_last: (0.0, 0.0),
            theme: theme_setting,
            single_image_size,
            last_frame: None,
            render_epoch: 0,
            safe_release_epoch: 0,
        }
    }

    /// Active color palette for the current theme.
    fn pal(&self) -> Palette {
        match resolve_theme(self.theme) {
            Theme::Dark => dark_palette(),
            Theme::Light => light_palette(),
            // resolve_theme never returns System, but the compiler needs
            // the arm.
            Theme::System => dark_palette(),
        }
    }

    /// Advance the edge-drawer state machine and return `Some((open_frac,
    /// width_px, top_px, height_px))` when the drawer should be drawn
    /// (logical px for egui). Call once per frame from `tick_pre_frame`.
    ///
    /// Trigger model — the handle is the ONLY trigger:
    /// - Open: the pointer is near the handle (a thin vertical pill at the
    ///   left edge, inset `HANDLE_X`). The handle sits vertically centered in
    ///   the viewer, positioned from WINDOW GEOMETRY (toolbar→status) so it
    ///   never depends on a stale `viewport_size` value.
    /// - Keep open: pointer on the handle, the drawer body, or a slim
    ///   left-edge strip (full viewer height).
    /// - Close: pointer leaves all of those for `TREE_LEAVE_MS` (or an
    ///   outside click). Pinned drawers ignore this.
    ///
    /// The handle color follows the current image's overall luminance with
    /// hysteresis + debounce (stable across pan/zoom — re-measured per image
    /// load, not per frame).
    fn tick_drawer(&mut self, dt: f32) -> Option<(f32, f32, f32, f32)> {
        let ppp = self.wgpu_state
            .as_ref()
            .map(|w| w.pixels_per_point)
            .unwrap_or(1.0)
            .max(0.1);

        // Viewer vertical span in LOGICAL px, from window geometry (not a
        // possibly-stale viewport field). These are current and stable.
        let win_h_px = self.wgpu_state.as_ref().map(|w| w.config.height).unwrap_or(800);
        let top_logical = TOOLBAR_HEIGHT as f32;
        let bottom_logical = (win_h_px as f32 / ppp) - STATUS_BAR_HEIGHT as f32;
        let vh_logical = (bottom_logical - top_logical).max(1.0);

        // Handle rect (logical), inset from the window edge, vertically
        // centered in the viewer. THIS is the single source for the handle
        // geometry (also used by the draw pass) — keep its derivation in
        // sync with the draw block so the trigger zone lines up exactly.
        let handle_w = 5.0_f32;
        let handle_h = 48.0_f32;
        let handle_x = HANDLE_X;
        let handle_y = top_logical + vh_logical * 0.5 - handle_h * 0.5;
        let handle_rect = egui::Rect::from_min_size(
            egui::pos2(handle_x, handle_y),
            egui::vec2(handle_w, handle_h),
        );
        // Proximity zone = handle rect padded. This is the ONLY open trigger.
        let zone = handle_rect.expand(HANDLE_ZONE_PAD);
        let active = !self.is_fullscreen && !self.show_tree && self.viewer.is_some();
        if active {
            // `router.cursor_pos` is PHYSICAL px; the handle/zone/geometry are
            // LOGICAL, so convert the cursor to logical before comparing.
            let cursor = egui::pos2(
                self.router.cursor_pos.x / ppp,
                self.router.cursor_pos.y / ppp,
            );
            // Left-edge strip (full viewer height, ~10px): a KEEP-OPEN region
            // only, NOT a first-open trigger (so the drawer pops only when the
            // pointer nears the handle, not anywhere along the left edge).
            let in_left_strip = cursor.x >= 0.0
                && cursor.x <= 10.0
                && cursor.y >= top_logical
                && cursor.y <= bottom_logical;
            let in_zone = zone.contains(cursor);
            let in_drawer = self.drawer_open && {
                // Use the ANIMATED width (what's actually drawn) so the
                // keep-open region matches the visible drawer.
                let dx = self.drawer_width_anim;
                cursor.x >= 0.0
                    && cursor.x <= dx
                    && cursor.y >= top_logical
                    && cursor.y <= bottom_logical
            };

            if in_zone {
                // First open via handle proximity.
                self.drawer_open = true;
                self.drawer_leave_at = None;
            } else if self.drawer_open {
                // Once open, keep it while the pointer is on the handle, the
                // left strip, or the drawer body; otherwise start the timer.
                if in_left_strip || in_drawer {
                    self.drawer_leave_at = None;
                } else {
                    let now = std::time::Instant::now();
                    match self.drawer_leave_at {
                        None => self.drawer_leave_at = Some(now),
                        Some(t) if now.duration_since(t).as_millis() as u64 >= TREE_LEAVE_MS => {
                            self.drawer_open = false;
                            self.drawer_leave_at = None;
                        }
                        _ => {}
                    }
                }
            }
        } else {
            self.drawer_open = false;
            self.drawer_leave_at = None;
        }

        // Ease the open fraction toward the target.
        let target = if self.drawer_open { 1.0 } else { 0.0 };
        let k = ((dt * 1000.0) / TREE_DRAWER_ANIM_MS).clamp(0.0, 1.0);
        if self.drawer_anim < target {
            self.drawer_anim = (self.drawer_anim + k).min(target);
        } else if self.drawer_anim > target {
            self.drawer_anim = (self.drawer_anim - k).max(target);
        }
        let a = self.drawer_anim;
        let frac = if a < 0.5 { 2.0 * a * a } else { 1.0 - (-2.0 * a + 2.0).powi(2) / 2.0 };

        // Luminance debounce + hysteresis for the handle contrast.
        let lum = self.viewer.as_ref().map(|v| v.lock().image_luminance).unwrap_or(0.5);
        let changed = (self.drawer_luminance - lum).abs() > 0.001;
        if changed {
            let now = std::time::Instant::now();
            match self.drawer_luminance_at {
                None => self.drawer_luminance_at = Some(now),
                Some(t) if now.duration_since(t).as_millis() as u64 >= HANDLE_DEBOUNCE_MS => {
                    self.drawer_luminance = lum;
                    if lum > HANDLE_LIGHT_THRESHOLD {
                        self.drawer_handle_light = false;
                    } else if lum < HANDLE_DARK_THRESHOLD {
                        self.drawer_handle_light = true;
                    }
                    self.drawer_luminance_at = None;
                }
                _ => {}
            }
        } else {
            self.drawer_luminance_at = None;
        }

        // Ease the effective width toward the target (max of the user-dragged
        // base and the content minimum) so widening for a long row and
        // collapsing back are soft transitions (not an instant jump). This
        // eased width is the single source for drawing + hit-test + wheel.
        let width_target = self.drawer_user_width.max(self.drawer_content_min);
        let wk = ((dt * 1000.0) / 150.0).clamp(0.0, 1.0); // ~150ms exponential
        self.drawer_width_anim += (width_target - self.drawer_width_anim) * wk;
        if (self.drawer_width_anim - width_target).abs() < 0.5 {
            self.drawer_width_anim = width_target;
        }

        if !self.drawer_open && self.drawer_anim <= 0.001 {
            // Fully retracted: body not drawn, but the handle must always
            // render. Return Some so the caller draws the handle.
            if active {
                return Some((0.0, 0.0, top_logical, vh_logical));
            }
            return None;
        }
        let width = self.drawer_width_anim * frac;
        Some((frac, width, top_logical, vh_logical))
    }

    /// Phase 8: pull the active palette's panel_bg. Used by the
    /// context-menu egui::Windows (which run inside closures that
    /// don't have `self`).
    #[allow(dead_code)]
    fn pal_bg(&self) -> egui::Color32 { self.pal().panel_bg }
    #[allow(dead_code)]
    fn pal_btn(&self) -> egui::Color32 { self.pal().button_fill }
    #[allow(dead_code)]
    fn pal_text(&self) -> egui::Color32 { self.pal().text_primary }
    #[allow(dead_code)]
    fn pal_text_dim(&self) -> egui::Color32 { self.pal().text_tertiary }
    #[allow(dead_code)]
    fn pal_stroke(&self) -> egui::Color32 { self.pal().card_stroke }

    fn init_window(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let stored = self.settings.window_size().unwrap_or((DEFAULT_W, DEFAULT_H));
        tracing::info!("init_window: stored size {:?}", stored);

        // winit's `inner_size()` at this point can over-report the actual
        // client size (because the OS hasn't yet finished sizing the
        // window). We therefore create the window + wgpu/egui/d2d in two
        // phases: the window is created here at the requested logical size;
        // the renderer is created in the first Resized event where the
        // size is authoritative.
        // Cap the initial size to 90% of the primary monitor so we never
        // create a window that exceeds the screen (Windows would clip it
        // and the egui surface would be bigger than the visible area).
        let monitor = event_loop.primary_monitor();
        let (max_w, max_h, scale, mon_pos, mon_size) = monitor
            .map(|m| {
                let sf = m.scale_factor();
                let LogicalSize { width, height } = m.size().to_logical::<f64>(sf);
                // Phase 8: also grab the monitor's logical origin +
                // size so we can center the window on it. Without an
                // explicit position winit lets the OS place the window
                // wherever it likes — on multi-monitor setups or after
                // the previous session moved it to the bottom-right,
                // the window can open partially off-screen.
                let LogicalPosition { x, y } = m.position().to_logical::<f64>(sf);
                let LogicalSize { width: mw, height: mh } = m.size().to_logical::<f64>(sf);
                (width * 0.9, height * 0.9, sf, (x, y), (mw, mh))
            })
            .unwrap_or((1920.0, 1080.0, 1.0, (0.0, 0.0), (1920.0, 1080.0)));

        let (init_w, init_h, min_w, min_h) = if let Some((iw, ih)) = self.single_image_size {
            // Immersive single-image launch. Goal: image occupies the
            // viewer rect with NO letterbox (win_w / (win_h - chrome_h)
            // == iw / ih). Chrome is fixed (titlebar + status bar) and
            // sits outside the image area, so the window is image
            // dimensions plus chrome while keeping image aspect.
            //
            // Previous bug: only TOOLBAR_HEIGHT (40) was added; the
            // 48px status bar was forgotten, which compressed the
            // status bar zone AND kept win aspect independent of image
            // aspect — produced wide letterbox bands (D2D viewer
            // letterboxing the image).
            //
            // Also: min_inner_size is unified to MIN_W/MIN_H (same as
            // the multi-image modes) so the user can resize the window
            // smaller than the image, and so `set_fullscreen`
            // (Borderless(None)) does not collide with the OS min-size
            // constraint (which made single-image fullscreen overflow
            // the monitor on certain configs).
            let chrome_w = 0.0_f64;
            let chrome_h = (TOOLBAR_HEIGHT + STATUS_BAR_HEIGHT) as f64;
            let img_w = iw as f64 / scale;
            let img_h = ih as f64 / scale;
            let img_aspect = img_w / img_h;

            // Width target: if image is wider than 90% of monitor width,
            // shrink so it fits; otherwise show at native size.
            let target_w = img_w.min(max_w).max(MIN_W as f64);
            // Derive height from aspect; chrome_h lives outside the
            // image area so win_h - chrome_h matches image aspect.
            let mut lh = (target_w - chrome_w) / img_aspect + chrome_h;
            lh = lh.min(max_h).max(MIN_H as f64);
            let lw = ((lh - chrome_h) * img_aspect + chrome_w)
                .max(MIN_W as f64).min(max_w);

            tracing::debug!(
                "single_image window: image={}x{} scale={:.2} -> init_w={:.0} init_h={:.0} (image aspect={:.3}, viewer aspect={:.3})",
                iw, ih, scale, lw, lh,
                img_aspect, (lw - chrome_w) / (lh - chrome_h)
            );

            // Min window size: unified with the multi-image modes so
            // winit's min_inner_size constraint does not conflict with
            // Borderless fullscreen on monitors smaller than the image.
            (lw, lh, MIN_W as f64, MIN_H as f64)
        } else if self.settings.window_size().is_some() {
            // Restored session size. `stored` is *intended* to be in
            // LOGICAL pixels (save_window_geometry now saves logical
            // via to_logical) but settings.json from older builds
            // (commits before today) may contain physical pixels —
            // those would translate to ever-larger windows on each
            // launch at >100% DPI. Migration: if `stored` exceeds
            // the monitor's logical 90% by a wide margin (the
            // tell-tale sign of a physical value), divide by the
            // current scale factor to recover the logical size.
            let (stored_lw, stored_lh) = {
                let too_big_w = stored.0 as f64 > max_w * 1.5;
                let too_big_h = stored.1 as f64 > max_h * 1.5;
                if too_big_w || too_big_h {
                    tracing::warn!(
                        "settings.json window_size ({},{}) appears to be physical pixels; \
                         converting to logical at scale={:.2}",
                        stored.0, stored.1, scale,
                    );
                    (
                        (stored.0 as f64 / scale).max(MIN_W as f64).min(max_w),
                        (stored.1 as f64 / scale).max(MIN_H as f64).min(max_h),
                    )
                } else {
                    (
                        (stored.0 as f64).max(MIN_W as f64).min(max_w),
                        (stored.1 as f64).max(MIN_H as f64).min(max_h),
                    )
                }
            };
            (
                stored_lw,
                stored_lh,
                MIN_W as f64,
                MIN_H as f64,
            )
        } else {
            // First run: about two-thirds of the work area.
            ((max_w / 1.5).max(MIN_W as f64), (max_h / 1.5).max(MIN_H as f64), MIN_W as f64, MIN_H as f64)
        };

        // Phase 8: DPI-aware default placement. Center the window on
        // the primary monitor in LOGICAL coordinates (winit converts
        // LogicalPosition → PhysicalPosition using the same scale
        // factor we read above, so this is correct at 100% / 125% /
        // 150% scaling alike). Clamped so the title bar always stays
        // visible even if the computed rect slightly overflows.
        let pos_x = (mon_pos.0 + (mon_size.0 - init_w) / 2.0)
            .max(mon_pos.0)
            .min(mon_pos.0 + mon_size.0 - min_w);
        let pos_y = (mon_pos.1 + (mon_size.1 - init_h) / 2.0)
            .max(mon_pos.1)
            .min(mon_pos.1 + mon_size.1 - min_h);

let window = event_loop.create_window(
            WindowAttributes::default()
                .with_title("Aperture Neo")
                .with_inner_size(LogicalSize::new(init_w, init_h))
                .with_min_inner_size(LogicalSize::new(min_w, min_h))
                // Phase 8: explicit centered placement (see below).
                .with_position(LogicalPosition::new(pos_x, pos_y))
                // Custom-drawn titlebar: the OS frame is removed; egui draws
                // the title row (menu buttons + window controls). Resize
                // borders are restored in init_renderer by re-adding
                // WS_THICKFRAME (gives edge resize + Aero Snap back).
                .with_decorations(false)
                // Create the window HIDDEN. DWM's first composition of
                // a newly-created visible window can land before
                // WindowEvent::Resized fires (and therefore before
                // init_renderer runs and presents the wgpu/D2D
                // surfaces), which produces the launch flash. By
                // starting hidden and revealing from init_renderer
                // after both surfaces have been presented at least
                // once, the first composition the user can possibly
                // see already has both surfaces populated.
                .with_visible(false),
        )?;

        // Phase 16 fix: MAIN_HWND was read everywhere (native menus,
        // wallpaper, clipboard) but never written — it stayed 0, so the
        // native TrackPopupMenu right-click menus bailed on
        // `if hwnd_raw == 0 { return; }`. Grab the real Win32 HWND from
        // the winit window handle here once.
        use raw_window_handle::RawWindowHandle;
        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::Win32(win32) = handle.as_raw() {
                MAIN_HWND.store(win32.hwnd.get() as isize, std::sync::atomic::Ordering::Relaxed);
            }
        }

        self.window = Some(window);
        Ok(())
    }

    /// Phase-2 init: create the wgpu surface, egui context, D2D viewer,
    /// and HWND-owned child. Called from the first `Resized` event with
    /// the authoritative OS-reported size.
    fn init_renderer(&mut self, w: u32, h: u32) -> Result<()> {
        if self.wgpu_state.is_some() {
            return Ok(());
        }
        let window = self.window.as_ref().ok_or_else(|| anyhow::anyhow!("window not yet created"))?;

        let wgpu_state = init_wgpu_at_size(window, w, h)?;
        tracing::info!("init_renderer: wgpu OK");
        let egui_state = init_egui(&wgpu_state.device, wgpu_state.surface_format);
        tracing::info!("init_renderer: egui OK");

        // Viewer is unitless (no pixels yet); the first viewer.resize
        // call below sets the physical-pixel viewport.
        let viewer = Arc::new(Mutex::new(Direct2DViewer::new(1, 1)));

        // egui-native rendering: the decoded frame is uploaded as an
        // egui::TextureHandle (via the coordinator holding a clone of the
        // egui Context), and painted through egui's mesh pipeline. No
        // separate wgpu image-quad pipeline is needed.
        let coordinator = Arc::new(DecodeCoordinator::new(
            self.nav.clone(), viewer.clone(),
            egui_state.ctx.clone(),
            wgpu_state.device.limits().max_texture_dimension_2d,
        ));

        // Extract HWND from the winit window
        let hwnd = {
            let raw = window.window_handle()?.as_raw();
            match raw {
                RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
                _ => return Err(anyhow::anyhow!("Expected Win32 window handle")),
            }
        };
        tracing::info!("init_renderer: HWND = {:?}", hwnd.0);

        // Add WS_CLIPCHILDREN so the wgpu/egui surface does not paint over
        // the embedded Direct2D viewer child window, and restore
        // WS_THICKFRAME (removed by with_decorations(false)) so Windows
        // provides edge-resize + Aero Snap for the custom-chrome window.
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_STYLE,
                WS_CLIPCHILDREN, WS_THICKFRAME, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE,
                SWP_NOZORDER, SWP_NOACTIVATE,
            };
            let prev = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_STYLE,
                prev | WS_CLIPCHILDREN.0 as isize | WS_THICKFRAME.0 as isize,
            );
            let _ = SetWindowPos(
                hwnd,
                None,
                0, 0, 0, 0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            // Win11 rounded corners for the custom-chrome window.
            use windows::Win32::Graphics::Dwm::{
                DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE,
            };
            const DWMWCP_ROUND: i32 = 2;
            let pref: i32 = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
            // Subclass the window for borderless edge-resize: winit does not
            // hit-test resize borders for undecorated windows, so we handle
            // WM_NCCALCSIZE (client = full window) and WM_NCHITTEST (edge
            // zones → HT*) ourselves, forwarding everything else to winit.
            install_resize_hook(hwnd);
        }

        let (cx, cy, cw, ch) = self.compute_viewer_rect(
            wgpu_state.config.width, wgpu_state.config.height);

        // Hand the physical-pixel viewport to the viewer immediately so
        // the first decode (coordinator.request_current below) →
        // set_image_gpu → compute_fit sees a correct viewport. We use
        // set_viewport_physical (no compute_fit) because there is no
        // image yet — compute_fit is a no-op anyway, and reserving
        // resize() (which triggers rect-anim machinery) for the OS
        // Resized path keeps semantics clean.
        viewer.lock().set_viewport_physical(cw, ch, cx as f32, cy as f32);

        self.wgpu_state = Some(wgpu_state);
        self.egui_state = Some(egui_state);
        self.viewer = Some(viewer);
        self.coordinator = Some(coordinator);

        // Load initial folder if present (already loaded in new()).
        // Just trigger a decode of the current item.
        let count = self.nav.lock().count();
        tracing::info!("init_renderer: nav count = {}", count);
        if count > 0 {
        if let Some(coordinator) = &self.coordinator {
            coordinator.clear_caches();
            coordinator.request_current(SlideDir::None);
        }
            if let Some(ref folder) = self.initial_folder {
                tracing::info!("Initial folder loaded: {}", folder.display());
            }
        }

        // Force-present the wgpu surface once so the main HWND's
        // DXGI back buffer is populated with canvas_clear before DWM
        // does its very first composition. Without this, the first
        // DWM sample of the parent's DXGI surface finds an uninit
        // back buffer and falls back to drawing the COLOR_WINDOW
        // brush into the parent's client area — this is the
        // persistent "dark/light launch flash" the user has been
        // seeing. Force-present the wgpu surface so the main
        // HWND's back buffer is populated before DWM's first
        // composition.
        self.present_wgpu_surface_for_init();

        // Now that both surfaces have a populated first back buffer,
        // make the main HWND visible. It was created with
        // with_visible(false) in init_window so DWM couldn't
        // compose an empty placeholder before we got here.
        if let Some(w) = &self.window {
            w.set_visible(true);
        }

        Ok(())
    }

    /// Physical size of the window CLIENT area, via Win32 GetClientRect.
    /// More reliable than winit's `inner_size()` / `current_monitor().size()`
    /// in borderless fullscreen, both of which can report the size scaled
    /// by the DPI factor (making the wgpu surface larger than the display
    /// and pushing the image off-centre). Falls back to inner_size() when
    /// the HWND isn't registered yet.
    ///
    /// Takes the main HWND out-of-band so it can be called while
    /// `self.egui_state` is mutably borrowed (a `&self` method would
    /// conflict). Returns `(u32, u32)` physical client size.
    fn client_area_physical_using(hwnd_raw: isize, fallback: (u32, u32)) -> (u32, u32) {
        if hwnd_raw != 0 {
            use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
            let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            let mut rc = windows::Win32::Foundation::RECT::default();
            let ok = unsafe { GetClientRect(hwnd, &mut rc) };
            if ok.is_ok() && rc.right > rc.left && rc.bottom > rc.top {
                return ((rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32);
            }
        }
        fallback
    }

    fn compute_viewer_rect(&self, win_w: u32, win_h: u32) -> (i32, i32, u32, u32) {
        // Snap panel widths to WHOLE physical pixels (round(anim*ppp)/ppp),
        // matching the exact snap egui uses when drawing each panel
        // (build_egui_ui line "phase 12"). Using a bare logical round()
        // here would drift from the drawn tree edge by a fractional
        // physical pixel during the panel animation, leaving a 1px seam
        // between the tree edge and the viewer (and desyncing the
        // in_viewer hit-test from where the image is actually drawn).
        let ppp = self.wgpu_state
            .as_ref()
            .map(|w| w.pixels_per_point)
            .unwrap_or(1.0)
            .max(0.1);
        let snap = |x: f32| (x * ppp).round() / ppp;
        let tree_w = if self.show_tree { snap(self.tree_panel.anim).round() as u32 } else { 0 };
        let thumb_w = if self.show_thumbs { snap(self.thumb_panel.anim).round() as u32 } else { 0 };
        self.viewer_rect_with_widths(win_w, win_h, tree_w, thumb_w)
    }

    fn viewer_rect_with_widths(
        &self, win_w: u32, win_h: u32, tree_w: u32, thumb_w: u32,
    ) -> (i32, i32, u32, u32) {
        // Scale ALL panel sizes to physical pixels using the same
        // pixels_per_point that egui renders at, so the D2D viewer aligns
        // exactly beneath the egui panels.
        let ppp = self.wgpu_state
            .as_ref()
            .map(|w| w.pixels_per_point)
            .unwrap_or(1.0)
            .max(0.1);
        let tree_w_phys = (tree_w as f32 * ppp).round() as u32;
        let thumb_w_phys = (thumb_w as f32 * ppp).round() as u32;
        // Fullscreen hides the title/status bars; the bottom toolbar is a
        // floating overlay, so no space is reserved.
        let (toolbar, status) = if self.is_fullscreen {
            (0, 0)
        } else {
            (
                (TOOLBAR_HEIGHT as f32 * ppp).round() as i32,
                (STATUS_BAR_HEIGHT as f32 * ppp).round() as i32,
            )
        };
        let x = tree_w_phys as i32;
        let y = toolbar;
        let w = win_w.saturating_sub(tree_w_phys + thumb_w_phys);
        let h = win_h.saturating_sub((toolbar + status) as u32);
        (x, y, w, h)
    }

    /// Continuous-precision viewer rect (f32 physical), derived from the
    /// UNROUNDED panel animation widths. Used to drive the viewer's fit
    /// re-centre during a panel collapse so the image tracks the smooth
    /// animation instead of the quantized integer panel edge (the
    /// "pause then jump" stall). The integer `central_rect_phys` is still
    /// used for the image-quad scissor (rasterisation bound), so this
    /// continuous origin never draws outside the panel.
    fn viewer_rect_physical_continuous(&self, win_w: u32, win_h: u32) -> (f32, f32, f32, f32) {
        let ppp = self.wgpu_state
            .as_ref()
            .map(|w| w.pixels_per_point)
            .unwrap_or(1.0)
            .max(0.1);
        let tree_px = if self.show_tree && !self.is_fullscreen { self.tree_panel.anim * ppp } else { 0.0 };
        let thumb_px = if self.show_thumbs && !self.is_fullscreen { self.thumb_panel.anim * ppp } else { 0.0 };
        // In fullscreen the title bar and status bar are hidden, and the
        // bottom toolbar is a FLOATING overlay (does not reserve layout
        // space), so the image occupies the full client rect. The egui
        // CentralPanel already spans the full window; the bar floats over it.
        let (toolbar_px, status_px) = if self.is_fullscreen {
            (0.0, 0.0)
        } else {
            (TOOLBAR_HEIGHT as f32 * ppp, STATUS_BAR_HEIGHT as f32 * ppp)
        };
        let x = tree_px;
        let y = toolbar_px;
        let w = (win_w as f32 - tree_px - thumb_px).max(0.0);
        let h = (win_h as f32 - toolbar_px - status_px).max(0.0);
        (x, y, w, h)
    }

    fn handle_navigation(&mut self, direction: NavigationDirection, slide_dir: SlideDir) {
        self.nav.lock().move_to(direction);
        if let Some(coordinator) = &self.coordinator {
            coordinator.request_current(slide_dir);
        }
        // Phase 3: a user-initiated navigation (Prev/Next keys
        // or button) resets the slide-show timer so the auto-advance
        // doesn't double-fire on top of the manual move. The
        // slide-show tick (render_frame) is what fires when
        // running; we don't pause/resume here, just delay the
        // next tick.
        if self.slide_show_running {
            self.slide_show_last = Some(std::time::Instant::now());
        }
        if let Some(window) = &self.window { window.request_redraw(); }
    }

    /// Arrow-key KEYDOWN handler. A fresh press (no prior intent) fires
    /// navigation IMMEDIATELY so a single tap still jumps on the
    /// down-stroke (with the iOS slide). Every press then records the
    /// direction so the fixed-rate dispatcher can keep advancing while the
    /// key is held. Only the FIRST press triggers an immediate jump — OS
    /// auto-repeat KEYDOWNs just re-assert the same intent.
    fn on_arrow_keydown(&mut self, direction: NavigationDirection, slide_dir: SlideDir) {
        if self.nav_intent.is_none() {
            // First press in a fresh hold — jump immediately (animated).
            let now = std::time::Instant::now();
            self.nav_press_at = Some(now);
            self.nav_last_step = Some(now);
            self.handle_navigation(direction, slide_dir);
        }
        self.nav_intent = Some((direction, slide_dir));
    }

    /// Per-frame dispatcher for a held arrow key, called from
    /// `render_frame` BEFORE the egui frame. Fires the next step on a
    /// FIXED-TIME pulse (`HOLD_NAV_INTERVAL`) while the key is held — but
    /// only after the key has been held for `HOLD_NAV_THRESHOLD`; a quick
    /// tap stays a single animated navigation and never enters the walk.
    /// Each held step uses `SlideDir::None` (direct cut, no slide) — this
    /// is the fast "walk the gallery" mode: steady pace, no per-step
    /// animation, no waiting on decode (the pre-decode cache makes it a
    /// hit). KEYUP clears `nav_intent`, so a frame after release the next
    /// step is a no-op — immediate stop.
    fn tick_arrow_nav(&mut self) {
        let Some((direction, _)) = self.nav_intent else { return; };
        let now = std::time::Instant::now();
        // The walk only engages once the hold has lasted the threshold.
        let engaged = self
            .nav_press_at
            .map(|t| now.duration_since(t) >= HOLD_NAV_THRESHOLD)
            .unwrap_or(false);
        if !engaged {
            return;
        }
        let due = self
            .nav_last_step
            .map(|t| now.duration_since(t) >= HOLD_NAV_INTERVAL)
            .unwrap_or(true);
        if !due {
            return;
        }
        self.nav_last_step = Some(now);
        // Held steps cut directly (no slide) — the iOS transition is for
        // single taps / button clicks only.
        self.handle_navigation(direction, SlideDir::None);
    }

    fn navigate_to_folder(&mut self, path: PathBuf) {
        // Keep the current image + thumbnails when the target folder has
        // no images — the viewer always reflects the displayed image's
        // own folder.
        if !Self::folder_has_images(&path) {
            tracing::debug!(
                "navigate_to_folder: {} has no images — keeping current view",
                path.display()
            );
            return;
        }
        if let Err(e) = self.nav.lock().navigate_folder(path.clone()) {
            tracing::error!("navigate_to_folder({}) failed: {:#}", path.display(), e);
            return;
        }
        if Self::folder_has_images(&path) {
            self.settings.push_recent_folder(path.clone());
        }
        self.settings.set_last_folder(path.clone());
        self.file_tree.refresh_recent(&self.settings.recent_folders());
        self.file_tree.refresh_favorites(&self.settings.favorite_folders());
        if let Some(coordinator) = &self.coordinator {
            coordinator.request_current(SlideDir::None);
        }
        if let Some(window) = &self.window { window.request_redraw(); }
    }

    /// Page navigation: jump by the number of thumbnails visible per page
    /// (real paging, not a fixed ±10).
    fn handle_navigation_jump(&mut self, delta: i32) {
        let len = self.nav.lock().count();
        if len == 0 { return; }
        let cur = self.nav.lock().current_index() as i32;
        let page = self.thumbs_per_page();
        let target = (cur + delta * page as i32).clamp(0, (len as i32) - 1) as usize;
        self.handle_navigation(NavigationDirection::Index(target), SlideDir::None);
    }

    /// How many thumbnail cards fit in the visible thumbs panel
    /// (card ≈ image area + label + spacing).
    fn thumbs_per_page(&self) -> usize {
        let card_h = 170.0 + 30.0; // image cap + label/margins
        let vh = self.viewport_h as f32;
        ((vh / card_h).floor() as usize).clamp(1, 100)
    }

    /// Ctrl+Arrow: cycle among folders in the tree root the user last
    /// clicked (ACTIVE_ROOT): Favorites → favorites list, Recent → recent
    /// list, This PC → sibling folders (same parent) that contain images.
    fn handle_cycle_folder(&mut self, dir: i32) {
        let Some(cur) = self.nav.lock().current().map(|i| i.path.clone()) else { return };
        let folder = cur.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| cur.clone());

        let active_root = ACTIVE_ROOT.load(std::sync::atomic::Ordering::Relaxed);
        let (list, kind) = match active_root {
            0 => (self.settings.favorite_folders(), "favorites"),
            1 => (self.settings.recent_folders(), "recent"),
            _ => {
                let siblings = std::fs::read_dir(&folder)
                    .map(|rd| {
                        let mut v: Vec<PathBuf> = rd
                            .flatten()
                            .map(|e| e.path())
                            .filter(|p| p.is_dir() && Self::folder_has_images(p))
                            .collect();
                        v.sort();
                        v
                    })
                    .unwrap_or_default();
                (siblings, "siblings")
            }
        };
        if list.len() < 2 { return; }
        let idx = list.iter().position(|f| f == &folder).unwrap_or(0);
        let len = list.len() as i32;
        let next = list[((idx as i32 + dir).rem_euclid(len)) as usize].clone();
        tracing::debug!("cycle_folder({kind}): {} -> {}", folder.display(), next.display());
        self.navigate_to_folder(next);
    }

    fn handle_wheel_in_viewer(&mut self, delta_y: f32, cursor_x: i32, cursor_y: i32) {
        let wheel = (delta_y * 100.0) as i32;
        if let Some(v_arc) = self.viewer.as_ref() {
            let mut v = v_arc.lock();
            v.on_wheel(wheel, cursor_x as f32, cursor_y as f32);
        }
        if let Some(window) = &self.window { window.request_redraw(); }
    }

    fn handle_double_click(&mut self) {
        if let Some(v_arc) = self.viewer.as_ref() {
            let mut v = v_arc.lock();
            v.fit_to_screen();
        }
        if let Some(window) = &self.window { window.request_redraw(); }
    }

    fn render_frame(&mut self) -> Result<()> {
        self.tick_pre_frame();
        // Assign this frame's render epoch and stamp it into the viewer so
        // retired images are tagged with the submission that drew them. The
        // fence in submit_wgpu_frame confirms completion before releasing.
        self.render_epoch += 1;
        if let Some(viewer) = &self.viewer {
            viewer.lock().set_render_epoch(self.render_epoch);
        }
        let mut state = self.capture_frame_state();
        let central_rect_phys = self.build_egui_ui(&mut state);
        self.submit_wgpu_frame(central_rect_phys, &state)?;
        self.drain_frame_actions(state.actions);
        Ok(())
    }

    /// Blocks A+B: coordinator poll, arrow-nav tick, slideshow tick, viewer bg sync.
    fn tick_pre_frame(&mut self) {
        let pal = self.pal();
        let dt = self.last_frame.map(|t| t.elapsed().as_secs_f32()).unwrap_or(1.0 / 60.0);
        // 1. Poll decode coordinator
        if let Some(coordinator) = &self.coordinator {
            // coordinator.poll() applies any in-flight decoded
            // bitmap to the viewer (calls set_image on the
            // Direct2DViewer). The image upload happens on the
            // NEXT render. With ControlFlow::Poll (Phase 7 回滚
            // Phase 5 的 Wait), the next render fires
            // automatically; we don't need an explicit
            // request_redraw() here.
            coordinator.poll();
            // Warm the pre-decode cache for the current image's ±2
            // neighbours so a fast navigation hits the cache (instant)
            // instead of blocking on WIC.
            coordinator.predecode_adjacent();
        }

        // Advance the rotation + rect animations. The rect-anim MUST be
        // committed when it finishes — otherwise a rect-anim started by
        // set_rotation / fit_to_screen stays resident forever and the
        // image-quad ignores pan/zoom/offset (the "rotation breaks wheel
        // zoom / fullscreen doesn't fit until I drag" bug).
        if let Some(viewer) = &self.viewer {
            let mut v = viewer.lock();
            v.tick_rotation();
            v.tick_rect_anim();
            v.tick_slide();
        }

        // Held-arrow-key dispatcher. Advances one image per fixed-rate
        // pulse while a key is held (direct cut, no slide). KEYUP clears
        // `nav_intent`, so a frame after release this stops immediately.
        self.tick_arrow_nav();

        // Phase 3: slide-show 3-second tick. The user toggles via
        // UiAction::ToggleSlideShow; render_frame here is where the
        // auto-advance fires. We re-use handle_navigation so the
        // slide animation is identical to a manual Next — the
        // existing touch-gallery style handles the visual.
        if self.slide_show_running {
            const SLIDE_TICK: std::time::Duration = std::time::Duration::from_secs(3);
            let now = std::time::Instant::now();
            let elapsed = self.slide_show_last.map(|t| now.duration_since(t)).unwrap_or(SLIDE_TICK);
            if elapsed >= SLIDE_TICK {
                self.slide_show_last = Some(now);
                self.handle_navigation(NavigationDirection::Next, SlideDir::Next);
                if let Some(window) = &self.window { window.request_redraw(); }
            }
        }

        // Keep the viewer background in sync with the theme.
        if let Some(v) = &self.viewer {
            v.lock().bg = pal.d2d_clear;
        }

        // Advance the edge-drawer state machine (left-edge auto-expand tree
        // menu). Stored in `drawer_draw` for `build_egui_ui` to render.
        self.drawer_draw = self.tick_drawer(dt);
    }

    /// Block C: pre-extract UI state so closures don't fight &mut self.
    fn capture_frame_state(&mut self) -> FrameState {
        let nav_count = self.nav.lock().count();
        let (nav_idx, nav_count2) = {
            let n = self.nav.lock();
            (n.current_index(), n.count())
        };
        let current_path = self.nav.lock().current().map(|i| i.path.clone());
        let current_size = self.viewer.as_ref()
            .and_then(|v| v.lock().current_gpu.as_ref().map(|img| (img.width, img.height)));
        let zoom_pct = self.viewer.as_ref()
            .map(|v| v.lock().zoom_value() * 100.0).unwrap_or(0.0);
        // Prefer the persisted setting; fall back to the folder the user
        // launched the app with (which may not be saved yet on first run).
        let folder = self.settings.last_folder()
            .or_else(|| self.initial_folder.clone());

        // Capture image-quad background colour BEFORE the egui_state
        // mutable borrow opens (egui_state holds &mut self through to
        // flush_inbox, so self.pal() inside the render scope would
        // conflict with it).
        let pal_bg = self.pal().d2d_clear;

        let ppp = self.wgpu_state
            .as_ref()
            .map(|w| w.pixels_per_point)
            .unwrap_or(1.0)
            .max(0.1);
        let now = std::time::Instant::now();
        let dt = self.last_frame.map(|t| now.duration_since(t).as_secs_f32()).unwrap_or(1.0 / 60.0);

        FrameState {
            nav_count,
            nav_idx,
            nav_count2,
            current_path,
            current_size,
            zoom_pct,
            folder,
            pal_bg,
            actions: Vec::new(),
            dt,
            ppp,
        }
    }

    /// Block D (egui UI): build all egui panels and capture the central rect.
    /// Returns the central rect in physical pixels for `submit_wgpu_frame`.
    fn build_egui_ui(&mut self, state: &mut FrameState) -> Option<(i32, i32, u32, u32)> {
        let pal = self.pal();
        let dt = state.dt;
        let ppp = state.ppp;

        // Declared outside SCOPE 1 because the fullscreen-chrome
        // countdown in SCOPE 2 reads it. `Option<Rect>` is Copy.
        let mut toolbar_rect: Option<egui::Rect> = None;

        // SCOPE 1: all egui_state-bound code runs in this inner block so
        // its `&mut self.egui_state` borrow is released before the
        // draw_context_menu call (which takes &mut self.actions and
        // &self for self.pal()/self.nav.lock()). Re-acquired in SCOPE 2.
        //
        // Clone the egui Context (Arc-backed, cheap) HERE so SCOPE 2 and
        // draw_context_menu can pass it around without keeping the
        // &mut self.egui_state borrow alive across those calls.
        let egui_ctx: egui::Context;
        {
            let Some(egui_state) = self.egui_state.as_mut() else { return None; };
            let Some(_wgpu_state) = self.wgpu_state.as_ref() else { return None; };
            egui_ctx = egui_state.ctx.clone();

        // Sync surface size to the ACTUAL client area. The wgpu surface
        // covers the client rect only — using outer_size (which includes
        // the title bar and borders) makes DWM squash the surface and
        // desyncs egui hit-testing from the drawn UI.
        {
            // winit 0.30's `window.inner_size()` / `current_monitor().size()`
            // report the window size SCALED by the DPI factor in fullscreen
            // (e.g. 2560x1440 for a 2048x1152 monitor at 125%), which
            // desyncs the wgpu surface config from the real monitor
            // resolution and pushes the image off-centre. The most reliable
            // physical size is the window CLIENT rect (Win32), which in
            // borderless fullscreen equals the monitor resolution. Fall
            // back to inner_size() if the HWND isn't available yet.
            let fallback = self.window
                .as_ref()
                .map(|w| {
                    let s = w.inner_size();
                    (s.width.max(1), s.height.max(1))
                })
                .unwrap_or((1, 1));
            let hwnd_raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
            let phys = Self::client_area_physical_using(hwnd_raw, fallback);
            let cw = phys.0;
            let ch = phys.1;
            let wgpu_state_mut = self.wgpu_state.as_mut().expect("checked above");
            if cw != wgpu_state_mut.config.width || ch != wgpu_state_mut.config.height
            {
                wgpu_state_mut.config.width = cw;
                wgpu_state_mut.config.height = ch;
                wgpu_state_mut.surface.configure(
                    &wgpu_state_mut.device,
                    &wgpu_state_mut.config,
                );
            }
        }
        // Re-borrow immutably for the rest of the frame.
        let wgpu_state = self.wgpu_state.as_ref().unwrap();

        // Take the input events accumulated from winit since last frame
        // and fill in the screen rect + time. egui works in LOGICAL
        // points, so the physical surface size must be divided by
        // pixels_per_point.
        self.router.pixels_per_point = ppp;
        let mut raw_input = self.router.take_pending();
        raw_input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::Vec2::new(
                wgpu_state.config.width as f32 / ppp,
                wgpu_state.config.height as f32 / ppp,
            ),
        ));
        // Report the device's max texture side so egui's `load_texture`
        // debug-assert accepts our (clamped) images. Without this egui uses a
        // stale/unknown limit and asserts on a legitimately-sized texture.
        raw_input.max_texture_side = Some(wgpu_state.device.limits().max_texture_dimension_2d as usize);
        // Pass ppp to egui via ViewportInfo::native_pixels_per_point
        // instead of Context::set_pixels_per_point. Setting
        // native_pixels_per_point feeds directly into the
        // pixels_per_point computation in begin_pass (`zoom_factor *
        // native_pixels_per_point`) WITHOUT triggering the
        // zoom-factor ratio adjustment.
        //
        // That ratio adjustment is the source of a scissor-validation
        // crash on the FIRST frame: when set_pixels_per_point is
        // called before begin_pass, begin_pass sees a non-None
        // new_zoom_factor and rescales the input screen_rect by
        // `old_zoom / new_zoom`. On the first frame `old_zoom` is the
        // default 1.0 and `viewport.input.screen_rect` is still the
        // ///InputState default of (0, 0, 10000, 10000) — so the
        // adjustment produces a 8000×8000 screen_rect, blowing past
        // the 2304×1296 surface in the egui_wgpu scissor clamp.
        // Setting native_pixels_per_point sidesteps the ratio
        // adjustment entirely while still keeping egui's internal ppp
        // in sync (since ppp = zoom_factor * native_pixels_per_point
        // = 1.0 * 1.25 = 1.25 on the first frame).
        let active_viewport = raw_input
            .viewport_id;
        let mut vp_info = raw_input
            .viewports
            .get(&active_viewport)
            .cloned()
            .unwrap_or_default();
        vp_info.native_pixels_per_point = Some(ppp);
        raw_input
            .viewports
            .insert(active_viewport, vp_info);
        raw_input.time = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        );
        egui_state.ctx.begin_pass(raw_input);

        // Apply egui visuals when the theme changes (toggle or startup).
        // `System` resolves to the OS preference, so we compare the resolved
        // value to avoid re-applying identical visuals every frame.
        // `resolve_theme` is a free fn (no &self) so it can run while
        // `self.egui_state` is mutably borrowed.
        let resolved = resolve_theme(self.theme);
        if self.applied_visuals != Some(resolved) {
            self.applied_visuals = Some(resolved);
            let mut visuals = match resolved {
                Theme::Light => {
                    let mut v = egui::Visuals::light();
                    v.panel_fill = egui::Color32::from_rgb(246, 246, 246);
                    v.window_fill = v.panel_fill;
                    v.override_text_color = Some(egui::Color32::from_rgb(26, 27, 30));
                    v
                }
                Theme::Dark | Theme::System => {
                    let mut v = egui::Visuals::dark();
                    v.panel_fill = egui::Color32::from_rgb(15, 16, 17);
                    v.window_fill = v.panel_fill;
                    v.override_text_color = Some(egui::Color32::from_rgb(247, 248, 248));
                    v
                }
            };
            // Kill egui's default light-gray chrome:
            //  * panel separators (1px line between Side/Central/TopBottom
            //    panels) come from `noninteractive.bg_stroke` — transparent
            //    makes panels blend seamlessly.
            //  * `window_shadow`/`popup_shadow` give the shortcuts Window
            //    a light halo below it.
            visuals.window_stroke = egui::Stroke::NONE;
            visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(0.0_f32, egui::Color32::TRANSPARENT);
            visuals.widgets.noninteractive.bg_fill = egui::Color32::TRANSPARENT;
            visuals.window_shadow = egui::epaint::Shadow::NONE;
            visuals.popup_shadow = egui::epaint::Shadow::NONE;
            egui_ctx.set_visuals(visuals);
        }

        // Frame delta for UI animations (panel widths, chrome fade).
        let now = std::time::Instant::now();
        let _dt = self.last_frame.map(|t| now.duration_since(t).as_secs_f32()).unwrap_or(1.0 / 60.0);
        self.last_frame = Some(now);

        // ----- TITLEBAR (custom-drawn, replaces the OS frame) -----
        if !self.is_fullscreen {
            let resp = egui::TopBottomPanel::top("titlebar")
                .exact_height(TOOLBAR_HEIGHT as f32)
                .frame(
                    egui::Frame::default()
                        .fill(pal.panel_bg)
                        .inner_margin(egui::Margin::symmetric(0.0, 0.0)),
                )
                .show(&egui_state.ctx, |ui| {
                    if let Some(window) = self.window.as_ref() {
                        Self::draw_titlebar(
                            ui, &mut state.actions, window,
                            self.show_tree, self.show_thumbs,
                            &pal,
                        );
                    }
                });
            toolbar_rect = Some(resp.response.rect);
        }
        // Ease the fullscreen bottom-bar alpha/height toward its target.
        {
            let target = if self.is_fullscreen && self.chrome_visible { 1.0 } else { 0.0 };
            let k = (dt / 0.15).clamp(0.0, 1.0);
            self.chrome_anim += (target - self.chrome_anim) * k;
            if (self.chrome_anim - target).abs() < 0.01 {
                self.chrome_anim = target;
            }
        }
        if self.is_fullscreen && self.chrome_anim > 0.02 {
            // ----- OVERLAY CONTROL BAR (floats over the image) -----
            // Anchored to the window bottom as a Foreground Area so it does
            // NOT reserve layout space (which would shrink the CentralPanel
            // and leave the image with a bottom gap). It fades in/out with
            // `chrome_anim`; the image already fills the whole window, so
            // the bar overlays it semi-transparently.
            let a = self.chrome_anim;
            let bar_h = 48.0 * a;
            let screen_h = egui_state.ctx.input(|i| i.screen_rect.height());
            let bar_y = screen_h - bar_h;
            let bar_rect_outer = egui::Rect::from_min_size(
                egui::pos2(0.0, bar_y),
                egui::vec2(egui_state.ctx.input(|i| i.screen_rect.width()), bar_h),
            );
            let bar_w = egui_state.ctx.input(|i| i.screen_rect.width());
            let bar_resp = egui::Area::new(egui::Id::new("overlay_toolbar"))
                .order(egui::Order::Foreground)
                .fixed_pos(bar_rect_outer.min)
                // Give the Area its actual size so ui.max_rect() (which
                // draw_fullscreen_bar reads for the bar height) is correct
                // instead of degenerating to ~0 and clipping every button.
                .default_size(egui::vec2(bar_w, bar_h))
                .show(&egui_state.ctx, |ui| {
                    let area_rect = ui.max_rect();
                    ui.set_clip_rect(area_rect);
                    ui.allocate_rect(area_rect, egui::Sense::hover());
                    // Semi-transparent surface behind the row, over the
                    // area's own rect (local coords).
                    ui.painter().rect_filled(
                        area_rect,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(
                            pal.panel_bg.r(), pal.panel_bg.g(), pal.panel_bg.b(),
                            (235.0 * a) as u8,
                        ),
                    );
                    Self::draw_fullscreen_bar(
                        ui, &mut state.actions, &state.current_path,
                        state.nav_idx, state.nav_count2, state.current_size, state.zoom_pct,
                        &pal, true, self.slide_show_running,
                    );
                });
            toolbar_rect = Some(bar_resp.response.rect);
            self.fullscreen_bar_rect = Some(bar_resp.response.rect);
        } else {
            self.fullscreen_bar_rect = None;
        }

        // ----- SIDE PANELS (custom width controller) -----
        // Widths ease toward their targets (macOS sidebar animation);
        // the tree's content minimum is measured while drawing so the
        // panel widens for deep folders and shrinks back on collapse.
        // Phase 12: widths are snapped to WHOLE physical pixels
        // (round(anim*ppp)/ppp) before feeding egui — the D2D child
        // is positioned at round(anim*ppp), so a fractional logical
        // width left a 1px column of canvas showing between panel
        // and viewer (the gray seam near the bottom bar).
        let ppp_snap = ppp.max(0.1);
        let tree_anim = self.tree_panel.tick(self.show_tree && !self.is_fullscreen, dt);
        let thumb_anim = self.thumb_panel.tick(self.show_thumbs && !self.is_fullscreen, dt);
        let tree_anim = (tree_anim * ppp_snap).round() / ppp_snap;
        let thumb_anim = (thumb_anim * ppp_snap).round() / ppp_snap;

        // We collect any tree/thumb actions into a fresh Vec that the
        // outer scope owns. The draw helpers take &mut Vec<UiAction>
        // directly so they can queue without borrowing self.
        let mut side_actions: Vec<UiAction> = Vec::new();
        if tree_anim > 0.5 {
            let resp = egui::SidePanel::left("tree")
                .resizable(false)
                .exact_width(tree_anim)
                .frame(
                    egui::Frame::default()
                        .fill(pal.panel_bg)
                        .inner_margin(egui::Margin::same(0.0)),
                )
                .show(&egui_state.ctx, |ui| {
                    // Phase 14: pass the nav's CURRENT FOLDER, not
                    // its current image's path. ImageItem.path is
                    // a file; tree nodes are folders, so the old
                    // `current_path` made is_current() never match.
                    // Also pass the user-pinned active root so the
                    // highlight follows the row the user last
                    // picked from (a folder may live in Favorites +
                    // Recent; we only highlight the active row).
                    let current_folder = self.nav.lock().folder().cloned();
                    let min_w = Self::draw_tree_panel_static(
                        ui,
                        &self.file_tree,
                        current_folder,
                        &state.folder,
                        state.nav_count,
                        &mut side_actions,
                        &mut self.pending_tree_intent,
                        &pal,
                    );
                    self.tree_panel.content_min = min_w;
                });
            let r = resp.response.rect;
            self.tree_rect_phys = (
                r.left() * ppp, r.top() * ppp,
                r.width() * ppp, r.height() * ppp,
            );
        } else {
            self.tree_rect_phys = (0.0, 0.0, 0.0, 0.0);
        }
        if thumb_anim > 0.5 {
            let tc = self.texture_cache.clone();
            let nav_items = self.nav.lock().items().to_vec();
            let cur_idx = self.nav.lock().current_index();
            let force_scroll = cur_idx != self.last_thumb_idx;
            let resp = egui::SidePanel::right("thumbs")
                .resizable(false)
                .exact_width(thumb_anim)
                .frame(
                    egui::Frame::default()
                        .fill(pal.panel_bg)
                        .inner_margin(egui::Margin::same(0.0)),
                )
                .show(&egui_state.ctx, |ui| {
                    Self::draw_thumbs_panel_static(
                        ui,
                        &tc,
                        &nav_items,
                        cur_idx,
                        force_scroll,
                        &mut side_actions,
                        &pal,
                    );
                });
            let r = resp.response.rect;
            self.thumb_rect_phys = (
                r.left() * ppp, r.top() * ppp,
                r.width() * ppp, r.height() * ppp,
            );
            self.last_thumb_idx = cur_idx;
        } else {
            self.thumb_rect_phys = (0.0, 0.0, 0.0, 0.0);
        }
        // Merge side panel actions into the main queue.
        state.actions.extend(side_actions);

        // ----- SHORTCUT HELP (popover anchored below the "?" button) -----
        // Phase 9: rects of all popovers that need a hole punched in
        // the D2D child this frame. Collected while drawing, applied
        // once after the popup block (combined region — punching
        // twice would let the second SetWindowRgn overwrite the first).
        if self.show_shortcut_help {
            // Force a fixed-size popover at top-center, above the egui
            // CentralPanel. The image is painted as egui meshes inside the
            // CentralPanel, so an Order::Foreground popover (layer 3 in
            // egui 0.29) plus a Position above the CentralPanel draws on
            // top of both the image and the surrounding chrome.
            let screen = egui_state.ctx.input(|i| i.screen_rect);
            let popup_w = 360.0_f32.min(screen.width() - 16.0);
            let popup_h = 360.0_f32;
            // Anchor the popup to the "?" button: right-aligned to the
            // button's right edge, appearing directly below it. Falls back
            // to a centered-ish top position if the button hasn't been drawn.
            let anchor_pos = {
                let anchor = HELP_ANCHOR.with(|c| c.get());
                let valid = !(anchor.any_nan() || anchor == egui::Rect::NOTHING);
                if valid {
                    // `pivot(RIGHT_TOP)` puts the window's RIGHT-TOP corner
                    // at fixed_pos, so pass the button's right edge directly
                    // (not minus popup_w — that double-shifted it left).
                    egui::pos2(anchor.max.x, anchor.max.y + 6.0)
                } else {
                    egui::pos2(
                        (screen.width() - popup_w) * 0.5,
                        (TOOLBAR_HEIGHT as f32) + 6.0,
                    )
                }
            };
            let help_resp = egui::Window::new(
                egui::RichText::new("Keyboard Shortcuts").size(13.0).strong(),
            )
            .order(egui::Order::Foreground)
            // No default shadow/fill — the gray halo around the window
            // read as a stray background block on both themes.
            .frame(
                egui::Frame::default()
                    .fill(pal.panel_bg)
                    // Phase 14: drop the 1px stroke (same reason as
                    // the context menus) — it painted 0.5px inside
                    // the frame rect and leaked past the hole.
                    .stroke(egui::Stroke::NONE)
                    .rounding(8.0)
                    .outer_margin(egui::Margin::same(2.0))
                    .inner_margin(egui::Margin::same(12.0))
                    .shadow(egui::Shadow::NONE),
            )
            .fixed_pos(anchor_pos)
            .fixed_size(egui::vec2(popup_w, popup_h))
            .pivot(egui::Align2::RIGHT_TOP)
            .resizable(false)
            .collapsible(false)
            .fade_in(false)
            .fade_out(false)
            .show(&egui_state.ctx, |ui| {
                egui::Grid::new("shortcuts_grid")
                    .num_columns(2)
                    .spacing([32.0, 7.0])
                    .show(ui, |ui| {
                        macro_rules! shortcut {
                            ($key:expr, $desc:expr) => {
                                ui.label(
                                    egui::RichText::new($key)
                                        .monospace()
                                        .size(12.0)
                                        .color(pal.key_hint),
                                );
                                ui.label(
                                    egui::RichText::new($desc)
                                        .size(12.0)
                                        .color(pal.help_desc),
                                );
                                ui.end_row();
                            };
                        }
                        shortcut!("← → ↑ ↓", "Previous / Next image");
                        shortcut!("PgUp / PgDn", "Page through thumbnails");
                        shortcut!("Home / End", "First / Last image");
                        shortcut!("Ctrl + ←→↑↓", "Cycle related folders");
                        shortcut!("Wheel", "Zoom (50%–500%)");
                        shortcut!("Drag", "Pan image");
                        shortcut!("F / Ctrl + 0", "Fit image to window");
                        shortcut!("Ctrl + + / −", "Zoom in / out");
                        shortcut!("Ctrl + F", "Fullscreen");
                        shortcut!("Ctrl + W", "Set as wallpaper");
                        shortcut!("Ctrl + E", "Open in Explorer");
                        shortcut!("Ctrl + D", "Toggle favorite");
                        shortcut!("Ctrl + /", "Toggle this help");
                        shortcut!("Esc", "Exit fullscreen / Quit");
                    });
            });
            // Phase 9: record the popover rect for the unified hole
            // punch after the popup block, and close on any click
            // outside the window.
            //
            // Phase 10 fix: clicks on the `?` button itself are
            // EXCLUDED from clicked_elsewhere. The ? handler queues
            // UiAction::ToggleShortcutHelp this same frame; if the
            // popover also closed itself here, apply_action's toggle
            // flipped it straight back open — the panel could never
            // be dismissed via the button. With the exclusion, the
            // button path closes via the toggle action, and any
            // other outside click closes via clicked_elsewhere.
            if let Some(resp) = help_resp {
                // Click position this frame (None = no click).
                let pointer = egui_state.ctx.input(|i| i.pointer.latest_pos());
                let anchor = HELP_ANCHOR.with(|c| c.get());
                let anchor_valid = !(anchor.any_nan() || anchor == egui::Rect::NOTHING);
                let clicked_on_help_btn = match pointer {
                    Some(p) => {
                        anchor_valid
                            && egui::Rect::from_min_max(
                                anchor.min - egui::Vec2::splat(2.0),
                                anchor.max + egui::Vec2::splat(2.0),
                            )
                            .contains(p)
                    }
                    None => false,
                };
                if resp.response.clicked_elsewhere() && !clicked_on_help_btn {
                    self.show_shortcut_help = false;
                }
            }
        }
        // END SCOPE 1: the egui_state borrow on self.egui_state is
        // released here so draw_context_menu can take &mut self.actions
        // and self.pal()/self.nav.lock() can take &self without
        // conflicting.
        }

        // ----- SETTINGS DROPDOWN (Linear-style) -----
        // Opens from the ⚙ title-bar button, anchored below/right-aligned
        // to it. Offers global prefs: clear recents, clear favorites,
        // follow-system theme, and sidebar-on-launch. Uses `egui_ctx` (a
        // clone) so we can dispatch actions via &mut self without the
        // egui_state borrow.
        if self.show_settings_menu {
            let pal = self.pal();
            let theme_system = self.settings.is_theme_system();
            let sidebar_on_launch = self.settings.show_tree_on_launch();
            let anchor = SETTINGS_ANCHOR.with(|c| c.get());
            let anchor_valid = !(anchor.any_nan() || anchor == egui::Rect::NOTHING);
            // Menu self-sizes to the longest label so it isn't a wide empty
            // box: measure each item's text width (via the font's layout_job)
            // and use (widest + padding) as the window's default width.
            // Long labels never clip; short items don't stretch.
            fn text_w(ctx: &egui::Context, t: &str) -> f32 {
                let job = egui::text::LayoutJob::simple(
                    t.into(),
                    egui::FontId::proportional(12.5),
                    egui::Color32::WHITE,
                    200.0,
                );
                ctx.fonts(|f| f.layout_job(job).size().x)
            }
            let widest = [
                "Clear browse history",
                "Clear favorites",
                "Follow system theme",
                "Show sidebar on launch",
            ]
            .iter()
            .map(|t| text_w(&egui_ctx, t))
            .fold(0.0_f32, f32::max);
            let menu_w = (widest + 44.0).max(160.0);
            let pos = if anchor_valid {
                egui::pos2(anchor.max.x - menu_w, anchor.max.y + 4.0)
            } else {
                egui::pos2(16.0, (TOOLBAR_HEIGHT as f32) + 4.0)
            };
            let menu_frame = egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(
                    pal.panel_bg.r(), pal.panel_bg.g(), pal.panel_bg.b(), 235,
                ))
                .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::same(6.0))
                .shadow(egui::Shadow {
                    spread: 0.0,
                    color: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 120),
                    offset: egui::vec2(0.0, 4.0),
                    blur: 24.0,
                });
            let mut close_menu = false;
            let mut settings_action: Option<UiAction> = None;
            let mut settings_win = egui::Window::new("");
            settings_win = settings_win
                // Unique Id — otherwise an empty title gives this window the
                // same Id as the right-click context menu, and their item
                // widget Ids collide (egui 'First/Second use of widget ID'
                // warnings; clicks land on the wrong menu and never fire).
                .id(egui::Id::new("settings_menu"))
                .frame(menu_frame)
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .default_width(menu_w)
                .resizable(false)
                .collapsible(false)
                .title_bar(false);
            let make_item = |ui: &mut egui::Ui, label: &str, checked: Option<bool>| -> bool {
                let row_h = 28.0_f32;
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), row_h),
                    egui::Sense::click(),
                );
                if resp.is_pointer_button_down_on() {
                    // Pressed — strong accent fill for clear click feedback.
                    ui.painter().rect_filled(rect, 6.0, pal.button_hover);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 6.0, pal.hover_fill);
                }
                ui.painter().text(
                    egui::pos2(rect.left() + 8.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(12.5),
                    pal.text_secondary,
                );
                if let Some(chk) = checked {
                    ui.painter().text(
                        egui::pos2(rect.right() - 8.0, rect.center().y),
                        egui::Align2::RIGHT_CENTER,
                        if chk { "✓" } else { "" },
                        egui::FontId::proportional(12.5),
                        pal.accent,
                    );
                }
                resp.clicked()
            };
            let settings_resp = settings_win.show(&egui_ctx, |ui| {
                if make_item(ui, "Clear browse history", None) {
                    settings_action = Some(UiAction::SettingsClearRecent);
                    close_menu = true;
                }
                if make_item(ui, "Clear favorites", None) {
                    settings_action = Some(UiAction::SettingsClearFavorites);
                    close_menu = true;
                }
                ui.separator();
                if make_item(ui, "Follow system theme", Some(theme_system)) {
                    settings_action = Some(UiAction::SettingsThemeSystem);
                    close_menu = true;
                }
                if make_item(ui, "Show sidebar on launch", Some(sidebar_on_launch)) {
                    settings_action = Some(UiAction::SettingsSidebarOnLaunch);
                    close_menu = true;
                }
            });
            // Close on click-elsewhere (e.g. clicking the image / a menu
            // outside), mirroring the shortcut-help and context menus. This
            // also stops the settings menu lingering while a right-click
            // context menu opens on top.
            if settings_resp.map_or(false, |ir| ir.response.clicked_elsewhere()) {
                self.show_settings_menu = false;
            }
            if close_menu {
                self.show_settings_menu = false;
            }
            if let Some(act) = settings_action {
                self.apply_action(act);
            }
        }

        // ----- RIGHT-CLICK CONTEXT MENU (egui-drawn, Linear theme) -----
        // Replaces the native Win32 TrackPopupMenu. Replacing
        // `self.ctx_menu` on each right-click auto-closes any previous
        // popup (the next frame redraws at the new position). Clicking
        // a menu item dispatches its UiAction and closes the popup;
        // clicking elsewhere closes it permanently (next right-click
        // re-opens with fresh state).
        //
        // We extract everything the menu needs (current image path,
        // palette, open-menu variant) and pass it to a free function
        // so this call doesn't conflict with the egui_state borrow
        // (already released in SCOPE 1).
        // ----- RIGHT-CLICK CONTEXT MENU (egui-drawn, Linear theme) -----
        // Replaces the native Win32 TrackPopupMenu. The menu is drawn
        // every frame while `ctx_menu` is Some. A new right-click simply
        // replaces `ctx_menu` (closing any old popup). draw_context_menu
        // returns whether to keep it open; if it returns false (menu item
        // clicked, or an outside click after the open grace window) we
        // clear `ctx_menu`. `ctx_menu_open_at` is set when a menu opens so
        // the right-button-down that OPENED it isn't treated as an outside
        // click on the first frame.
        if let Some(menu) = self.ctx_menu.as_ref() {
            let pal = self.pal();
            let current_path = self.nav.lock().current().map(|i| i.path.clone());
            let keep_open = Self::draw_context_menu(
                &egui_ctx,
                menu,
                &pal,
                current_path.as_deref(),
                &mut self.actions,
                self.ctx_menu_open_at,
            );
            if !keep_open {
                self.ctx_menu = None;
                self.ctx_menu_open_at = None;
            }
        }

        // SCOPE 2: re-acquire egui_state for the remaining panels
        // (status bar, fullscreen chrome, central panel) and the
        // pass-ending calls (end_pass / tessellate). The egui pass
        // opened in SCOPE 1 is still active — we just continue using
        // the same egui::Context (`egui_ctx` outside this scope
        // aliases the same Arc).
        let Some(egui_state) = self.egui_state.as_mut() else { return None; };
        let Some(wgpu_state) = self.wgpu_state.as_ref() else { return None; };

        // ----- STATUS BAR -----
        // Same three-zone layout as the fullscreen bar (filename left /
        // controls center / info + help right) for a consistent UI.
        if !self.is_fullscreen {
            egui::TopBottomPanel::bottom("statusbar")
                // Phase 9: explicit zero-margin frame — the default
                // panel frame's inner margin ate into the 48px bar
                // height, leaving the button row top-anchored and
                // visually clipped by the bar edge.
                .frame(
                    egui::Frame::default()
                        .fill(pal.panel_bg)
                        .inner_margin(egui::Margin::symmetric(0.0, 0.0)),
                )
                .exact_height(STATUS_BAR_HEIGHT as f32)
                .show(&egui_state.ctx, |ui| {
                    Self::draw_fullscreen_bar(
                        ui, &mut state.actions, &state.current_path,
                        state.nav_idx, state.nav_count2, state.current_size, state.zoom_pct,
                        &pal, false, self.slide_show_running,
                    );
                });
        }

        // ----- Fullscreen chrome countdown -----
        // Hovering the overlay toolbar pauses the timer; 2.5s of idle
        // elsewhere hides it again.
        if self.is_fullscreen && self.chrome_visible {
            let pointer = egui_state.ctx.input(|i| i.pointer.latest_pos());
            let hovering = pointer.map_or(false, |p| {
                toolbar_rect.map_or(false, |r| r.contains(p))
            });
            if hovering {
                self.chrome_hide_at = Some(std::time::Instant::now());
            } else if let Some(t) = self.chrome_hide_at {
                if t.elapsed() >= std::time::Duration::from_millis(2500) {
                    self.chrome_visible = false;
                    if let Some(w) = &self.window { w.request_redraw(); }
                }
            }
        }

        // Reserve the central area; the D2D viewer child HWND lives here.
        // We do NOT paint a background fill — that would cover the child.
        // We capture the CentralPanel's rect to position the D2D child
        // exactly underneath it (single source of truth = egui layout).
        //
        // Pre-extract the edge-drawer inputs into locals so the CentralPanel
        // closure only captures these (never borrows *self) — otherwise the
        // closure would need `&mut self` while `egui_state` (self.egui_state)
        // is mutably borrowed, which the borrow checker rejects.
        let drawer_state = self.drawer_draw;
        // The drawer's effective width comes straight from `tick_drawer`'s
        // returned `width` (the eased value = max(user_width, content_min) *
        // frac). Using THAT (not a recomputation) keeps drawing, hit-test and
        // wheel routing on the exact same width — the old bug rendered wider
        // than the hit-test width, leaving the right portion click-through.
        let drawer_eff_width = drawer_state.map(|(_f, w, _t, _h)| w).unwrap_or(0.0);
        let drawer_anim = self.drawer_anim;
        let drawer_handle_light = self.drawer_handle_light;
        let cursor_physical = self.router.cursor_pos;
        let drawer_pal = pal;
        let drawer_file_tree = &self.file_tree;
        let drawer_pending = &mut self.pending_tree_intent;
        let drawer_nav_folder = self.nav.lock().folder().cloned();
        let drawer_nav_count = state.nav_count;
        // Accumulates the drawer content's widest row (for auto-widening).
        // Written inside the closure (Cell, no self borrow); applied to
        // `self.drawer_user_width` after `_center`.
        let drawer_content_w = std::cell::Cell::new(0.0_f32);
        // The image is painted as egui meshes inside the CentralPanel
        // closure. Extract a clone of the viewer Arc so the closure doesn't
        // borrow `self` (which is already mutably borrowed via egui_state).
        let image_viewer = self.viewer.clone();

        let mut central_rect_phys: Option<(i32, i32, u32, u32)> = None;
        let _center = egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(&egui_state.ctx, |ui| {
                let r = ui.max_rect();
                let ppp = egui_state.ctx.pixels_per_point();
                central_rect_phys = Some((
                    (r.min.x * ppp).round() as i32,
                    (r.min.y * ppp).round() as i32,
                    (r.width() * ppp).round() as u32,
                    (r.height() * ppp).round() as u32,
                ));

                // Paint the viewer background behind the image. The surface
                // is also cleared to `canvas_clear` before the egui pass,
                // but the CentralPanel's `Frame::none()` leaves the central
                // area un-painted, so fill it with the viewer letterbox to
                // avoid the background showing the panel chrome color.
                let dc = pal.d2d_clear;
                ui.painter().rect_filled(
                    r,
                    0.0,
                    egui::Color32::from_rgb(
                        (dc[0] * 255.0) as u8,
                        (dc[1] * 255.0) as u8,
                        (dc[2] * 255.0) as u8,
                    ),
                );

                // Paint the current image (and any outgoing slide frame) as
                // egui meshes, clipped to the central panel rect. Clipping
                // keeps the sliding-out image from leaking over the tree /
                // thumb panels, matching the old scissor behavior.
                if let Some(viewer) = &image_viewer {
                    let painter = ui.painter().with_clip_rect(r);
                    viewer.lock().paint_viewer(&painter, ppp);
                }

                // ----- Panel edge highlight -----
                // A 2px accent line on the draggable edge when hovered
                // or actively dragged (confirmation affordance).
                let active_edge = self.drag_panel.or(self.panel_edge_hover);
                if let Some(edge) = active_edge {
                    let line_rect = match edge {
                        0 => {
                            let (x, y, w, h) = self.tree_rect_phys;
                            if w > 0.0 {
                                Some(egui::Rect::from_min_size(
                                    egui::pos2((x + w - 1.5) / ppp, y / ppp),
                                    egui::vec2(3.0 / ppp, h / ppp),
                                ))
                            } else { None }
                        }
                        1 => {
                            let (x, y, w, h) = self.thumb_rect_phys;
                            if w > 0.0 {
                                Some(egui::Rect::from_min_size(
                                    egui::pos2((x - 1.5) / ppp, y / ppp),
                                    egui::vec2(3.0 / ppp, h / ppp),
                                ))
                            } else { None }
                        }
                        _ => None,
                    };
                    if let Some(lr) = line_rect {
                        ui.painter().rect_filled(
                            lr,
                            0.0,
                            egui::Color32::from_rgb(0x71, 0x70, 0xff),
                        );
                    }
                }

                // ----- EDGE DRAWER (left-edge auto-expand tree menu) -----
                // Overlays the viewer (never affects layout). Drawn HERE,
                // inside the CentralPanel frame (before `end_pass()`) so the
                // widgets participate in egui hit-testing — drawing after
                // `end_pass()` made them visible but non-interactive. Uses
                // only pre-extracted locals (no `*self` capture) so the
                // closure can co-exist with the `egui_state` borrow.
                if let Some((frac, _width, top, height)) = drawer_state {
                    let valid = top.is_finite() && height.is_finite()
                        && drawer_eff_width.is_finite() && frac.is_finite();
                    if valid {
                        let nav_folder = drawer_nav_folder;
                        let visible_body = frac > 0.001 || drawer_anim > 0.001;
                        let fixed_rect = egui::Rect::from_min_size(
                            egui::pos2(0.0, top),
                            egui::vec2(drawer_eff_width, height),
                        );
                        if visible_body {
                            let slide_x = -drawer_eff_width * (1.0 - frac);
                            let body_rect = fixed_rect.translate(egui::vec2(slide_x, 0.0));
                            if body_rect.max.x > 0.0 {
                                let file_tree = drawer_file_tree;
                                let pending = drawer_pending;
                                let actions = &mut state.actions;
                                let ctx = ui.ctx().clone();
                                egui::Area::new(egui::Id::new("edge_drawer"))
                                    .constrain(false)
                                    .order(egui::Order::Foreground)
                                    .default_size(fixed_rect.size())
                                    .fixed_pos(body_rect.min)
                                    .show(&ctx, |ui| {
                                        ui.set_clip_rect(fixed_rect);
                                        ui.set_max_size(fixed_rect.size());
                                        let frame = egui::Frame::default()
                                            .fill(drawer_pal.panel_bg)
                                            .inner_margin(egui::Margin::same(0.0))
                                            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(90)));
                                        frame.show(ui, |ui| {
                                            let min_w = Self::draw_tree_panel_static(
                                                ui,
                                                file_tree,
                                                nav_folder,
                                                &state.folder,
                                                drawer_nav_count,
                                                actions,
                                                pending,
                                                &drawer_pal,
                                            );
                                            drawer_content_w.set(min_w.max(drawer_content_w.get()));
                                        });
                                    });
                            }
                        }
                        // Handle: thin translucent pill at the left edge.
                        {
                            let handle_w = 5.0_f32;
                            let handle_h = 48.0_f32;
                            let handle_color = if drawer_handle_light {
                                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 0x33)
                            } else {
                                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 0x33)
                            };
                            let hrect = egui::Rect::from_min_size(
                                egui::pos2(HANDLE_X, top + height * 0.5 - handle_h * 0.5),
                                egui::vec2(handle_w, handle_h),
                            );
                            let ppp = wgpu_state.pixels_per_point.max(0.1);
                            let cur = egui::pos2(
                                cursor_physical.x / ppp,
                                cursor_physical.y / ppp,
                            );
                            let hovered = hrect.expand(6.0).contains(cur);
                            let ctx = ui.ctx().clone();
                            let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new("edge_handle_painter"));
                            ctx.layer_painter(layer).rect_filled(
                                hrect,
                                2.5,
                                if hovered { handle_color.gamma_multiply(1.6) } else { handle_color },
                            );
                        }
                    }
                }
            });
        // Apply the drawer's content-driven width now that the frame is
        // built. This is `drawer_content_min`, a TEMPORARY content-minimum —
        // it widens the drawer for long rows and shrinks back to
        // `drawer_user_width` when they collapse (mirroring the classic
        // tree's `PanelWidth.content_min`). The base never grows on its own.
        self.drawer_content_min = drawer_content_w.get();        // ----- egui → wgpu -----
        let full_output = egui_state.ctx.end_pass();
        let textures_delta = full_output.textures_delta;
        let paint_jobs = egui_state.ctx.tessellate(full_output.shapes, full_output.pixels_per_point);

        // Apply the captured CentralPanel rect to update viewport dimensions.
        if let Some((px, py, pw, ph)) = central_rect_phys {
            // Clamp to the surface — the first frame's layout can be
            // garbage (huge available_rect before fonts settle).
            let px = px.clamp(0, wgpu_state.config.width as i32);
            let py = py.clamp(0, wgpu_state.config.height as i32);
            let pw = pw.min(wgpu_state.config.width.saturating_sub(px as u32));
            let ph = ph.min(wgpu_state.config.height.saturating_sub(py as u32));
            if pw >= 1 && ph >= 1 {
                self.viewport_w = pw;
                self.viewport_h = ph;
            }
        }

        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [wgpu_state.config.width, wgpu_state.config.height],
            pixels_per_point: wgpu_state.pixels_per_point.max(0.1),
        };

        // All egui frame building is done. Store the output data so
        // submit_wgpu_frame can perform the actual wgpu rendering.
        // We intentionally do NOT touch the surface/encoder here.
        self._pending_egui_output = Some(PendingEguiOutput {
            textures_delta,
            paint_jobs,
            screen_descriptor,
        });

        // Sync the viewer's viewport to the EGUI-COMPUTED central rect
        // (single source of truth = the egui layout this frame). Only
        // mutates viewport fields — does NOT call compute_fit, so the
        // user's current pan/zoom/fit is preserved across panel
        // animation changes (tree/thumb width easing).
        //
        // We feed CONTINUOUS panel widths (not the quantized
        // `central_rect_phys` integers) so that during a panel collapse
        // the image re-fits against the smooth animated edge every frame
        // instead of "pausing then jumping" on the quantized integer the
        // moment the easing decelerates below a pixel. The integer rect is
        // still used for the image-quad scissor in submit_wgpu_frame.
        if let Some(viewer) = &self.viewer {
            let (vx, vy, vw, vh) = self.viewer_rect_physical_continuous(
                wgpu_state.config.width,
                wgpu_state.config.height,
            );
            viewer.lock().set_viewport_continuous(vw, vh, vx, vy);
        }
        // END SCOPE 2: egui_state borrow released.

        central_rect_phys
    }

    /// Submit the wgpu frame: egui render pass, submit, present, thumbnail
    /// flush. All surface/encoder/renderpass work lives here. The central
    /// rect is applied to the viewport in `build_egui_ui` before this is
    /// called; it is not needed by the render itself (the image is painted
    /// as egui meshes).
    #[allow(unused_variables)]
    fn submit_wgpu_frame(
        &mut self,
        central_rect_phys: Option<(i32, i32, u32, u32)>,
        state: &FrameState,
    ) -> Result<()> {
        let pal = self.pal();
        let pal_bg = state.pal_bg;

        let Some(egui_state) = self.egui_state.as_mut() else { return Ok(()); };
        let Some(wgpu_state) = self.wgpu_state.as_ref() else { return Ok(()); };

        let Some(pending) = self._pending_egui_output.take() else { return Ok(()) };
        let PendingEguiOutput { textures_delta, paint_jobs, screen_descriptor } = pending;

        let surface_texture = wgpu_state.surface.get_current_texture()?;
        let view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Apply egui texture deltas.
        for (id, delta) in &textures_delta.set {
            egui_state.renderer.update_texture(&wgpu_state.device, &wgpu_state.queue, *id, delta);
        }
        for id in &textures_delta.free {
            egui_state.renderer.free_texture(id);
        }

        let mut encoder = wgpu_state.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("egui") });

        {
            let _cmds = egui_state.renderer.update_buffers(
                &wgpu_state.device,
                &wgpu_state.queue,
                &mut encoder,
                &paint_jobs,
                &screen_descriptor,
            );
            drop(_cmds);
        }

        // Canvas color matches the panel background (#0f1011) — no
        // gray seams where egui doesn't paint.
        //
        // On an sRGB-encoded surface, wgpu encodes the clear value
        // through the surface's linear→sRGB encoder on store; passing
        // an sRGB fraction (0..1) over-brightens the cleared region
        // relative to the authored palette. Convert to linear first.
        //
        // On a *linear* surface the GPU does NOT do that encoding on
        // store, so the sRGB-encoded palette value must be passed
        // through raw (no srgb_to_linear). The fix is keyed on
        // `surface_is_srgb` so a DX12 driver that reports only linear
        // formats doesn't render the cleared region too dark — the
        // "black" half of the user's white+black launch flash.
        let clear_color = if wgpu_state.surface_is_srgb {
            wgpu::Color {
                r: srgb_to_linear(pal.canvas_clear.0),
                g: srgb_to_linear(pal.canvas_clear.1),
                b: srgb_to_linear(pal.canvas_clear.2),
                a: 1.0,
            }
        } else {
            wgpu::Color {
                r: pal.canvas_clear.0,
                g: pal.canvas_clear.1,
                b: pal.canvas_clear.2,
                a: 1.0,
            }
        };
        // The image is now painted as egui meshes (in `build_egui_ui`,
        // inside the CentralPanel), so the surface is cleared and fully
        // rendered by the single egui pass below. The egui background
        // (`Frames` fill the panels) plus the image meshes together cover
        // the whole surface — no separate image-quad pass is needed.
        //
        // egui's fragment targets use `LoadOp::Clear(clear_color)` when
        // this is the only pass: the clear color is `canvas_clear`, which
        // fills the areas not covered by any panel frame (and the viewer
        // background is painted as a rect_filled in the CentralPanel).
        let rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("egui"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear_color),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        egui_state.renderer.render(
            &mut rpass.forget_lifetime(),
            &paint_jobs,
            &screen_descriptor,
        );

        let _ = wgpu_state.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();

        // Non-blocking reap: lets wgpu-core actually free resources that
        // were marked destroyed in prior frames (the defer-to-maintain model).
        // Without this, destroyed textures accumulate and — combined with a
        // texture freed while its submit is in-flight — would surface as
        // validation errors at the next Queue::submit. Poll is pure-poll: it
        // never waits on the GPU, so it costs nothing.
        wgpu_state.device.poll(wgpu::Maintain::Poll);

        // Advance the "safe to release" watermark. A retired image (tagged
        // with the epoch it was last drawn in) is only released after we have
        // committed `RETIRE_FRAME_BUDGET` frames past it, by which point —
        // given Fifo + frame-latency-1 caps the GPU at ~1 frame behind, plus
        // the non-blocking poll above reaping every frame — the texture is
        // guaranteed no longer sampled by any in-flight submit. This is purely
        // frame-based and NEVER blocks the render thread (wgpu 22 offers no
        // non-blocking "which submission completed" query).
        self.safe_release_epoch = self.render_epoch.saturating_sub(RETIRE_FRAME_BUDGET);
        if let Some(viewer) = &self.viewer {
            // Scope the lock so the guard is dropped before any other work.
            let safe = self.safe_release_epoch;
            viewer.lock().release_retired_through(safe);
        }

        // Drain any completed thumbnail decodes from background
        // threads and upload them as egui textures.
        let _ = self.texture_cache.flush_inbox(&egui_state.ctx);

        Ok(())
    }

    /// Block E: apply deferred UI actions + drain tree right-click intent.
    fn drain_frame_actions(&mut self, actions: Vec<UiAction>) {
        // Apply deferred UI actions (after egui borrows are released).
        // Include any actions queued directly from the keyboard handler.
        let mut all_actions = actions;
        all_actions.extend(std::mem::take(&mut self.actions));
        for action in all_actions {
            self.apply_action(action);
        }

        // Phase 16: drain any tree right-click intent recorded during
        // the egui frame. The egui closure can't call
        // open_tree_context_menu (needs &mut self while the frame
        // holds it), so it writes to pending_tree_intent; we open the
        // egui context menu here, once the egui borrows are released.
        if let Some(intent) = self.pending_tree_intent.take() {
            let is_fav = self.settings.favorite_folders().iter().any(|f| f == &intent.path);
            // intent.pos_phys holds egui LOGICAL points (the router
            // position is already logical; egui menus want logical).
            let pos = egui::pos2(intent.pos_phys.0 as f32, intent.pos_phys.1 as f32);
            self.ctx_menu = Some(CtxMenu::Tree {
                pos,
                path: intent.path,
                root_idx: intent.root_idx,
                is_favorite: is_fav,
            });
            self.ctx_menu_open_at = Some(std::time::Instant::now());
            if let Some(es) = self.egui_state.as_ref() {
                es.ctx.request_repaint();
            }
        }
    }

    /// Fullscreen overlay control bar (auto-hides; slides up from the
    /// Phase 8: bottom-bar layout, redesigned.
    /// Buttons on the LEFT (left-aligned, fixed order per the spec):
    ///   `后退 | 前进 | 适应/1:1 | ⏵/⏸ | ↻ | ⛶`
    /// Filename (or zoom% · resolution) on the RIGHT.
    /// The egui `vertical_centered` helper doesn't actually center
    /// vertically (it uses `Layout::top_down(Align::Center)`), so
    /// the previous center-group layout always looked top-anchored.
    /// Left-aligning the buttons + right-aligning the info is
    /// simpler, looks more deliberate, and matches the user's
    /// request to skip the fragile centering logic.
    // Fullscreen flag: true when running as the immersive top bar.
    #[allow(clippy::too_many_arguments)]
    fn draw_fullscreen_bar(
        ui: &mut egui::Ui,
        actions: &mut Vec<UiAction>,
        current_path: &Option<PathBuf>,
        nav_idx: usize,
        nav_count: usize,
        current_size: Option<(u32, u32)>,
        zoom_pct: f32,
        pal: &Palette,
        fullscreen: bool,
        slide_show_running: bool,
    ) {
        let _ = nav_count;
        let _ = nav_idx;
        let _ = fullscreen;
        let bar = ui.max_rect();
        // Accent-filled buttons always use white text (both themes).
        let nav_btn = |ui: &mut egui::Ui, label: &str| -> bool {
            ui.add(
                egui::Button::new(
                    egui::RichText::new(label).size(13.0).strong().color(egui::Color32::WHITE),
                )
                .fill(pal.accent)
                .min_size(egui::vec2(0.0, 30.0))
                .rounding(6.0),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        };
        // Neutral buttons: theme fill + subtle stroke so they read
        // on both dark and light backgrounds.
        let neutral_btn = |ui: &mut egui::Ui, label: &str| -> bool {
            ui.add(
                egui::Button::new(egui::RichText::new(label).size(13.0).strong())
                    .fill(pal.button_fill)
                    .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                    .min_size(egui::vec2(0.0, 30.0))
                    .rounding(6.0),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        };

        // LEFT GROUP: navigation + view controls. Phase 9 fix: build
        // the row over the FULL bar rect via new_child (same pattern
        // that fixed the titlebar). Plain ui.horizontal() inherits
        // whatever interior height the panel's default margins leave,
        // which squeezed the buttons against the bar's top edge;
        // max_rect(bar) + Align::Center guarantees true vertical
        // centering regardless of panel chrome.
        let mut left_row = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(bar)
                .id_salt("fs-bar-left-row")
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        left_row.add_space(14.0);
        if nav_btn(&mut left_row, "后退") { actions.push(UiAction::Prev); }
        if nav_btn(&mut left_row, "前进") { actions.push(UiAction::Next); }
        left_row.add_space(8.0);
        left_row.add(egui::Separator::default().vertical().spacing(8.0));
        left_row.add_space(8.0);
        if neutral_btn(&mut left_row, "适应/1:1") { actions.push(UiAction::FitOrOriginal); }
        left_row.add_space(4.0);
        let slide_glyph = if slide_show_running { "⏸" } else { "⏵" };
        if neutral_btn(&mut left_row, slide_glyph) { actions.push(UiAction::ToggleSlideShow); }
        left_row.add_space(4.0);
        if neutral_btn(&mut left_row, "↻") { actions.push(UiAction::RotateImage(1)); }
        left_row.add_space(8.0);
        left_row.add(egui::Separator::default().vertical().spacing(8.0));
        left_row.add_space(8.0);
        if neutral_btn(&mut left_row, "⛶") { actions.push(UiAction::ToggleFullscreen); }

        // RIGHT GROUP: filename (when there's a current image) OR
        // zoom% · resolution. Right-aligned, fixed at the right
        // edge of the bar. Uses a separate horizontal layout with
        // a max_rect anchored to the right side.
        let right_rect = egui::Rect::from_min_max(
            egui::pos2(bar.right() - 360.0, bar.top()),
            egui::pos2(bar.right() - 14.0, bar.bottom()),
        );
        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(right_rect)
                .id_salt("fs-bar-right")
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        if let Some(p) = current_path {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "—".into());
            right.add(
                egui::Label::new(
                    egui::RichText::new(name)
                        .size(13.0)
                        .strong()
                        .color(pal.text_secondary),
                )
                .truncate(),
            );
            right.add_space(12.0);
        }
        if let Some((w, h)) = current_size {
            right.label(
                egui::RichText::new(format!("{:.0}%  ·  {}x{}", zoom_pct, w, h))
                    .size(12.5)
                    .color(pal.text_tertiary),
            );
        }
    }

    /// Custom-drawn titlebar: drag area + sidebar toggles + centered app
    /// title + vector-drawn window controls. Replaces the native OS frame.
    #[allow(clippy::too_many_arguments)]
    fn draw_titlebar(
        ui: &mut egui::Ui,
        actions: &mut Vec<UiAction>,
        window: &Window,
        show_tree: bool,
        show_thumbs: bool,
        pal: &Palette,
    ) {
        // Whole-bar drag/double-click hit area FIRST, so interactive
        // widgets drawn on top steal their own events. allocate_rect
        // advances the layout cursor, so the visible content is drawn in a
        // child ui anchored to the same rect.
        let bar_rect = ui.max_rect();
        let bar = ui.allocate_rect(bar_rect, egui::Sense::click_and_drag());
        // Phase 11 fix: fire drag_window ONCE per press (drag_started),
        // not every frame of the drag. winit's drag_window releases
        // mouse capture and enters Windows' MODAL SC_MOVE loop; calling
        // it again every frame while that loop (or its aftermath) is
        // still winding down made re-entry fail probabilistically —
        // most often with the window parked at the screen's bottom-
        // right corner near the taskbar — leaving the window
        // un-draggable until a resize reset the capture state.
        if bar.drag_started() {
            let _ = window.drag_window();
        }
        if bar.double_clicked() {
            actions.push(UiAction::ToggleMaximize);
        }

        // Explicit horizontal layout with vertical centering — egui's
        // `horizontal()` helper centers against CONTENT height, not the
        // container, which pushed buttons to the top edge.
        let mut content = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(bar_rect)
                .id_salt("titlebar_content")
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        {
            content.add_space(12.0);

            // Sidebar toggle buttons (macOS-style, active state tinted —
            // accent fill always gets white text).
            let toggle_btn = |ui: &mut egui::Ui, label: &str, active: bool| -> bool {
                let fill = if active {
                    pal.accent
                } else {
                    pal.button_fill
                };
                let text = if active {
                    egui::Color32::WHITE
                } else {
                    pal.text_secondary
                };
                ui.add(
                    egui::Button::new(egui::RichText::new(label).size(13.0).strong().color(text))
                        .fill(fill)
                        .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                        .min_size(egui::vec2(56.0, 30.0))
                        .rounding(6.0),
                ).clicked()
            };
            if content.add(
                egui::Button::new(egui::RichText::new("Open Folder").size(13.0).strong())
                    .fill(pal.button_fill)
                    .min_size(egui::vec2(0.0, 30.0))
                    .rounding(6.0),
            ).clicked() {
                actions.push(UiAction::OpenFolder);
            }
            content.add_space(6.0);
            if toggle_btn(&mut content, "Tree", show_tree) {
                actions.push(UiAction::ToggleTree);
            }
            content.add_space(4.0);
            if toggle_btn(&mut content, "Thumbs", show_thumbs) {
                actions.push(UiAction::ToggleThumbs);
            }

            // Window controls, right-aligned, vertically centered.
            content.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(2.0);
                if Self::window_control(ui, "close-btn", WindowGlyph::Close, true, pal) {
                    actions.push(UiAction::ExitApp);
                }
                if Self::window_control(ui, "max-btn", WindowGlyph::Maximize, false, pal) {
                    actions.push(UiAction::ToggleMaximize);
                }
                if Self::window_control(ui, "min-btn", WindowGlyph::Minimize, false, pal) {
                    actions.push(UiAction::MinimizeWindow);
                }
                // Settings dropdown button (⚙). Opens a Linear-style
                // menu anchored below it for global prefs: clear recents,
                // clear favorites, follow-system theme, and sidebar-on-
                // launch toggle.
                let (srect, sresp) = ui.allocate_exact_size(
                    egui::vec2(32.0, 30.0),
                    egui::Sense::click(),
                );
                if sresp.hovered() {
                    ui.painter().rect_filled(srect, 6.0, pal.hover_fill);
                }
                SETTINGS_ANCHOR.with(|c| c.set(srect));
                if sresp.clicked() {
                    actions.push(UiAction::ToggleSettings);
                }
                let sc = srect.center();
                let p = ui.painter();
                // Simple gear: three short radial spokes around a hub.
                let stroke = egui::Stroke::new(1.3_f32, pal.text_secondary);
                for ang in [0.0_f32, std::f32::consts::FRAC_PI_3, 2.0 * std::f32::consts::FRAC_PI_3] {
                    let dir = egui::vec2(ang.cos(), ang.sin());
                    p.line_segment(
                        [sc - dir * 6.0, sc + dir * 6.0],
                        stroke,
                    );
                }
                p.circle_stroke(sc, 4.2, stroke);
                // Phase 4: theme toggle — modernised "circle + offset
                // dot" glyph (was the half-moon painter-drawn in
                // earlier commits). Renders as a small filled disc
                // with a smaller disc punching out an asymmetric
                // crescent — reads as a sun/moon icon without the
                // heavy outline of the previous half-moon, fitting
                // the Linear minimal language. Behaviour unchanged.
                let (trect, tresp) = ui.allocate_exact_size(
                    egui::vec2(32.0, 30.0),
                    egui::Sense::click(),
                );
                if tresp.hovered() {
                    ui.painter().rect_filled(trect, 6.0, pal.hover_fill);
                }
                let tc = trect.center();
                let p = ui.painter();
                p.circle_filled(tc, 7.0, pal.text_secondary);
                p.circle_filled(
                    tc + egui::vec2(3.5, -3.5),
                    6.0,
                    pal.panel_bg,
                );
                if tresp.clicked() {
                    actions.push(UiAction::ToggleTheme);
                }
                // Phase 4: `?` keyboard-shortcuts button. Lives in
                // the title bar (next to the theme toggle) per the
                // spec — always reachable regardless of whether
                // the bottom bar is collapsed. Renders as a 32x30
                // chip with a `?` glyph. Click → UiAction::
                // ToggleShortcutHelp, which the same handler as
                // the bottom-bar `?` button dispatches.
                let (hrect, hresp) = ui.allocate_exact_size(
                    egui::vec2(32.0, 30.0),
                    egui::Sense::click(),
                );
                if hresp.hovered() {
                    ui.painter().rect_filled(hrect, 6.0, pal.hover_fill);
                }
                ui.painter().text(
                    hrect.center(),
                    egui::Align2::CENTER_CENTER,
                    "?",
                    egui::FontId::proportional(15.0),
                    pal.text_secondary,
                );
                // Phase 8: write HELP_ANCHOR here so the shortcut
                // help popover anchors to the TOP-BAR `?` button
                // (its current home). Without this, the popover
                // falls back to "above the status bar's right side"
                // (line 1165), which puts it at the bottom of the
                // screen even though the button the user clicked is
                // at the top.
                HELP_ANCHOR.with(|c| c.set(hrect));
                if hresp.clicked() {
                    actions.push(UiAction::ToggleShortcutHelp);
                }
            });
        }

        // Centered app title (absolute painter — never disturbs layout).
        ui.painter().text(
            bar_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Aperture Neo Turbo",
            egui::FontId::proportional(15.0),
            pal.text_tertiary,
        );
    }

    /// One caption button with a vector-drawn glyph (immune to font
    /// fallback issues). Returns true when clicked.
    fn window_control(ui: &mut egui::Ui, _id: &str, glyph: WindowGlyph, danger: bool, pal: &Palette) -> bool {
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(42.0, TOOLBAR_HEIGHT as f32),
            egui::Sense::click(),
        );
        if resp.hovered() {
            let bg = if danger {
                egui::Color32::from_rgb(196, 43, 28)
            } else {
                pal.hover_fill
            };
            ui.painter().rect_filled(rect, 6.0, bg);
        }
        let p = ui.painter();
        let c = rect.center();
        let stroke = egui::Stroke::new(1.3_f32, pal.text_secondary);
        match glyph {
            WindowGlyph::Minimize => {
                p.line_segment([egui::pos2(c.x - 5.0, c.y), egui::pos2(c.x + 5.0, c.y)], stroke);
            }
            WindowGlyph::Maximize => {
                p.rect_stroke(
                    egui::Rect::from_center_size(c, egui::vec2(11.0, 11.0)),
                    1.0,
                    stroke,
                );
            }
            WindowGlyph::Close => {
                p.line_segment(
                    [c - egui::vec2(4.5, 4.5), c + egui::vec2(4.5, 4.5)],
                    stroke,
                );
                p.line_segment(
                    [c + egui::vec2(4.5, -4.5), c - egui::vec2(4.5, -4.5)],
                    stroke,
                );
            }
        }
        resp.clicked()
    }

    /// Draw the folder tree. Returns the content-driven minimum width
    /// (logical px) so the panel can widen for deep/long entries and
    /// shrink back when they collapse.
    ///
    /// `pending` is an out-param for the deferred native context menu:
    /// when a node is right-clicked, `draw_tree_node` writes the path
    /// + cursor + root + depth to this slot. The caller (which has
    /// access to `self.pending_tree_menu`) drains the slot after the
    /// egui frame ends and calls `show_tree_context_menu`. Passing
    /// the field by `&mut` here is the bug fix that makes right-click
    /// on tree nodes actually work — the previous code used a local
    /// `Option` inside the egui closure which was dropped before
    /// the drain step, silently losing every right-click.
    ///
    /// `pending_ctx` is an out-param for the deferred tree context
    /// menu: when a node is right-clicked, the egui closure writes
    /// a `TreeCtxMenu` (path + cursor + root_idx + depth) to this
    /// slot. The closure runs inside the egui frame which holds
    /// &mut self, so we can't call `open_tree_context_menu` from
    /// here directly; instead we propagate the right-click to
    /// `self.tree_ctx_menu` via this out-param. Phase 8.
    fn draw_tree_panel_static(
        ui: &mut egui::Ui,
        tree: &crate::file_tree::FileTree,
        // Phase 14: the nav's CURRENT FOLDER, used by the tree for
        // highlighting the active row. (Was previously `current_path`
        // = `ImageItem.path`, i.e. a file — which never matches the
        // folder path of a tree node, so is_current() never fired.)
        current_folder: Option<PathBuf>,
        folder: &Option<PathBuf>,
        nav_count: usize,
        actions: &mut Vec<UiAction>,
        pending_ctx: &mut Option<TreeCtxIntent>,
        pal: &Palette,
    ) -> f32 {
        let frame = egui::Frame::default()
            .inner_margin(egui::Margin::same(10.0))
            .stroke(egui::Stroke::new(0.0_f32, egui::Color32::TRANSPARENT));
        let inner = frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("FOLDERS")
                        .size(12.0)
                        .strong()
                        .color(pal.text_dim),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Phase 7: back-to-top button. Phase 4 用了 `▲`
                    // (U+25B2) Unicode 三角形，但实际渲染时感觉
                    // 纵向偏窄（Unicode 三角形 glyph 的视觉高度
                    // 比同等 size 的正方形小一截，跟按钮的
                    // 32px 高度不协调）。
                    //
                    // 改用 Path 绘制一个填充三角形：顶点在
                    // (cx, top)，左下 (cx - half, bottom)，
                    // 右下 (cx + half, bottom)。Painter
                    // 直接画到按钮位置，跟随主题 text_secondary
                    // 颜色，跟随 Linear 极简风格。按钮整体仍
                    // 是 egui::Button 负责 hover/click hit-testing
                    // 和 hover_fill 背景。
                    // Phase 9: shrunk 28x26 → 20x20 and inset the
                    // triangle 4px top/bottom with a 0.26 half-width
                    // factor — the previous triangle dominated the
                    // header row and sat visually heavier than the
                    // "FOLDERS" label next to it. The hit area stays
                    // comfortable at 20px and the row's Align::Center
                    // layout keeps it vertically aligned with the text.
                    // Phase 12: triangle → HOME glyph (stroked house:
                    // pitched roof + body), matching the vector-drawn
                    // window controls' 1.5px round-cap style. Reads as
                    // "back home / top of list" more naturally than a
                    // bare triangle.
                    // Phase 13: stroked two-piece house → FILLED house
                    // pentagon (apex → right eave → right floor → left
                    // floor → left eave). A solid silhouette reads
                    // unmistakably as "home" at 20px, unlike the stroked
                    // roof+rect which rendered as a triangle perched on
                    // an unrelated box. The pentagon is convex, so a
                    // single convex_polygon shape suffices.
                    let (rect, btn_resp) = ui.allocate_exact_size(
                        egui::vec2(20.0, 20.0),
                        egui::Sense::click(),
                    );
                    if btn_resp.hovered() {
                        ui.painter().rect_filled(rect, 4.0, pal.hover_fill);
                    }
                    let cx = rect.center().x;
                    let apex_y = rect.top() + 3.0;
                    let eave_y = rect.center().y - 0.5;
                    let floor_y = rect.bottom() - 4.0;
                    let body_l = rect.left() + 4.0;
                    let body_r = rect.right() - 4.0;
                    let house = [
                        egui::pos2(cx, apex_y),
                        egui::pos2(body_r, eave_y),
                        egui::pos2(body_r, floor_y),
                        egui::pos2(body_l, floor_y),
                        egui::pos2(body_l, eave_y),
                    ];
                    ui.painter().add(egui::Shape::convex_polygon(
                        house.to_vec(),
                        pal.text_secondary,
                        egui::Stroke::NONE,
                    ));
                    let btn_resp = btn_resp.on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Back to top");
                    if btn_resp.clicked() {
                        tree.state.lock().scroll_to_top = true;
                    }
                });
            });
            ui.add_space(6.0);

            if let Some(p) = folder {
                let path_short = crate::path_shorten::shorten(p);
                // Truncate so a long path can't force the panel wider
                // than the user set it. Double-click anywhere on the
                // label emits a CopyFolderPath action so the actual
                // clipboard write happens after the egui borrow ends.
                let path_label = ui.add(
                    egui::Label::new(
                        egui::RichText::new(path_short)
                            .size(12.0)
                            .color(pal.text_tertiary),
                    )
                    .truncate()
                    .sense(egui::Sense::click()),
                );
                let path_resp = path_label.on_hover_text("Double click to copy folder path");
                if path_resp.double_clicked() {
                    actions.push(UiAction::CopyFolderPath(p.clone()));
                }
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(format!("{} image{}", nav_count, if nav_count == 1 { "" } else { "s" }))
                        .size(12.0)
                        .color(pal.text_dim),
                );
                ui.add_space(10.0);
            }

            let tree_scroll_y = tree.state.lock().scroll_offset_y;
            let tree_out = egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .vertical_scroll_offset(tree_scroll_y)
                .show(ui, |ui| {
                    // Scope the guard so it's DROPPED before the recursion
                    // below. `draw_tree_node` re-locks `tree.state` (to set
                    // pending_expand_scroll when the user expands a node) —
                    // holding the guard across the recursive call is a
                    // SELF-DEADLOCK (parking_lot mutex is not reentrant),
                    // which is exactly the "window opens but never paints"
                    // hang. All the state we need is pulled out here.
                    let (mut roots, mut expanded, reveal, recent_scroll_target) = {
                        let mut state = tree.state.lock();
                        // Pending expand-scroll: if a directory header was
                        // expanded on the previous frame and its subtree
                        // overflows the viewport, scroll the header to align
                        // with the top (Align::Min pins to TOP, `None` only
                        // scrolls the minimum to bring it into view). If it
                        // fits, leave the scroll alone. Gated on all three
                        // being set so the scroll only fires AFTER the
                        // subtree has been measured on the open frame — the
                        // expand-click sets pending_expand_scroll, and the
                        // `should_capture` block in draw_tree_node fills
                        // rect + subtree height on the following draw.
                        if state.pending_expand_scroll.is_some()
                            && state.pending_expand_rect.is_some()
                            && state.pending_expand_subtree_h > 0.0
                        {
                            let path = state.pending_expand_scroll.take().unwrap();
                            let header_rect = state.pending_expand_rect.take().unwrap();
                            let subtree_h = state.pending_expand_subtree_h;
                            state.pending_expand_subtree_h = 0.0;
                            let clip_h = ui.clip_rect().height();
                            if subtree_h > clip_h + 1.0 {
                                ui.scroll_to_rect(
                                    egui::Rect::from_min_size(
                                        header_rect.min,
                                        egui::vec2(header_rect.width(), 0.0),
                                    ),
                                    Some(egui::Align::Min),
                                );
                            }
                            let _ = path; // suppress unused-warning
                        }
                        if state.scroll_to_top {
                            state.scroll_to_top = false;
                            ui.scroll_to_rect(
                                egui::Rect::from_min_size(ui.min_rect().min, egui::Vec2::ZERO),
                                None,
                            );
                        }
                        let roots = std::mem::take(&mut state.roots);
                        let expanded = std::mem::take(&mut state.expanded);
                        let reveal = state.reveal_target.take();
                        let recent_scroll_target = state.recent_scroll_target.take();
                        (roots, expanded, reveal, recent_scroll_target)
                    };
                    let mut revealed_node = false;
                    let mut recent_scroll = recent_scroll_target;
                    // Phase 2 + Phase 8 修复: 之前用闭包内局部 `pending` 变量，
                    // 闭包返回时被 drop 导致右键事件被静默丢弃。Phase 8 改用
                    // 传入 `&mut Option<TreeCtxMenu>` 指向 self.tree_ctx_menu。
                    // Phase 9 修复: 不再在此处清空！之前每帧 `*pending_ctx =
                    // None` 会把上一帧刚写入的右键请求在 popup 绘制前抹掉，
                    // 菜单只显示 1 帧即消失。现在字段只在用户选中菜单项或
                    // 点击外部时由 popup 绘制块显式清除。
                    let mut max_w: f32 = 0.0;
                    // IMPORTANT: `tree.state` lock is NOT held here — the final
                    // `draw_tree_node` call may lock it briefly. This block is
                    // the recursion; the outer guard is already dropped.
                    for (root_idx, root) in roots.iter_mut().enumerate() {
                        max_w = max_w.max(Self::draw_tree_node(
                            ui, tree, root, 0, &current_folder, &mut expanded, actions, root_idx,
                            &reveal, &mut revealed_node, pending_ctx,
                            &mut recent_scroll, &pal,
                        ));
                    }
                    // Re-lock to restore the taken state for the next frame.
                    // `recent_scroll` is a ONE-SHOT intent (scroll a Recent
                    // entry into view after a This PC collapse): it is always
                    // cleared here so a stale target can't keep firing
                    // `scroll_to_rect` every frame, which would make the
                    // scrollbar appear "stuck" (dragging fights the forced
                    // offset). `reveal_state` may legitimately need several
                    // frames while ancestors expand, so it is only kept when
                    // not yet revealed.
                    {
                        let mut state = tree.state.lock();
                        state.roots = roots;
                        state.expanded = expanded;
                        if !revealed_node {
                            state.reveal_target = reveal;
                        }
                        // Recent-scroll: clear unconditionally (one-shot).
                        let _ = recent_scroll;
                        state.recent_scroll_target = None;
                    }
                    max_w
                });
            // Read back the ScrollArea's ACTUAL (clamped) offset so the
            // sticky tree scroll stays bounded — fixes scrollbar drag,
            // scroll_to_rect reveal/recent centering, and the unbounded
            // "scroll into blank space then freeze" bug.
            let off = tree_out.state.offset.y;
            tree.state.lock().scroll_offset_y = off;
            tree_out.inner
        });
        // + panel margins + scrollbar allowance.
        inner.inner + 20.0 + 10.0
    }

    /// Draw one tree node. Returns the widest content row (logical px) in
    /// its subtree for the panel's content-driven minimum width.
    #[allow(clippy::too_many_arguments)]
    fn draw_tree_node(
        ui: &mut egui::Ui,
        tree: &crate::file_tree::FileTree,
        node: &mut crate::file_tree::TreeNode,
        depth: usize,
        // Phase 14: the nav's current FOLDER (used for the active
        // row's indigo card). Was previously a file path
        // (`ImageItem.path` via `nav.current()`) which never matched
        // a tree node's folder path — is_current() never fired.
        current_folder: &Option<PathBuf>,
        expanded: &mut [std::collections::HashSet<std::path::PathBuf>; 3],
        actions: &mut Vec<UiAction>,
        root_idx: usize,
        reveal: &Option<PathBuf>,
        revealed: &mut bool,
        // Phase 2: out-param to defer the native context menu show
        // until after the egui frame ends. None = no right-click
        // pending. Some(...) = the egui closure captured a
        // secondary-click on this node; render_frame's drain step
        // reads it and calls open_tree_context_menu.
        pending: &mut Option<TreeCtxIntent>,
        recent_scroll: &mut Option<PathBuf>,
        pal: &Palette,
    ) -> f32 {
        // Nodes are created with `children: Some(vec![])`, so emptiness —
        // not `None` — is the "not loaded yet" signal. `loading` marks
        // "already walked, empty result" so we don't re-query every frame.
        let unloaded = node.children.as_ref().map_or(true, |c| c.is_empty());
        let is_virtual_root = node.path.as_os_str().is_empty();
        // Favorites (root 0) and Recent (root 1) entries at depth 1 are
        // non-expandable navigation leaves with rounded highlight UI.
        let is_leaf_entry = (root_idx == 0 || root_idx == 1) && depth == 1 && !is_virtual_root;
        if depth > 0 && expanded[root_idx].contains(&node.path) && unloaded && !node.loading && !is_virtual_root && !is_leaf_entry
        {
            crate::file_tree::FileTree::load_children(node);
            node.loading = true;
        }
        // Roots (depth 0, empty path) now use the expanded set too, so
        // their triangles collapse properly.
        let is_open = expanded[root_idx].contains(&node.path);
        // Highlight only in the root the user last picked a folder from —
        // the same path may be registered in both Recent and Favorites.
        let active_root = ACTIVE_ROOT.load(std::sync::atomic::Ordering::Relaxed);
        let is_current = current_folder
            .as_ref()
            .map(|c| c == &node.path && root_idx == active_root)
            .unwrap_or(false);
        let display = node.display_name.clone();
        let path_clone = node.path.clone();
        let is_reveal_target = reveal.as_ref() == Some(&node.path) && root_idx == 2;
        let text_color = if is_current || is_reveal_target {
            pal.selection_text
        } else {
            pal.text_secondary
        };
        // Cached text width → content-driven panel minimum.
        if node.text_w <= 0.0 {
            let galley = ui.painter().layout_no_wrap(
                display.clone(),
                egui::FontId::proportional(13.0),
                text_color,
            );
            node.text_w = galley.size().x;
        }
        let row_w = depth as f32 * 14.0 + 24.0 + node.text_w;
        let mut max_w = row_w;

        // Leaf entries (Favorites / Recent): custom-painted rounded
        // highlight (egui selectable_label's built-in selected color mixes
        // green/blue with our indigo). Click navigates, right-click manages.
        // Linear-style: rounded background card + left accent bar on the
        // selected edge + bolded label so it stays visually distinct from
        // the collapsed directory rows below it.
        if is_leaf_entry {
            let selected = is_current || is_reveal_target;
            let (rect, resp) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 26.0),
                egui::Sense::click(),
            );
            if selected {
                ui.painter().rect_filled(rect, 6.0, pal.selected_card_fill);
                // Left accent bar — a slim darker indigo strip on the leading
                // edge, inset vertically so it doesn't collide with the
                // rounded corners of the card (matches Linear's row marker).
                let bar = egui::Rect::from_min_size(
                    egui::pos2(rect.left() + 2.0, rect.top() + 3.0),
                    egui::vec2(2.0, rect.height() - 6.0),
                );
                ui.painter().rect_filled(bar, 1.0, pal.selected_card_stroke);
                // Linear-style edge shine: a faint accent glow along the
                // leading edge to lift the selected row off the panel.
                crate::effects::paint_edge_shine(
                    ui.painter(),
                    rect,
                    pal.selected_card_stroke,
                    36,
                );
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, 6.0, pal.hover_fill);
            }
            // Faint grain over the whole card breaks up the flat fill so it
            // reads as a surface rather than a solid block (Linear's Grain).
            crate::effects::paint_grain(ui.painter(), rect, 4);
            ui.painter().text(
                egui::pos2(rect.left() + 10.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                display,
                if selected {
                    egui::FontId::proportional(15.0)
                } else {
                    egui::FontId::proportional(14.0)
                },
                if selected { pal.selection_text } else { pal.text_secondary },
            );
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            if is_reveal_target && !*revealed {
                ui.scroll_to_rect(resp.rect, Some(egui::Align::Center));
                *revealed = true;
            }
            // Scroll a Recent entry into view (e.g. after a This PC
            // collapse hid the current folder).
            if root_idx == 1 && recent_scroll.as_ref() == Some(&node.path) {
                ui.scroll_to_rect(resp.rect, Some(egui::Align::Center));
                *recent_scroll = None;
            }
            if resp.clicked() {
                actions.push(UiAction::FolderChosen(path_clone.clone(), root_idx));
            }
            // Phase 2: native context menu (replaces the previous
            // egui resp.context_menu closure). secondary_clicked()
            // fires once on right-button-down; capture the cursor
            // position and defer the actual popup show to
            // render_frame()'s drain step (the egui closure can't
            // safely call show_tree_context_menu which needs
            // &mut self while the egui frame has &mut self borrowed).
            if resp.secondary_clicked() {
                if let Some(pos) = ui.ctx().input(|i| i.pointer.latest_pos()) {
                    *pending = Some(TreeCtxIntent {
                        pos_phys: (pos.x as i32, pos.y as i32),
                        path: path_clone.clone(),
                        root_idx,
                    });
                }
            }
        } else {
            // ---- Handwritten directory row (replaces CollapsingHeader) ----
            // We draw the row OURSELVES so height/indent/click/highlight are
            // deterministic and never fight egui's internal header state.
            // Each row: indent = depth*14px, a disclosure triangle, the
            // label, and (when open) its children indented one level.
            let dir_selected = is_current || is_reveal_target;
            // This PC expanded directories get only bold — no bg, no bar.
            let this_pc_expanded = root_idx == 2 && !is_virtual_root;
            let indent = depth as f32 * 14.0;
            let row_h = 26.0_f32;
            let (row_rect, row_resp) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), row_h),
                egui::Sense::click(),
            );
            // Full-row background + accent bar + bold (skip bar/bg for This
            // PC expanded directories).
            if dir_selected && !this_pc_expanded {
                ui.painter().rect_filled(row_rect, 6.0, pal.selected_card_fill);
                let bar = egui::Rect::from_min_size(
                    egui::pos2(row_rect.left() + 2.0, row_rect.top() + 3.0),
                    egui::vec2(2.0, row_rect.height() - 6.0),
                );
                ui.painter().rect_filled(bar, 1.0, pal.selected_card_stroke);
                crate::effects::paint_edge_shine(
                    ui.painter(), row_rect, pal.selected_card_stroke, 36,
                );
            } else if row_resp.hovered() {
                ui.painter().rect_filled(row_rect, 6.0, pal.hover_fill);
            }
            crate::effects::paint_grain(ui.painter(), row_rect, 3);
            // Disclosure triangle (rotates on open).
            let tri_c = egui::pos2(row_rect.left() + indent + 7.0, row_rect.center().y);
            if is_open {
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        tri_c + egui::vec2(-3.5, -2.5),
                        tri_c + egui::vec2(3.5, -2.5),
                        tri_c + egui::vec2(0.0, 2.5),
                    ],
                    pal.text_secondary,
                    egui::Stroke::NONE,
                ));
            } else {
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        tri_c + egui::vec2(-2.5, -3.5),
                        tri_c + egui::vec2(2.5, 0.0),
                        tri_c + egui::vec2(-2.5, 3.5),
                    ],
                    pal.text_secondary,
                    egui::Stroke::NONE,
                ));
            }
            // Label (bold when selected). Text is drawn at fixed x so long
            // names never wrap and never overlap the next row.
            let text_x = row_rect.left() + indent + 18.0;
            ui.painter().text(
                egui::pos2(text_x, row_rect.center().y),
                egui::Align2::LEFT_CENTER,
                display,
                if dir_selected {
                    egui::FontId::proportional(15.0)
                } else {
                    egui::FontId::proportional(14.0)
                },
                text_color,
            );
            // Capture this row for the pending-expand scroll decision.
            if tree.state.lock().pending_expand_scroll.as_ref().is_some_and(|p| p == &path_clone) {
                tree.state.lock().pending_expand_rect = Some(row_rect);
            }
            if is_reveal_target && !*revealed {
                ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
                *revealed = true;
            }
            // Click toggles expansion; non-root nodes also navigate. We
            // check the row response AND a triangle/label hit so the whole
            // row (including the blank area right of the label) is clickable.
            let header_clicked = row_resp.clicked();
            if header_clicked {
                if is_open {
                    expanded[root_idx].remove(&path_clone);
                } else {
                    expanded[root_idx].insert(path_clone.clone());
                    tree.state.lock().pending_expand_scroll = Some(path_clone.clone());
                }
                if is_open && root_idx == 2 {
                    if let Some(cur) = current_folder {
                        if cur != &path_clone && cur.starts_with(&path_clone) {
                            *recent_scroll = Some(cur.clone());
                        }
                    }
                }
                if depth > 0 {
                    actions.push(UiAction::FolderChosen(path_clone.clone(), root_idx));
                }
            }
            // Right-click → deferred native context menu.
            if row_resp.secondary_clicked() {
                if let Some(pos) = ui.ctx().input(|i| i.pointer.latest_pos()) {
                    *pending = Some(TreeCtxIntent {
                        pos_phys: (pos.x as i32, pos.y as i32),
                        path: path_clone.clone(),
                        root_idx,
                    });
                }
            }
            // Render children (indented) when the node is open. After they
            // lay out, if this row was the pending-expand target, record the
            // subtree height so the consumer can scroll it to the top.
            if is_open {
                if let Some(children) = node.children.as_mut() {
                    if children.is_empty() && !is_virtual_root && node.loading {
                        ui.painter().text(
                            egui::pos2(row_rect.left() + indent + 18.0, row_rect.bottom() + 12.0),
                            egui::Align2::LEFT_TOP,
                            "(empty)",
                            egui::FontId::proportional(11.0),
                            pal.text_dim,
                        );
                    }
                    for child in children.iter_mut() {
                        let w = Self::draw_tree_node(
                            ui, tree, child, depth + 1, current_folder, expanded, actions, root_idx,
                            reveal, revealed, pending, recent_scroll, pal,
                        );
                        max_w = max_w.max(w);
                    }
                }
                // Record subtree height for the pending-expand scroll.
                if tree.state.lock().pending_expand_scroll.as_ref().is_some_and(|p| p == &path_clone) {
                    let bottom = ui.min_rect().bottom();
                    tree.state.lock().pending_expand_subtree_h = bottom - row_rect.top();
                }
            }
        }
        max_w
    }

    fn draw_thumbs_panel_static(
        ui: &mut egui::Ui,
        texture_cache: &Arc<crate::texture_cache::TextureCache>,
        nav_items: &[aperture_core::ImageItem],
        cur_idx: usize,
        force_scroll: bool,
        actions: &mut Vec<UiAction>,
        pal: &Palette,
    ) {
        let frame = egui::Frame::default()
            .fill(pal.panel_bg)
            .inner_margin(egui::Margin::same(10.0));
        frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new("THUMBNAILS")
                    .size(12.0)
                    .strong()
                    .color(pal.text_dim),
            );
            ui.add_space(4.0);
            let nav_count = nav_items.len();
            ui.label(
                egui::RichText::new(format!("{}/{}", if nav_count == 0 { 0 } else { cur_idx + 1 }, nav_count))
                    .size(11.0)
                    .color(pal.text_tertiary),
            );
            ui.add_space(6.0);

            if nav_count == 0 {
                ui.label(
                    egui::RichText::new("No images in folder.\nUse Open Folder to choose one.")
                        .size(11.0)
                        .color(pal.text_dim),
                );
                return;
            }

            let scroll_out = egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .vertical_scroll_offset(THUMB_SCROLL_Y.with(|c| c.get()))
                .show(ui, |ui| {
                    // Multi-column thumbnail grid with EXPLICIT width
                    // thresholds so the square cards grow between thresholds
                    // instead of the count jittering at every pixel:
                    //   < T1     -> 1 col (card fills width)
                    //   T1..T2   -> 2 cols (cards grow with width)
                    //   >= T2    -> 3 cols
                    // The per-card Frame adds 2*inner_margin (8px), which the
                    // old formula ignored -> the last column overflowed and
                    // was clipped by the window right edge.
                    const CARD_MARGIN_TOTAL: f32 = 8.0; // 2 × inner_margin(4)
                    let avail = (ui.available_width() - 8.0).max(80.0);
                    let gap: f32 = 6.0;
                    // Effective width available across all cards + gaps.
                    let col_count = if avail >= 520.0 {
                        3usize
                    } else if avail >= 340.0 {
                        2usize
                    } else {
                        1usize
                    };
                    let cols = col_count.max(1);
                    let card_w = ((avail - gap * (cols as f32 - 1.0) - CARD_MARGIN_TOTAL * cols as f32)
                        / cols as f32)
                        .max(60.0);
                    // Row height = square image (card_w) + filename line +
                    // margins/spacing. Used only by the scroll prefetch
                    // estimator below.
                    let row_full_h: f32 = card_w + 26.0;

                    // Thumbnail decode virtualization: only request decodes
                    // for cards near the visible range (±buffer), not all
                    // images in the folder. We now compute by *rows*
                    // (each row contains `cols` cards), and the prefetch
                    // covers ~2 extra rows in the direction of scroll.
                    let clip = ui.clip_rect();
                    let content_top = ui.min_rect().top();
                    let est_row_h = row_full_h;
                    let first_row = (((clip.top() - content_top) / est_row_h).floor() as isize - 5)
                        .max(0) as usize;
                    let last_row = ((((clip.top() - content_top) + clip.height()) / est_row_h).ceil()
                        as isize + 10)
                        .max(0) as usize;
                    let i0 = (first_row * cols).min(nav_items.len());
                    let i1 = ((last_row + 1) * cols).min(nav_items.len());
                    // Prefetch in the direction of scroll: a fast scroll shows
                    // placeholders for cards about to enter. We request decodes
                    // for an extra ~2 rows in the direction the user is moving.
                    let cur_scroll = THUMB_SCROLL_Y.with(|c| c.get());
                    let last_scroll = THUMB_LAST_SCROLL_Y.with(|c| c.get());
                    THUMB_LAST_SCROLL_Y.set(cur_scroll);
                    let rows_screen = (clip.height() / est_row_h).ceil() as isize + 2;
                    let first_row_with_prefetch = if cur_scroll > last_scroll {
                        first_row
                    } else if cur_scroll < last_scroll {
                        (first_row as isize - rows_screen).max(0) as usize
                    } else {
                        (first_row as isize - rows_screen / 2).max(0) as usize
                    };
                    let last_row_with_prefetch = if cur_scroll > last_scroll {
                        ((last_row as isize + rows_screen) as usize).min((nav_items.len() + cols - 1) / cols)
                    } else {
                        last_row
                    };
                    let pf_lo = (first_row_with_prefetch * cols).min(nav_items.len());
                    let pf_hi = ((last_row_with_prefetch + 1) * cols).min(nav_items.len());
                    for item in nav_items.get(pf_lo..pf_hi).unwrap_or(&[]) {
                        texture_cache.request_thumb(item.path.clone());
                    }

                    let n = nav_items.len();
                    let mut i = 0usize;
                    let mut resp_rects: Vec<egui::Rect> = Vec::with_capacity(n);
                    while i < n {
                        let row_count = (n - i).min(cols);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = gap;
                            for col in 0..row_count {
                                let idx = i + col;
                                let item = &nav_items[idx];
                                let path = item.path.clone();
                                let is_selected = idx == cur_idx;
                                let name = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();

                                // Square image area: the card is a SQUARE frame
                                // (card_w × card_w). The thumbnail is contained
                                // (fit) inside it with padding so its corners
                                // don't clip the card's rounded corners. The
                                // filename goes on its OWN line BELOW the image
                                // (the card.inner ui is forced top-down so the
                                // label isn't laid out to the image's right).
                                let img_h = card_w;
                                const IMG_PAD: f32 = 6.0;

                                let card = egui::Frame::default()
                                    .fill(if is_selected {
                                        pal.selected_card_fill
                                    } else {
                                        egui::Color32::TRANSPARENT
                                    })
                                    .stroke(if is_selected {
                                        egui::Stroke::new(1.2_f32, pal.selected_card_stroke)
                                    } else {
                                        egui::Stroke::new(1.0_f32, pal.card_stroke)
                                    })
                                    .rounding(egui::Rounding::same(8.0))
                                    .inner_margin(egui::Margin::same(4.0));
                                let resp = card.show(ui, |ui| {
                                    // Force a vertical stack so the image area
                                    // and the filename line up top-to-bottom
                                    // instead of side by side (the inherited
                                    // `horizontal` row layout put the label to
                                    // the image's right).
                                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(card_w, img_h),
                                            egui::Sense::hover(),
                                        );
                                        if let Some(entry) = texture_cache.get_thumb(&path) {
                                            let size = entry.texture.size_vec2();
                                            let inner = rect.shrink(IMG_PAD);
                                            let scale = (inner.width() / size.x.max(1.0))
                                                .min(inner.height() / size.y.max(1.0));
                                            let fit_size =
                                                egui::vec2(size.x * scale, size.y * scale);
                                            let fit_rect = egui::Rect::from_center_size(
                                                inner.center(),
                                                fit_size,
                                            );
                                            ui.painter().image(
                                                entry.texture.id(),
                                                fit_rect,
                                                egui::Rect::from_min_max(
                                                    egui::Pos2::ZERO,
                                                    egui::Pos2::new(1.0, 1.0),
                                                ),
                                                egui::Color32::WHITE,
                                            );
                                        } else {
                                            ui.painter().rect_filled(
                                                rect,
                                                6.0,
                                                pal.thumb_placeholder,
                                            );
                                            ui.painter().text(
                                                rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                "···",
                                                egui::FontId::proportional(12.0),
                                                pal.text_dim,
                                            );
                                        }
                                        // Filename on its own truncated line.
                                        ui.add_space(2.0);
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(name)
                                                    .size(10.5)
                                                    .color(if is_selected {
                                                        pal.text_primary
                                                    } else {
                                                        pal.text_tertiary
                                                    }),
                                            )
                                            .truncate(),
                                        );
                                    });
                                }).response.interact(egui::Sense::click());
                                resp_rects.push(resp.rect);
                                if is_selected && force_scroll {
                                    ui.scroll_to_rect(resp.rect, Some(egui::Align::Center));
                                }
                                if resp.clicked() || resp.double_clicked() {
                                    actions.push(UiAction::ThumbClicked(idx));
                                }
                            }
                        });
                        i += row_count;
                        // Inter-row spacing: small fixed gap so the rows
                        // don't run into each other and the label has room.
                        ui.add_space(8.0);
                    }
                    // `i0`/`i1` are still useful as a sentinel — they bound
                    // the visible+decoded range. Mark them used here.
                    let _ = (i0, i1, first_row, last_row);
                });
                // Read back the ScrollArea's ACTUAL offset (egui clamps it
                // to [0, max] before returning) so the sticky value stays
                // bounded. This is the fix for: (a) scrollbar drag not
                // working (the read/write now agree), (b) scroll_to_rect
                // centering, (c) the "scroll into blank space then freeze"
                // bug — the stale sticky value was unbounded, and
                // `vertical_scroll_offset` forced it every frame, so egui
                // fought to reach an unreachable offset.
                let off = scroll_out.state.offset.y;
                THUMB_SCROLL_Y.with(|c| c.set(off));
        });
    }

    /// Draw the open right-click context menu (Linear-themed egui
    /// window). Replaces the native Win32 TrackPopupMenu that was
    /// introduced when the D2D child HWND clipped egui windows — that
    /// D2D layer is gone, so egui popups work again and follow the
    /// theme. A new right-click (which sets `ctx_menu`) closes any
    /// previous menu because `ctx_menu` is replaced wholesale; clicking
    /// anywhere else closes it via the outside-click detection below.
    ///
    /// Returns `true` if the menu should remain open (re-arm it on
    /// `self.ctx_menu`); `false` if it should close.
    fn draw_context_menu(
        ctx: &egui::Context,
        menu: &CtxMenu,
        pal: &Palette,
        current_path: Option<&Path>,
        actions: &mut Vec<UiAction>,
        opened_at: Option<std::time::Instant>,
    ) -> bool {
        let mut close = false;

        let frame = egui::Frame::default()
            .fill(egui::Color32::from_rgba_unmultiplied(
                pal.panel_bg.r(), pal.panel_bg.g(), pal.panel_bg.b(), 235,
            ))
            .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::same(6.0))
            .shadow(egui::Shadow {
                spread: 0.0,
                color: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 120),
                offset: egui::vec2(0.0, 4.0),
                blur: 24.0,
            });

        let mut item = |ui: &mut egui::Ui, label: &str, action: UiAction| -> bool {
            let mut clicked = false;
            // Allocate a FULL-WIDTH row first so the hover highlight spans
            // the whole menu item, not just the text extents (the old
            // Button::frame(false) made the highlight a variable-width box
            // hugging the label).
            let row_h: f32 = 28.0;
            let (row_rect, resp) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), row_h),
                egui::Sense::click(),
            );
            if resp.hovered() {
                ui.painter().rect_filled(
                    row_rect,
                    6.0,
                    pal.hover_fill,
                );
            }
            ui.painter().text(
                egui::pos2(row_rect.left() + 8.0, row_rect.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(12.5),
                pal.text_secondary,
            );
            if resp.clicked() {
                clicked = true;
                actions.push(action);
            }
            clicked
        };

        let window = egui::Window::new("")
            // Unique Id per variant so the image/tree context menus and the
            // settings menu never collide on widget Ids (empty title would
            // give all of them Id::new("") and break click hit-testing).
            .id(egui::Id::new(match menu {
                CtxMenu::Image { .. } => "ctx_menu_image",
                CtxMenu::Tree { .. } => "ctx_menu_tree",
            }))
            .frame(frame)
            .order(egui::Order::Foreground)
            .resizable(false)
            .collapsible(false)
            .title_bar(false)
            .fade_in(false)
            .fade_out(false);

        let window = window.auto_sized();

        match menu {
            CtxMenu::Image { pos } => {
                window.fixed_pos(*pos).show(ctx, |ui| {
                    if item(ui, "复制图片路径", UiAction::CopyPath) { close = true; }
                    if item(ui, "在资源管理器中打开", UiAction::OpenInExplorer) { close = true; }
                    if item(ui, "打印", UiAction::Print) { close = true; }
                    if item(ui, "设为桌面壁纸", UiAction::SetWallpaper) { close = true; }
                    if let Some(p) = current_path {
                        if let Some(parent) = p.parent().map(|q| q.to_path_buf()) {
                            if item(ui, "在目录树中定位", UiAction::RevealInTree(parent)) { close = true; }
                        }
                    }
                });
            }
            CtxMenu::Tree { pos, path, root_idx, is_favorite } => {
                window.fixed_pos(*pos).show(ctx, |ui| {
                    if item(ui, "在资源管理器中打开", UiAction::RevealInExplorer(path.clone())) { close = true; }
                    if *is_favorite {
                        if item(ui, "取消收藏", UiAction::RemoveFavorite(path.clone())) { close = true; }
                    } else {
                        if item(ui, "添加到收藏", UiAction::AddFavorite(path.clone())) { close = true; }
                    }
                    if *root_idx == 1 {
                        if item(ui, "从 Recent 移除", UiAction::RemoveRecent(path.clone())) { close = true; }
                    }
                    if *root_idx != 2 {
                        if item(ui, "在目录树中定位", UiAction::RevealInTree(path.clone())) { close = true; }
                    }
                });
            }
        }

        // egui does NOT auto-close a non-modal Window on outside clicks,
        // so detect it here. Menu item clicks already set `close = true`
        // above; any OTHER click is treated as "outside" and closes.
        //
        // IMPORTANT: the right-button-DOWN that OPENED the menu is still
        // "in flight" on the frame the menu first appears (egui reports
        // `pointer.any_click()` true for that same click). If we closed on
        // that, the menu would flash and vanish instantly. So we ignore
        // clicks within a short grace window (120 ms) after opening.
        let should_stay_open = if close {
            false
        } else {
            let opened_just_now = opened_at.map_or(false, |t| {
                t.elapsed().as_millis() < 120
            });
            if opened_just_now {
                true
            } else {
                !ctx.input(|i| i.pointer.any_click())
            }
        };
        let _ = menu; // menu is a borrow; keep the binding alive
        should_stay_open
    }

    fn apply_action(&mut self, action: UiAction) {
        match action {
            UiAction::OpenFolder => self.action_open_folder(),
            UiAction::Prev => self.handle_navigation(NavigationDirection::Previous, SlideDir::Previous),
            UiAction::Next => self.handle_navigation(NavigationDirection::Next, SlideDir::Next),
            UiAction::Fit => {
                if let Some(v) = &self.viewer { v.lock().fit_to_screen(); }
                if let Some(window) = &self.window { window.request_redraw(); }
            }
            UiAction::OneToOne => {
                if let Some(v) = &self.viewer {
                    v.lock().zoom_1_to_1();
                }
                if let Some(window) = &self.window { window.request_redraw(); }
            }
            // Phase 3: fit↔1:1 cycle. Read is_fit_scale out of the
            // viewer to decide which way to go. The bottom bar
            // button is the only call site for FitOrOriginal; the
            // keyboard F / Ctrl+0 shortcuts still go through Fit /
            // OneToOne for predictability.
            UiAction::FitOrOriginal => {
                if let Some(v) = &self.viewer {
                    let mut g = v.lock();
                    if g.is_fit_scale() {
                        g.zoom_1_to_1();
                    } else {
                        g.fit_to_screen();
                    }
                }
                if let Some(window) = &self.window { window.request_redraw(); }
            }
            // Phase 3: rotate by `delta` quarter-turns clockwise.
            // The bottom bar always sends +1; the keyboard Ctrl+R
            // could send -1 in the future. Rotation persists only
            // for the current image; the viewer's set_image resets
            // it to 0.
            UiAction::RotateImage(delta) => {
                if let Some(v) = &self.viewer {
                    let mut g = v.lock();
                    let cur = g.rotation();
                    let next = ((cur as i32 + delta).rem_euclid(4)) as u8;
                    g.set_rotation(next);
                }
                if let Some(window) = &self.window { window.request_redraw(); }
            }
            // Phase 3: flip the slide-show timer. The state is
            // stored on MainWindow (slide_show_running / _last);
            // render_frame polls the timer and auto-advances.
            UiAction::ToggleSlideShow => {
                self.slide_show_running = !self.slide_show_running;
                self.slide_show_last = Some(std::time::Instant::now());
            }
            UiAction::ToggleFullscreen => self.action_toggle_fullscreen(),
            UiAction::ToggleTree => {
                self.close_shortcut_help();
                self.show_tree = !self.show_tree;
            }
            UiAction::ToggleThumbs => {
                self.close_shortcut_help();
                self.show_thumbs = !self.show_thumbs;
            }
            UiAction::ThumbClicked(i) => {
                // Move nav index to the clicked thumb.
                let count = self.nav.lock().count();
                if i < count {
                    self.nav.lock().set_index(i);
                    if let Some(coordinator) = &self.coordinator {
                        coordinator.request_current(SlideDir::None);
                    }
                    if let Some(window) = &self.window { window.request_redraw(); }
                }
            }
            UiAction::FolderChosen(p, root) => {
                ACTIVE_ROOT.store(root, std::sync::atomic::Ordering::Relaxed);
                self.navigate_to_folder(p);
            }
            UiAction::ToggleShortcutHelp => {
                self.show_shortcut_help = !self.show_shortcut_help;
            }
            UiAction::ToggleFavorite => {
                // Favorites store FOLDERS (not files) — and only folders
                // that directly contain images, mirroring the gate on
                // `navigate_to_folder` that keeps Recent useful. The toggle
                // from the image context menu flips the current image's
                // parent folder in/out of favorites.
                if let Some(current) = self.nav.lock().current() {
                    if let Some(parent) = current.path.parent().map(|p| p.to_path_buf()) {
                        if Self::folder_has_images(&parent) {
                            self.settings.toggle_favorite_folder(parent);
                            self.file_tree
                                .refresh_favorites(&self.settings.favorite_folders());
                        } else {
                            tracing::debug!(
                                "ToggleFavorite skipped: {} has no images",
                                parent.display()
                            );
                        }
                    }
                }
            }
            UiAction::AddFavorite(p) => {
                // Gate: only add the folder if it directly contains images
                // (same rule as Recent in `navigate_to_folder`).
                if Self::folder_has_images(&p) {
                    self.settings.add_favorite_folder(p);
                    self.file_tree
                        .refresh_favorites(&self.settings.favorite_folders());
                } else {
                    tracing::debug!(
                        "AddFavorite skipped: {} has no images",
                        p.display()
                    );
                }
            }
            UiAction::RemoveFavorite(p) => {
                self.settings.remove_favorite_folder(&p);
                self.file_tree.refresh_favorites(&self.settings.favorite_folders());
            }
            UiAction::RemoveRecent(p) => {
                self.settings.remove_recent_folder(&p);
                self.file_tree.refresh_recent(&self.settings.recent_folders());
            }
            UiAction::RevealInExplorer(p) => {
                self.reveal_in_explorer(&p);
            }
            UiAction::BrowseFolder(p) => {
                self.navigate_to_folder(p);
            }
            UiAction::RevealInTree(p) => {
                // Phase 12: locate now NAVIGATES — expand the This PC
                // branch and scroll to the folder (reveal), AND load its
                // images into the viewer + thumbnail panel. The tree's
                // normal current-folder highlight takes over from there
                // (no special card — This PC headers don't suit one).
                self.file_tree.reveal(&p);
                self.navigate_to_folder(p);
            }
            UiAction::ExitApp => {
                self.save_window_geometry();
                std::process::exit(0);
            }
            UiAction::MinimizeWindow => {
                if let Some(w) = &self.window {
                    w.set_minimized(true);
                }
            }
            UiAction::ToggleMaximize => {
                if let Some(w) = &self.window {
                    let maximized = w.is_maximized();
                    w.set_maximized(!maximized);
                }
            }
            UiAction::ToggleTheme => {
                // Cycle Dark -> Light -> (System is opted into via settings
                // menu, not the header toggle). `toggle()` on Dark/Light
                // flips; to keep the header toggle predictable we map System
                // to a plain flip.
                self.theme = match self.theme {
                    Theme::Dark => Theme::Light,
                    Theme::Light => Theme::Dark,
                    Theme::System => Theme::Dark,
                };
                self.settings.set_theme(self.theme);
                // The D2D child bg follows via render_frame's palette sync.
            }
            UiAction::SetWallpaper => {
                if let Some(current) = self.nav.lock().current() {
                    let path = current.path.clone();
                    self.set_wallpaper(path);
                }
            }
            UiAction::OpenInExplorer => {
                if let Some(current) = self.nav.lock().current() {
                    let path = current.path.clone();
                    self.reveal_in_explorer(&path);
                }
            }
            UiAction::CopyPath => {
                // Phase 8: the image right-click context menu
                // emits this action (the menu closure can't
                // call self.copy_text_to_clipboard directly
                // because of the egui_state borrow).
                if let Some(current) = self.nav.lock().current() {
                    self.copy_text_to_clipboard(&current.path.to_string_lossy());
                }
            }
            UiAction::CopyFolderPath(p) => {
                self.copy_text_to_clipboard(&p.to_string_lossy());
                tracing::info!("copied folder path to clipboard: {}", p.display());
            }
            UiAction::Print => {
                if let Some(current) = self.nav.lock().current() {
                    let path = current.path.clone();
                    self.print_image(path);
                }
            }
            UiAction::ToggleSettings => {
                self.show_settings_menu = !self.show_settings_menu;
            }
            UiAction::SettingsClearRecent => {
                self.settings.clear_recent();
                self.file_tree.refresh_recent(&self.settings.recent_folders());
            }
            UiAction::SettingsClearFavorites => {
                self.settings.clear_favorites();
                self.file_tree.refresh_favorites(&self.settings.favorite_folders());
            }
            UiAction::SettingsThemeSystem => {
                let next = if self.settings.is_theme_system() {
                    Theme::Dark
                } else {
                    Theme::System
                };
                self.settings.set_theme(next);
                self.theme = next;
            }
            UiAction::SettingsSidebarOnLaunch => {
                let show = !self.settings.show_tree_on_launch();
                self.settings.set_show_tree_on_launch(show);
                self.show_tree = show;
            }
        }
    }

    fn action_open_folder(&mut self) {
        let start = self.settings.last_folder()
            .unwrap_or_else(|| PathBuf::from("C:\\"));
        let Some(path) = rfd::FileDialog::new()
            .set_directory(start)
            .set_title("Choose a folder of images")
            .pick_folder()
        else { return; };

        if let Err(e) = self.nav.lock().navigate_folder(path.clone()) {
            tracing::error!("navigate_folder failed: {:#}", e);
            return;
        }
        if Self::folder_has_images(&path) {
            self.settings.push_recent_folder(path.clone());
        }
        self.settings.set_last_folder(path.clone());
        self.file_tree.refresh_recent(&self.settings.recent_folders());
        self.file_tree.refresh_favorites(&self.settings.favorite_folders());
        if let Some(coordinator) = &self.coordinator {
            coordinator.request_current(SlideDir::None);
        }
        if let Some(window) = &self.window { window.request_redraw(); }
    }

    /// Close the shortcuts popover (and restore the child window region).
    /// Required before fullscreen toggles and panel layout changes — the
    /// popover's hole-punch region cannot track the moving child window.
    fn close_shortcut_help(&mut self) {
        self.show_shortcut_help = false;
    }

    fn action_toggle_fullscreen(&mut self) {
        self.close_shortcut_help();
        self.pending_tree_intent = None;
        let Some(window) = &self.window else { return; };
        self.is_fullscreen = !self.is_fullscreen;
        IS_FULLSCREEN.store(self.is_fullscreen, std::sync::atomic::Ordering::Relaxed);
        // Entering fullscreen always straightens the image first (snap
        // rotation to 0, no spin) and re-fits to the CURRENT window, then
        // the fit follows the monitor once the OS resizes it. This keeps
        // the fullscreen transition deterministic — a live rotation angle
        // would otherwise leave the image resolved at a rotated fit and
        // land off-centre (the "rotate then fullscreen breaks fit" bug).
        // We deliberately skip the rect-anim path animation here: it
        // fought the rotation/fit re-fit. The fit itself is the transition.
        if self.is_fullscreen {
            if let Some(v) = &self.viewer {
                v.lock().reset_rotation();
            }
        }
        if self.is_fullscreen {
            // Enter immersive mode: chrome hidden until the mouse moves.
            self.chrome_visible = true;
            self.chrome_hide_at = Some(std::time::Instant::now());
            self.chrome_move_accum = 0.0;
        } else {
            self.chrome_visible = true;
            self.chrome_hide_at = None;
        }
        // Phase 1 + Phase 11: hide / restore the standard resize-grip
        // frame (WS_THICKFRAME) so DWM doesn't draw a 1–2 px border
        // around the monitor while in borderless fullscreen. ORDER
        // MATTERS: entering strips the frame BEFORE the OS swap; exit
        // restores it AFTER set_fullscreen(None) — restoring while the
        // window is still monitor-sized made DWM paint a hairline
        // border around the whole display during the exit transition.
        // Phase 11 also disables the Win11 DWM hairline border and
        // rounded corners while immersive (both re-appear on top-level
        // windows during style transitions otherwise).
        let hwnd_raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
        let hwnd = if hwnd_raw != 0 {
            Some(HWND(hwnd_raw as *mut core::ffi::c_void))
        } else {
            None
        };
        if self.is_fullscreen {
            if let Some(hwnd) = hwnd {
                unsafe {
                    set_window_thick_frame(hwnd, false);
                    set_dwm_fullscreen_chrome(hwnd, false);
                }
            }
        }
        window.set_fullscreen(if self.is_fullscreen {
            Some(winit::window::Fullscreen::Borderless(None))
        } else {
            None
        });
        if !self.is_fullscreen {
            if let Some(hwnd) = hwnd {
                unsafe {
                    set_window_thick_frame(hwnd, true);
                    set_dwm_fullscreen_chrome(hwnd, true);
                }
            }
        }
        // Phase 11: supply the fullscreen animation TARGET in window
        // coords, computed from the FINAL panel layout (user widths).
        // The image's path is then independent of the tree/thumb
        // collapse animation — the viewport churns underneath while
        // the image glides straight to its settled position.
        //
        // The previous code called `set_viewport_target` which feeds
        // a `pending_viewport_target` consumed by `viewer.resize()`.
        // But `resize` ALSO calls `compute_fit()` immediately, which
        // clobbers the rect_anim's `from` state. The image therefore
        // teleports instead of animating, and the post-fit position
        // is computed against the still-narrow window before the
        // OS finishes maximising it. Phase 0 fix: synchronously
        // write the new viewport + re-fit, leaving the rect_anim
        // path animation for Tier 1.
        //
        // Use the CLIENT rect (Win32) for the fullscreen size — winit's
        // inner_size() reports it scaled by the DPI factor, which would
        // configure a surface larger than the display and shift the image.
        let isize = window.inner_size();
        let fallback = (isize.width.max(1), isize.height.max(1));
        let hwnd_raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
        let (csw, csh) = Self::client_area_physical_using(hwnd_raw, fallback);
        if self.is_fullscreen {
            if let Some(v) = &self.viewer {
                let mut g = v.lock();
                g.set_viewport_physical(csw, csh, 0.0, 0.0);
                g.fit_to_screen();
            }
        } else if let Some(v) = &self.viewer {
            // Exit: re-fit into the now-restored windowed viewport. The
            // next build_egui_ui will call set_viewport_physical with
            // the real (now restored) egui CentralPanel size.
            v.lock().fit_to_screen();
        }
    }

    /// Set the current image as desktop wallpaper (single monitor via SystemParametersInfo).
    fn set_wallpaper(&self, path: PathBuf) {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_SETDESKWALLPAPER,
                0,
                Some(wide.as_ptr() as *mut _),
                SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
            )
        };
        if ok.is_err() {
            tracing::warn!("set_wallpaper failed for {}: {:?}", path.display(), ok);
        } else {
            tracing::info!("wallpaper set: {}", path.display());
        }
    }

    /// Reveal a path in Windows Explorer — the SINGLE shared behavior for
    /// every "open in explorer" entry point (Ctrl+E, context menus):
    /// directories open directly; files open with the file selected.
    fn reveal_in_explorer(&self, path: &Path) {
        use std::process::Command;
        if path.is_dir() {
            let _ = Command::new("explorer").arg(path).spawn();
        } else {
            // "/select,<path>" must be a SINGLE argument.
            let _ = Command::new("explorer")
                .arg(format!("/select,{}", path.display()))
                .spawn();
        }
    }

    /// True when the folder directly contains at least one supported image.
    fn folder_has_images(p: &Path) -> bool {
        aperture_core::fs::enumerate_images(p)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// The logical width currently used to hit-test the edge drawer. This is
    /// the width that's actually DRAWN on screen (`drawer_draw.1`, i.e.
    /// `drawer_width_anim` scaled by the slide `frac`), so every consumer —
    /// click-outside-close, viewer-exclusion, wheel-over-drawer — tests the
    /// same boundary and they can never disagree about whether a point is on
    /// the drawer. Falls back to the animated width during the brief
    /// retracted/open frames where `drawer_draw` is `None` (the handle-only
    /// case at `drawer_anim <= 0.001`), where the drawer body isn't hit-testable
    /// anyway.
    fn drawer_hit_width(&self) -> f32 {
        self.drawer_draw.map(|(_, dw, _, _)| dw).unwrap_or(self.drawer_width_anim)
    }

    /// Which panel edge (if any) is within the drag hit-zone at this
    /// physical cursor position. 0 = tree right edge, 1 = thumbs left edge.
    fn panel_edge_at(&self, cx: f32, cy: f32) -> Option<u8> {
        // Classic tree panel edge (edge 0) — only when the panel is open.
        let (tx, ty, tw, th) = self.tree_rect_phys;
        if self.show_tree && tw > 0.0 && cx >= tx + tw - 6.0 && cx <= tx + tw + 4.0 && cy >= ty && cy <= ty + th {
            return Some(0);
        }
        // Edge drawer: when open (classic tree closed) and mostly slid out,
        // its right edge is draggable to resize `drawer_user_width`. Uses the
        // animated width (what's drawn) and converts the physical cursor to
        // logical for the comparison.
        if !self.show_tree && self.drawer_open && self.drawer_anim > 0.5 {
            let ppp = self.wgpu_state.as_ref().map(|w| w.pixels_per_point).unwrap_or(1.0).max(0.1);
            let win_h = self.wgpu_state.as_ref().map(|w| w.config.height).unwrap_or(800);
            let top = TOOLBAR_HEIGHT as f32;
            let bottom = (win_h as f32 / ppp) - STATUS_BAR_HEIGHT as f32;
            let dcx = cx as f32 / ppp;
            let dcy = cy as f32 / ppp;
            let dw = self.drawer_width_anim;
            if dw > 0.0 && dcx >= dw - 6.0 && dcx <= dw + 4.0 && dcy >= top && dcy <= bottom {
                return Some(0);
            }
        }
        let (hx, hy, hw, hh) = self.thumb_rect_phys;
        if hw > 0.0 && cx >= hx - 4.0 && cx <= hx + 6.0 && cy >= hy && cy <= hy + hh {
            return Some(1);
        }
        None
    }

    /// Phase 8: image right-click context menu. Stores the cursor
    /// position in `ctx_menu`; the actual popup is drawn by
    /// `draw_context_menu` inside `build_egui_ui`. Replacing
    /// `ctx_menu` on a new right-click automatically closes any
    /// previously-open popup and reopens at the new position.
    ///
    /// `cursor` arrives in PHYSICAL pixels (from the router's
    /// `cursor_pos`). egui::Window::fixed_pos expects LOGICAL points, so
    /// we convert here before storing. Storing the raw physical value put
    /// the popup WAY off to the bottom-right (the whole window is ~1843
    /// physical wide, so a click at physical x=1100 got placed at logical
    /// x=1100 which is ~2× the logical width — off-screen).
    fn open_image_context_menu(&mut self, cursor: egui::Pos2) {
        if self.nav.lock().current().is_none() { return; }
        let ppp = self.wgpu_state
            .as_ref()
            .map(|w| w.pixels_per_point)
            .unwrap_or(1.0)
            .max(0.1);
        let logical = egui::pos2(cursor.x / ppp, cursor.y / ppp);
        self.ctx_menu = Some(CtxMenu::Image { pos: logical });
        self.ctx_menu_open_at = Some(std::time::Instant::now());
        if let Some(es) = self.egui_state.as_ref() {
            es.ctx.request_repaint();
        }
    }

    /// Copy text to the clipboard (CF_UNICODETEXT).
    fn copy_text_to_clipboard(&self, text: &str) {
        use windows::Win32::System::DataExchange::{
            OpenClipboard, EmptyClipboard, SetClipboardData, CloseClipboard,
        };
        use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock,
            GMEM_MOVEABLE};
        use windows::Win32::System::Ole::CF_UNICODETEXT;
        use windows::Win32::Foundation::HANDLE;
        let hwnd_raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
        if hwnd_raw == 0 { return; }
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        unsafe {
            if OpenClipboard(hwnd).is_err() {
                tracing::warn!("clipboard: OpenClipboard failed");
                return;
            }
            let _ = EmptyClipboard();
            let mut wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = wide.len() * 2;
            let Some(h) = GlobalAlloc(GMEM_MOVEABLE, bytes).ok() else {
                let _ = CloseClipboard();
                return;
            };
            let dst = GlobalLock(h);
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_mut_ptr(), dst as *mut u16, wide.len());
                let _ = GlobalUnlock(h);
                if SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(h.0)).is_err() {
                    tracing::warn!("clipboard: SetClipboardData failed");
                }
            }
            let _ = CloseClipboard();
        }
    }

    /// Print via the Shell "print" verb (opens the system photo-print flow).
    fn print_image(&self, path: PathBuf) {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use std::os::windows::ffi::OsStrExt;
        let verb: Vec<u16> = "print".encode_utf16().chain(std::iter::once(0)).collect();
        let file: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let hwnd_raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        let result = unsafe {
            ShellExecuteW(
                hwnd,
                windows::core::PCWSTR(verb.as_ptr()),
                windows::core::PCWSTR(file.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            tracing::warn!("print: ShellExecute failed for {}", path.display());
        }
    }

    /// Handle a file/folder dropped onto the window: an image navigates to
    /// its parent folder and selects that file; a folder navigates into it.
    fn handle_dropped_file(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.navigate_to_folder(path);
            return;
        }
        if !aperture_core::SupportedFormats::is_supported(&path) {
            tracing::warn!("dropped file not a supported image: {}", path.display());
            return;
        }
        let Some(parent) = path.parent().map(|p| p.to_path_buf()) else { return; };
        // Show the side panels so the user sees the tree + thumbs context.
        self.show_tree = true;
        self.show_thumbs = true;
        if let Err(e) = self.nav.lock().navigate_folder(parent.clone()) {
            tracing::error!("drop: navigate_folder({}) failed: {:#}", parent.display(), e);
            return;
        }
        // Locate the dropped file within the folder and select it.
        let idx = self.nav.lock().items().iter().position(|it| it.path == path);
        if let Some(i) = idx {
            self.nav.lock().set_index(i);
        }
        if Self::folder_has_images(&parent) {
            self.settings.push_recent_folder(parent.clone());
        }
        self.settings.set_last_folder(parent);
        self.file_tree.refresh_recent(&self.settings.recent_folders());
        if let Some(coordinator) = &self.coordinator {
            coordinator.request_current(SlideDir::None);
        }
        if let Some(window) = &self.window { window.request_redraw(); }
    }

    fn save_window_geometry(&mut self) {
        if let Some(window) = &self.window {
            let size = window.inner_size();
            // Store the size in LOGICAL pixels. winit's
            // Window::inner_size returns physical pixels; if we save
            // those as-is, the next launch reads them back through
            // LogicalSize::new (window.rs:init_window), winit
            // applies the current scale factor, and the resulting
            // physical window is `previous_physical * scale / scale`
            // i.e. `previous_physical * 1` at 100% DPI but
            // `previous_physical * 1.25` at 125% DPI — the window
            // GROWS each launch until capped at monitor size. This
            // was the user's "open → close → open, the window is
            // bigger every time" symptom.
            let sf = window.scale_factor();
            let logical = size.to_logical::<f64>(sf);
            self.settings.set_window_size(logical.width as u32, logical.height as u32);
        }
    }

    /// Populate the wgpu surface's back buffer with canvas_clear and
    /// Present once. Called from `init_renderer` so the main HWND's
    /// DXGI swapchain has content for the very first DWM
    /// composition — otherwise the first sample falls back to
    /// COLOR_WINDOW and the user sees a "placeholder" flash on
    /// launch. Silent on failure (errors are logged) because the
    /// next render_frame will repaint anyway.
    fn present_wgpu_surface_for_init(&self) {
        let Some(wgpu_state) = self.wgpu_state.as_ref() else {
            return;
        };
        let pal = self.pal();
        let frame = match wgpu_state.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("init_present: get_current_texture failed: {:#}", e);
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = wgpu_state.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("init_present_encoder"),
        });
        // Same sRGB-aware clear logic as in render_frame — see comment there.
        let clear_color = if wgpu_state.surface_is_srgb {
            wgpu::Color {
                r: srgb_to_linear(pal.canvas_clear.0),
                g: srgb_to_linear(pal.canvas_clear.1),
                b: srgb_to_linear(pal.canvas_clear.2),
                a: 1.0,
            }
        } else {
            wgpu::Color {
                r: pal.canvas_clear.0,
                g: pal.canvas_clear.1,
                b: pal.canvas_clear.2,
                a: 1.0,
            }
        };
        let _rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("init_present"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear_color),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        drop(_rpass);
        wgpu_state.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
    }
}

impl ApplicationHandler for MainWindow {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            if let Err(e) = self.init_window(event_loop) {
                tracing::error!("Failed to initialize window: {:#}", e);
                event_loop.exit();
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // ---- App-level keyboard shortcuts (handled directly) ----
        if let WindowEvent::KeyboardInput { event: key_event, .. } = &event {
            // ---- Arrow-key hold via KEYUP (the "brake") ----
            //
            // Hold-to-page: a fresh KEYDOWN jumps on the down-stroke (with
            // the iOS slide), records the direction into `nav_intent`. The
            // fixed-rate dispatcher then advances on a steady `HOLD_NAV_INTERVAL`
            // pulse while the key stays down, each step cutting directly
            // (SlideDir::None, no slide) and hitting the pre-decode cache —
            // the fast "walk the gallery" mode. Stops exactly on KEYUP.
            //
            //   * KEYDOWN for an arrow: fresh press (no intent) fires
            //     handle_navigation immediately; always records the
            //     direction. Auto-repeat KEYDOWNs re-assert the intent
            //     (they do NOT fire handle_navigation directly).
            //   * KEYUP for an arrow: clears `nav_intent` + `nav_last_step`.
            //     The next frame's dispatcher sees no intent and stops.
            //   * Per-frame dispatcher (in render_frame): if `nav_intent`
            //     is Some AND the fixed-rate pulse is due, fire one more
            //     handle_navigation (no slide).
            if matches!(key_event.state, ElementState::Released) {
                if let PhysicalKey::Code(code) = key_event.physical_key {
                    let dir = match code {
                        KeyCode::ArrowLeft | KeyCode::ArrowUp => Some(NavigationDirection::Previous),
                        KeyCode::ArrowRight | KeyCode::ArrowDown => Some(NavigationDirection::Next),
                        _ => None,
                    };
                    if let Some(dir) = dir {
                        // Clear the held intent so the fixed-rate dispatcher
                        // stops advancing. We do NOT call handle_navigation
                        // here — the dispatcher next frame sees no intent
                        // and stops. Also reset the step timer so a fresh
                        // hold starts its own pulse.
                        if let Some((held, _)) = self.nav_intent {
                            if held == dir {
                                self.nav_intent = None;
                                self.nav_last_step = None;
                                self.nav_press_at = None;
                                return;
                            }
                        }
                    }
                }
            }

            if matches!(key_event.state, ElementState::Pressed) {
                if let PhysicalKey::Code(code) = key_event.physical_key {
                    // Persistent modifier state — survives frame drains
                    // (the batch copy is cleared by take_pending).
                    let ctrl_pressed = self.router.modifiers.ctrl;
                    tracing::debug!("key {:?} repeat={} ctrl={}", code, key_event.repeat, ctrl_pressed);
                    let mut consumed = true;
                    match code {
                        KeyCode::ArrowLeft if ctrl_pressed => self.handle_cycle_folder(-1),
                        KeyCode::ArrowRight if ctrl_pressed => self.handle_cycle_folder(1),
                        KeyCode::ArrowUp if ctrl_pressed => self.handle_cycle_folder(-1),
                        KeyCode::ArrowDown if ctrl_pressed => self.handle_cycle_folder(1),
                        KeyCode::ArrowLeft => {
                            self.on_arrow_keydown(NavigationDirection::Previous, SlideDir::Previous);
                        }
                        KeyCode::ArrowRight => {
                            self.on_arrow_keydown(NavigationDirection::Next, SlideDir::Next);
                        }
                        KeyCode::ArrowUp => {
                            self.on_arrow_keydown(NavigationDirection::Previous, SlideDir::Previous);
                        }
                        KeyCode::ArrowDown => {
                            self.on_arrow_keydown(NavigationDirection::Next, SlideDir::Next);
                        }
                        KeyCode::PageUp => self.handle_navigation_jump(-1),
                        KeyCode::PageDown => self.handle_navigation_jump(1),
                        KeyCode::Home => {
                            self.handle_navigation(NavigationDirection::First, SlideDir::None);
                        }
                        KeyCode::End => {
                            self.handle_navigation(NavigationDirection::Last, SlideDir::None);
                        }
                        KeyCode::KeyF if ctrl_pressed => {
                            // Ctrl+F → fullscreen (must precede plain F/fit).
                            self.action_toggle_fullscreen();
                        }
                        KeyCode::KeyF => {
                            if let Some(v) = &self.viewer {
                                v.lock().fit_to_screen();
                            }
                            if let Some(window) = &self.window { window.request_redraw(); }
                        }
                        KeyCode::Digit0 if ctrl_pressed => {
                            // Ctrl+0 → fit to window.
                            if let Some(v) = &self.viewer {
                                v.lock().fit_to_screen();
                            }
                            if let Some(window) = &self.window { window.request_redraw(); }
                        }
                        KeyCode::Equal | KeyCode::NumpadAdd if ctrl_pressed => {
                            if let Some(v) = &self.viewer {
                                v.lock().zoom_step(1.25);
                            }
                            if let Some(window) = &self.window { window.request_redraw(); }
                        }
                        KeyCode::Minus | KeyCode::NumpadSubtract if ctrl_pressed => {
                            if let Some(v) = &self.viewer {
                                v.lock().zoom_step(1.0 / 1.25);
                            }
                            if let Some(window) = &self.window { window.request_redraw(); }
                        }
                        KeyCode::Escape => {
                            if self.is_fullscreen {
                                self.action_toggle_fullscreen();
                            } else {
                                self.save_window_geometry();
                                event_loop.exit();
                            }
                        }
                        KeyCode::KeyW if ctrl_pressed => {
                            // Ctrl+W → Set wallpaper
                            self.actions.push(UiAction::SetWallpaper);
                        }
                        KeyCode::KeyE if ctrl_pressed => {
                            // Ctrl+E → Open in Explorer
                            self.actions.push(UiAction::OpenInExplorer);
                        }
                        KeyCode::KeyD if ctrl_pressed => {
                            // Ctrl+D → Toggle favorite
                            self.actions.push(UiAction::ToggleFavorite);
                        }
                        KeyCode::Slash if ctrl_pressed => {
                            // Ctrl+/ → Toggle shortcut help
                            self.actions.push(UiAction::ToggleShortcutHelp);
                        }
                        KeyCode::KeyR if ctrl_pressed => {
                            // Ctrl+R → rotate clockwise
                            self.actions.push(UiAction::RotateImage(1));
                        }
                        _ => consumed = false,
                    }
                    if consumed {
                        return;
                    }
                }
            }
        }

        // ---- Drag & drop: open a dropped image / folder ----
        if let WindowEvent::DroppedFile(path) = &event {
            self.handle_dropped_file(path.clone());
            return;
        }

        // ---- Cursor movement: image pan, panel drag, edge hover, wake ----
        if let WindowEvent::CursorMoved { position, .. } = &event {
            let (x, y) = (position.x as f32, position.y as f32);
            let prev = self.last_cursor;
            self.last_cursor = Some((x, y));

            // Image drag-pan (left button held over the viewer).
            if self.pan_active {
                if let Some(viewer) = &self.viewer {
                    viewer.lock().on_pan(x - self.pan_last.0, y - self.pan_last.1);
                }
                self.pan_last = (x, y);
                if let Some(w) = &self.window { w.request_redraw(); }
            }

            // Panel width drag in progress (native state machine — ends
            // reliably on Left release).
            if let Some(panel) = self.drag_panel {
                if let Some((lx, _)) = prev {
                    let dx = x - lx;
                    let ppp = self.router.pixels_per_point.max(0.1);
                    match panel {
                        0 => {
                            if self.show_tree {
                                self.tree_panel.apply_drag(dx / ppp, 170.0, 420.0);
                            } else {
                                // Drawer active: drag its right edge. Only the
                                // BASE width is clamped; content can still widen
                                // it past this via drawer_content_min.
                                self.drawer_user_width =
                                    (self.drawer_user_width + dx / ppp).clamp(DRAWER_MIN_W, DRAWER_MAX_W);
                            }
                        }
                        1 => self.thumb_panel.apply_drag(-dx / ppp, 180.0, 560.0),
                        _ => {}
                    }
                    if let Some(w) = &self.window { w.request_redraw(); }
                }
            } else if !self.is_fullscreen {
                // Edge hover: highlight + resize cursor affordance.
                let edge = self.panel_edge_at(x, y);
                if edge != self.panel_edge_hover {
                    self.panel_edge_hover = edge;
                    if let Some(w) = &self.window {
                        let icon = match edge {
                            Some(0) => winit::window::CursorIcon::EwResize,
                            Some(1) => winit::window::CursorIcon::EwResize,
                            _ => winit::window::CursorIcon::Default,
                        };
                        w.set_cursor(icon);
                    }
                }
            }

            // Fullscreen chrome wake-up on significant mouse movement.
            if self.is_fullscreen {
                if let Some((lx, ly)) = prev {
                    self.chrome_move_accum += ((x - lx).powi(2) + (y - ly).powi(2)).sqrt();
                }
                // Minimum movement threshold so jitter doesn't wake the UI.
                if self.chrome_move_accum > 8.0 {
                    self.chrome_visible = true;
                    self.chrome_hide_at = Some(std::time::Instant::now());
                    self.chrome_move_accum = 0.0;
                    if let Some(w) = &self.window { w.request_redraw(); }
                }
            }
        }

        // ---- Wheel: route to tree/thumb scroll OR viewer zoom ----
        // Phase X: tree panel and thumb panel each contain an
        // egui::ScrollArea. Without this check, every wheel event is
        // consumed by the viewer zoom handler and the panels become
        // un-scrollable.
        if let WindowEvent::MouseWheel { delta, .. } = &event {
            let cursor = self.router.cursor_pos;
            let (tx, ty, tw, th) = self.tree_rect_phys;
            let (hx, hy, hw, hh) = self.thumb_rect_phys;
            let in_tree = self.show_tree
                && tw > 0.0 && th > 0.0
                && cursor.x >= tx && cursor.x <= tx + tw
                && cursor.y >= ty && cursor.y <= ty + th;
            // Edge drawer: when it's open (classic tree closed), scrolling
            // over it advances the tree's ScrollArea too. The drawer reuses
            // the SAME FileTree state. Its coords are LOGICAL (drawer_draw),
            // so convert the physical cursor to logical before testing.
            let dp_ppp = self.wgpu_state.as_ref().map(|w| w.pixels_per_point).unwrap_or(1.0).max(0.1);
            let dcx = cursor.x as f32 / dp_ppp;
            let dcy = cursor.y as f32 / dp_ppp;
            let in_drawer = !self.show_tree
                && self.drawer_open
                && self.drawer_draw
                    .map(|(_, dw, dt_, dh)| dcx >= 0.0 && dcx <= dw
                        && dcy >= dt_ && dcy <= dt_ + dh)
                    .unwrap_or(false);
            let in_thumbs = self.show_thumbs
                && hw > 0.0 && hh > 0.0
                && cursor.x >= hx && cursor.x <= hx + hw
                && cursor.y >= hy && cursor.y <= hy + hh;
            if in_tree || in_drawer || in_thumbs {
                // Advance the panel's ScrollArea directly. The ScrollArea
                // builder in build_egui_ui applies `vertical_scroll_offset`
                // from these sticky fields each frame, so a wheel here
                // scrolls the tree/thumbnail list even though the cursor
                // never "enters" the egui widget that owns the area.
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y * 48.0,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                // A generous upper bound so a single huge wheel step can't
                // push the sticky offset unbounded; the ScrollArea readback
                // re-clamps to the real content range next frame anyway.
                const MAX_STICKY: f32 = 1.0e7;
                if in_tree {
                    let mut s = self.file_tree.state.lock();
                    s.scroll_offset_y = (s.scroll_offset_y - dy).clamp(0.0, MAX_STICKY);
                }
                if in_drawer {
                    // The drawer shares the classic tree's FileTree/ScrollArea
                    // state, so scrolling it advances the same offset.
                    let mut s = self.file_tree.state.lock();
                    s.scroll_offset_y = (s.scroll_offset_y - dy).clamp(0.0, MAX_STICKY);
                }
                if in_thumbs {
                    THUMB_SCROLL_Y.with(|c| {
                        c.set((c.get() - dy).clamp(0.0, MAX_STICKY));
                    });
                }
                if let Some(es) = self.egui_state.as_ref() {
                    es.ctx.request_repaint();
                }
                return;
            }
            let dy = match delta {
                MouseScrollDelta::LineDelta(_, y) => *y,
                MouseScrollDelta::PixelDelta(p) => p.y as f32 / 100.0,
            };
            self.handle_wheel_in_viewer(dy, cursor.x as i32, cursor.y as i32);
            return;
        }

        // ---- Mouse input: pan, panel-edge drag start/end, dbl-click ----
        if let WindowEvent::MouseInput { state, button, .. } = &event {
            let cursor = self.router.cursor_pos;

            // Click-outside-to-hide the fullscreen toolbar: any left press
            // outside the floating bar's rect immediately hides the toolbar.
            // (Pressing the bar's own buttons is inside `toolbar_rect`, so
            // the bar stays; pressing the image/elsewhere dismisses it.)
            if self.is_fullscreen
                && self.chrome_visible
                && matches!(button, MouseButton::Left)
                && matches!(state, ElementState::Pressed)
            {
                let ppp = self.wgpu_state.as_ref().map(|w| w.pixels_per_point).unwrap_or(1.0).max(0.1);
                let clx = cursor.x as f32 / ppp;
                let cly = cursor.y as f32 / ppp;
                if let Some(bar) = self.fullscreen_bar_rect {
                    let inside = bar.contains(egui::pos2(clx, cly));
                    if !inside {
                        self.chrome_visible = false;
                        self.chrome_hide_at = None;
                    }
                }
            }

            // Panel edge drag: press on an edge → start; release → end.
            if matches!(button, MouseButton::Left) {
                if matches!(state, ElementState::Released) {
                    self.pan_active = false;
                    if self.drag_panel.take().is_some() {
                        return;
                    }
                }
                if matches!(state, ElementState::Pressed) && !self.is_fullscreen {
                    if let Some(edge) = self.panel_edge_at(cursor.x, cursor.y) {
                        // The shortcuts popover's hole region can't track
                        // the child while the layout moves — close it.
                        self.close_shortcut_help();
                        self.drag_panel = Some(edge);
                        self.panel_edge_hover = Some(edge);
                        return;
                    }
                }
                // Click-outside-to-close the edge drawer: any left press
                // outside the drawer body (when it's open, not pinned) slides
                // it back. Pinned drawers and in-progress edge resizes ignore
                // outside-clicks. Tests against the DRAWN width (the same
                // `drawer_hit_width` the viewer-exclusion check below uses),
                // so a point on the visible drawer is never treated as outside,
                // and the two cannot disagree in the same click.
                if matches!(state, ElementState::Pressed)
                    && self.drawer_open
                    && !self.drawer_pinned
                    && self.drag_panel.is_none()
                {
                    let ppp = self.wgpu_state.as_ref().map(|w| w.pixels_per_point).unwrap_or(1.0).max(0.1);
                    let win_h = self.wgpu_state.as_ref().map(|w| w.config.height).unwrap_or(800);
                    let top = TOOLBAR_HEIGHT as f32;
                    let bottom = (win_h as f32 / ppp) - STATUS_BAR_HEIGHT as f32;
                    // Logical cursor (drawer geometry is logical).
                    let clx = cursor.x as f32 / ppp;
                    let cly = cursor.y as f32 / ppp;
                    let inside = clx <= self.drawer_hit_width()
                        && cly >= top
                        && cly <= bottom;
                    if !inside {
                        self.drawer_open = false;
                        self.drawer_leave_at = None;
                    }
                }
            }

            // Check if cursor is within the viewer area (central panel).
            let in_viewer = if let Some(window) = &self.window {
                let size = window.inner_size();
                let (vx, vy, vw, vh) = self.compute_viewer_rect(size.width, size.height);
                let cx = cursor.x as i32;
                let cy = cursor.y as i32;
                let viewer_hit = cx >= vx && cx < vx + vw as i32 && cy >= vy && cy < vy + vh as i32;
                // If the edge drawer is open and the click lands inside it,
                // do NOT treat it as a viewer click (pan/double-click). The
                // tree menu is an overlay on top of the viewer — its clicks
                // must reach egui, not be consumed by the pan branch below.
                let ppp = self.wgpu_state.as_ref().map(|w| w.pixels_per_point).unwrap_or(1.0).max(0.1);
                let clx = cursor.x as f32 / ppp;
                let cly = cursor.y as f32 / ppp;
                let over_drawer = self.drawer_open && self.drawer_anim > 0.001 && {
                    // Same DRAWN width as the click-close test above, so these
                    // always agree; a click on the drawer body never reaches
                    // the pan double-click branch below.
                    let dw = self.drawer_hit_width();
                    let (top, hh) = self.drawer_draw.map(|(_, _, t, h)| (t, h)).unwrap_or((TOOLBAR_HEIGHT as f32, (self.viewport_h as f32) - STATUS_BAR_HEIGHT as f32));
                    clx <= dw && cly >= top && cly <= top + hh
                };
                viewer_hit && !over_drawer
            } else {
                false
            };

            // Right-click over the image → native context menu.
            if in_viewer
                && matches!(button, MouseButton::Right)
                && matches!(state, ElementState::Pressed)
            {
                // Phase 16: right-click over the image posts a custom
                // event; the native TrackPopupMenu pops in user_event.
                // cursor (the router position) is already PHYSICAL — the
                // Win32 menu anchor needs physical screen coords.
                self.open_image_context_menu(cursor);
                return;
            }

            if in_viewer && matches!(button, MouseButton::Left) {
                if matches!(state, ElementState::Pressed) {
                    // A left-click in the viewer while a context menu or
                    // the keyboard-shortcuts popover is open dismisses it.
                    // Both rely on egui's outside-click detection
                    // (`any_click()` / `clicked_elsewhere()`), which can't
                    // see viewer clicks because the pan branch below
                    // consumes them before they reach egui. Don't start a
                    // drag on this click — it's a dismiss click, not a pan.
                    if self.ctx_menu.is_some() || self.show_shortcut_help {
                        self.ctx_menu = None;
                        self.ctx_menu_open_at = None;
                        self.show_shortcut_help = false;
                        return;
                    }
                    // Start image drag-pan (double-click detection still runs).
                    self.pan_active = true;
                    self.pan_last = (cursor.x, cursor.y);
                    let now = std::time::Instant::now();
                    if let Some(prev) = self.last_double_click {
                        if now.duration_since(prev).as_millis() < 350 {
                            self.handle_double_click();
                            self.last_double_click = None;
                        } else {
                            self.last_double_click = Some(now);
                        }
                    } else {
                        self.last_double_click = Some(now);
                    }
                }
                return;
            }
            if in_viewer {
                return;
            }
        }

        // ---- Everything else feeds egui's accumulated RawInput ----
        match event {
            WindowEvent::CloseRequested => {
                // Phase 3: stop the slide-show timer so the
                // background tick doesn't try to navigate after
                // the window is being torn down.
                self.slide_show_running = false;
                self.slide_show_last = None;
                self.save_window_geometry();
                event_loop.exit();
            }

            WindowEvent::Resized(new_size) => {
                // winit 0.30 reports the resized PhysicalSize scaled by the
                // DPI factor in borderless fullscreen (e.g. 2560x1440 for a
                // 2048x1152 monitor at 125%). Prefer the Win32 CLIENT rect
                // so the surface matches the real display; keep the winit
                // size only before the renderer exists (initial launch,
                // where winit's value is correct for windowed mode).
                let (w, h) = if self.wgpu_state.is_some() {
                    let fallback = (new_size.width.max(1), new_size.height.max(1));
                    let hwnd_raw = MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed);
                    Self::client_area_physical_using(hwnd_raw, fallback)
                } else {
                    (new_size.width.max(1), new_size.height.max(1))
                };

                // First Resized: create the wgpu/egui/d2d stack at the
                // authoritative OS-reported size.
                if self.wgpu_state.is_none() {
                    if let Err(e) = self.init_renderer(w, h) {
                        tracing::error!("init_renderer failed: {:#}", e);
                    }
                    return;
                }

                if let Some(wgpu_state) = &mut self.wgpu_state {
                    wgpu_state.config.width = w;
                    wgpu_state.config.height = h;
                    wgpu_state.surface.configure(&wgpu_state.device, &wgpu_state.config);
                }

                // The pending egui frame's `ScreenDescriptor` was built
                // against the previous size. If submit_wgpu_frame ran
                // before the next build_egui_ui, the scissor/clip_rect
                // would be scaled by the OLD pixels-per-pixel value
                // against the NEW surface texture, producing scissor
                // rectangles that wgpu rejects with a validation error
                // ("scissor rect not contained in render target").
                // Drop the pending output — the next build_egui_ui
                // will refill it with the new size.
                self._pending_egui_output = None;

                let (vx, vy, vw, vh) = self.compute_viewer_rect(w, h);
                let _ = (vx, vy);
                self.viewport_w = vw;
                self.viewport_h = vh;
                if let Some(viewer) = &self.viewer {
                    viewer.lock().set_viewport_physical(vw, vh, vx as f32, vy as f32);
                }
            }

            WindowEvent::RedrawRequested => {
                if let Err(e) = self.render_frame() {
                    tracing::error!("Render error: {:#}", e);
                }
                // Phase 5 → 7 修复: 回滚到 ControlFlow::Poll
                // 行为。Phase 5 用 needs_redraw 标志 + Wait 来
                // 减少空闲 CPU，但实际效果不稳定 —— 14+ 个
                // request_redraw 调用点只要漏掉一个就会让 UI
                // 看起来"卡死"（输入事件被丢弃，hover/click
                // 反馈不显示）。回滚为简单的 Poll + 每帧无条件
                // request_redraw：现代硬件上空闲帧 CPU < 1%。
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::Focused(false) => {
                // Phase 3: pause the slide-show when the window loses
                // focus so a hidden window doesn't keep paging
                // through images. The user can re-toggle play/pause
                // to resume; we don't auto-resume on focus regain
                // (the user explicitly chose to background the app).
                self.slide_show_running = false;
                self.slide_show_last = None;
                if let Some(window) = &self.window { window.request_redraw(); }
            }

            ref ev @ (WindowEvent::CursorMoved { .. }
                | WindowEvent::CursorLeft { .. }
                | WindowEvent::ModifiersChanged(_)
                | WindowEvent::Focused(_)
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::KeyboardInput { .. }) => {
                event_router::forward_to_egui(&mut self.router, ev);
                // Phase 5 follow-up: any input event mutates
                // egui's internal state (hover, click, drag,
                // text). The next render is what makes those
                // changes visible — without re-arming here, the
                // UI would respond to clicks (egui's input is
                // updated) but the visual hover/press feedback
                // would never appear (no frame is requested).
                // This was the root cause of the 'all content
                // is unclickable' symptom reported after
                // shipping the Phase 5 commit.
                if let Some(window) = &self.window { window.request_redraw(); }
            }

            _ => {}
        }
    }
}

/// Original winit window procedure — resize hook forwards everything else.
static ORIG_WNDPROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// Main window HWND (isize) for Win32 calls from app code.
static MAIN_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// Mirrors MainWindow.is_fullscreen for the resize hook.
pub static IS_FULLSCREEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Which tree root the user last selected a folder in (0/1/2) —
/// controls Ctrl+Arrow cycling and the single-highlight behavior.
pub static ACTIVE_ROOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(2);

unsafe extern "system" fn resize_hook_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    match msg {
        // Client area covers the entire window (borderless technique).
        WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
        WM_NCHITTEST => {
            // No edge zones while fullscreen (immersive) or maximized.
            let fullscreen = IS_FULLSCREEN.load(std::sync::atomic::Ordering::Relaxed);
            let mut wr = windows::Win32::Foundation::RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            let ww = (wr.right - wr.left) as f32;
            let wh = (wr.bottom - wr.top) as f32;
            if fullscreen || IsZoomed(hwnd).as_bool() || ww <= 0.0 || wh <= 0.0 {
                return LRESULT(1); // HTCLIENT
            }
            let dpi = GetDpiForWindow(hwnd).max(96);
            let m = 8.0 * (dpi as f32 / 96.0); // edge zone width, physical px
            let x = ((lparam.0 as usize) & 0xFFFF) as u16 as i16 as i32 as f32;
            let y = (((lparam.0 as usize) >> 16) & 0xFFFF) as u16 as i16 as i32 as f32;
            let left = x < wr.left as f32 + m;
            let right = x > wr.right as f32 - m;
            let top = y < wr.top as f32 + m;
            let bottom = y > wr.bottom as f32 - m;
            // HT* values: LEFT=10 RIGHT=11 TOP=12 TOPLEFT=13 TOPRIGHT=14
            // BOTTOM=15 BOTTOMLEFT=16 BOTTOMRIGHT=17
            let ht: i32 = match (left, right, top, bottom) {
                (true, _, true, _) => 13,
                (_, true, true, _) => 14,
                (true, _, _, true) => 16,
                (_, true, _, true) => 17,
                (true, _, _, _) => 10,
                (_, true, _, _) => 11,
                (_, _, true, _) => 12,
                (_, _, _, true) => 15,
                _ => return LRESULT(1),
            };
            LRESULT(ht as isize)
        }
        _ => {
            let orig = ORIG_WNDPROC.load(std::sync::atomic::Ordering::Relaxed);
            if orig != 0 {
                CallWindowProcW(
                    Some(std::mem::transmute(orig)),
                    hwnd, msg, wparam, lparam,
                )
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }
}

/// Install the borderless edge-resize hook on the main window.
unsafe fn install_resize_hook(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_WNDPROC};
    let prev = SetWindowLongPtrW(
        hwnd,
        GWLP_WNDPROC,
        resize_hook_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT
            as isize,
    );
    ORIG_WNDPROC.store(prev, std::sync::atomic::Ordering::Relaxed);
}

#[allow(dead_code)]
fn init_wgpu(window: &Window) -> Result<WgpuState> {
    let size = window.inner_size();
    tracing::info!("init_wgpu: window.inner_size = {}x{}, scale_factor={}",
        size.width, size.height, window.scale_factor());
    init_wgpu_at_size(window, size.width, size.height)
}

fn init_wgpu_at_size(window: &Window, width: u32, height: u32) -> Result<WgpuState> {
    // Box the instance to give it 'static lifetime — one per app run.
    let instance: Box<wgpu::Instance> = Box::new(wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..Default::default()
    }));
    let surface = instance.create_surface(window)?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    })).ok_or_else(|| anyhow::anyhow!("No GPU adapter"))?;
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("main device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
        },
        None,
    ))?;
    let caps = surface.get_capabilities(&adapter);
    // Prefer an sRGB-encoded format. D2D's swapchain is created with
    // DXGI_FORMAT_B8G8R8A8_UNORM and DXGI_ALPHA_MODE_IGNORE
    // (crates/gpu/src/swapchain.rs:38-44) — an sRGB surface matches
    // that encoding and lets the egui_wgpu pipeline's existing
    // sRGB→linear clear colour (window.rs: srgb_to_linear) produce
    // the intended panel_bg colour. If `caps.formats[0]` happens to
    // already be sRGB on a given driver the behaviour is unchanged;
    // on drivers that report a linear format first (e.g. some
    // DX12 paths report Bgra8Unorm before Bgra8UnormSrgb), this
    // prevents the cleared region from being over-darkened by
    // the srgb_to_linear conversion on a linear surface.
    let format = caps.formats.iter().copied()
        .find(|f| f.is_srgb())
        .unwrap_or(caps.formats[0]);
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width,
        height,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        desired_maximum_frame_latency: 1,
    };
    surface.configure(&device, &config);
    // SAFETY: the surface holds an Arc<Window> internally via window.clone(),
    // so the lifetime bound on Surface<'window> can be safely extended to 'static.
    let surface: wgpu::Surface<'static> = unsafe { std::mem::transmute(surface) };

    // Phase 2: Arc-wrap device + queue so the decode coordinator can
    // hold long-lived references for texture uploads.
    let device = Arc::new(device);
    let queue = Arc::new(queue);

    Ok(WgpuState {
        surface,
        device,
        queue,
        config,
        surface_format: format,
        surface_is_srgb: format.is_srgb(),
        pixels_per_point: window.scale_factor() as f32,
        _instance: instance,
    })
}

fn init_egui(device: &wgpu::Device, format: wgpu::TextureFormat) -> EguiState {    let ctx = egui::Context::default();
    apply_linear_dark_theme(&ctx, &dark_palette());
    install_fonts(&ctx);
    let renderer = egui_wgpu::Renderer::new(device, format, None, 1, false);
    EguiState { ctx, renderer }
}

/// Load a system CJK-capable font so non-Latin folder names render
/// instead of showing placeholder boxes.
/// Convert an sRGB-encoded channel value (0.0–1.0) to the linear-light value
/// that wgpu's `LoadOp::Clear` expects on an sRGB surface. Required because
/// wgpu passes the clear color through the surface's linear->sRGB encoder
/// on store; passing the sRGB fraction directly over-brightens dark/light.
fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// Toggle the standard resize grip frame around the main window. When
/// entering immersive (borderless fullscreen) we hide it so DWM does
/// not draw a thin border around the monitor; on exit we restore it.
unsafe fn set_window_thick_frame(hwnd: HWND, on: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_STYLE, WS_THICKFRAME,
        SetWindowPos, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_NOACTIVATE,
    };
    let prev = GetWindowLongPtrW(hwnd, GWL_STYLE);
    let new = if on { prev | WS_THICKFRAME.0 as isize } else { prev & !(WS_THICKFRAME.0 as isize) };
    SetWindowLongPtrW(hwnd, GWL_STYLE, new);
    let _ = SetWindowPos(
        hwnd, None, 0, 0, 0, 0,
        SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// Phase 11: disable the Win11 DWM hairline window border and rounded
/// corners while in borderless fullscreen; restore on exit.
///
/// - `DWMWA_BORDER_COLOR` (= 34): `DWMWA_COLOR_NONE` (0xFFFFFFFE)
///   removes the 1px theme-colored border Windows 11 draws around
///   top-level windows — it can survive a fullscreen style swap and
///   reads as a hairline frame around the monitor.
/// - `DWMWA_WINDOW_CORNER_PREFERENCE` (= 33): `DWMWCP_DONOTROUND` (1)
///   prevents rounded-corner notches at the four screen corners when
///   the window is monitor-sized.
///
/// Both attributes fail harmlessly on Windows 10 (unsupported attr →
/// error ignored), where neither artifact exists.
unsafe fn set_dwm_fullscreen_chrome(hwnd: HWND, on: bool) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
        DWM_WINDOW_CORNER_PREFERENCE,
    };
    const DWMWCP_DONOTROUND: DWM_WINDOW_CORNER_PREFERENCE = DWM_WINDOW_CORNER_PREFERENCE(1);
    const DWMWCP_ROUND: DWM_WINDOW_CORNER_PREFERENCE = DWM_WINDOW_CORNER_PREFERENCE(2);
    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;
    const DWMWA_COLOR_DEFAULT: u32 = 0xFFFF_FFFF;

    if on {
        let pref = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let color: u32 = DWMWA_COLOR_DEFAULT;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &color as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
    } else {
        let pref = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let color: u32 = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &color as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyh.ttf",
        "C:\\Windows\\Fonts\\simsun.ttc",
        "C:\\Windows\\Fonts\\segoeui.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
    ];
    for (i, path) in candidates.iter().enumerate() {
        if let Ok(data) = std::fs::read(path) {
            let name = format!("sys-font-{i}");
            fonts.font_data.insert(
                name.clone(),
                egui::FontData::from_owned(data),
            );
            // Insert FIRST: system fonts (Microsoft YaHei UI / Segoe) lead
            // the stack per the design brief — Latin and CJK both render
            // with the native UI font, egui defaults act as fallback.
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, name.clone());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push(name);
            break;
        }
    }
    ctx.set_fonts(fonts);
}

/// Linear-style dark theme (DESIGN.md §2). Slightly bluer near-black,
/// violet brand accent, near-transparent surfaces, hairline borders.
fn apply_linear_dark_theme(ctx: &egui::Context, pal: &Palette) {
    let mut style = (*ctx.style()).clone();

    // ---- Colors (Linear palette, dark mode) ----
    let _bg_canvas      = egui::Color32::from_rgb(8, 9, 10);        // #08090a
    let bg_panel       = egui::Color32::from_rgb(15, 16, 17);      // #0f1011
    let bg_elevated    = egui::Color32::from_rgb(25, 26, 27);      // #191a1b
    let bg_hover       = egui::Color32::from_rgb(34, 37, 42);      // #23252a
    let text_primary   = egui::Color32::from_rgb(247, 248, 248);   // #f7f8f8
    let text_secondary = egui::Color32::from_rgb(208, 214, 224);   // #d0d6e0
    let _text_tertiary  = egui::Color32::from_rgb(138, 143, 152);   // #8a8f98
    let _text_quat      = egui::Color32::from_rgb(98, 102, 109);    // #62666d
    let brand          = egui::Color32::from_rgb(94, 106, 210);    // #5e6ad2
    let brand_hover    = egui::Color32::from_rgb(130, 143, 255);   // #828fff
    let border_subtle  = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 13);
    let border_std     = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 20);

    let mut visuals = egui::Visuals::dark();
    visuals.dark_mode = true;
    visuals.override_text_color = Some(text_primary);

    visuals.window_fill = bg_panel;
    visuals.window_stroke = egui::Stroke::new(1.0_f32, border_std);

    visuals.panel_fill = bg_panel;
    visuals.faint_bg_color = bg_elevated;
    visuals.extreme_bg_color = bg_hover;

    visuals.window_rounding = egui::Rounding::same(0.0);
    // Phase 6: bump menu rounding 6 → 8 to match the new chip
    // language. The default 6 read as a stock OS menu; 8 sits
    // tighter with the panel chips and the title-bar buttons
    // (which already use 8).
    visuals.menu_rounding = egui::Rounding::same(8.0);

    // Buttons
    visuals.widgets.noninteractive.bg_fill = bg_panel;
    // No 1px light separator between panels — panels blend seamlessly.
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(0.0_f32, egui::Color32::TRANSPARENT);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, text_secondary);

    // Phase 6: bump default button rounding 6 → 8 across
    // inactive / hovered / active so every Linear-styled button
    // matches the new chip language. Subtle but consistent — the
    // previous 6 was visually heavy on the larger 30-px-tall
    // title-bar / bottom-bar buttons.
    visuals.widgets.inactive.bg_fill = pal.button_fill;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, border_subtle);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, text_secondary);
    visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);

    visuals.widgets.hovered.bg_fill = pal.button_hover;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, border_std);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, text_primary);
    visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);

    visuals.widgets.active.bg_fill = brand;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, brand_hover);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, text_primary);
    visuals.widgets.active.rounding = egui::Rounding::same(8.0);

    visuals.widgets.open.bg_fill = bg_elevated;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, border_std);
    visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0_f32, text_primary);

    visuals.selection.bg_fill = brand.linear_multiply(0.25);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, brand);

    visuals.hyperlink_color = brand_hover;
    visuals.warn_fg_color = egui::Color32::from_rgb(255, 170, 0);
    visuals.error_fg_color = egui::Color32::from_rgb(255, 90, 90);
    visuals.code_bg_color = bg_elevated;

    visuals.window_shadow = egui::epaint::Shadow::NONE;
    visuals.popup_shadow = egui::epaint::Shadow::NONE;

    style.visuals = visuals;

    // ---- Spacing & sizing (Linear rhythm) ----
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);
    style.spacing.indent = 18.0;

    // ---- Font sizes (sub-16px uses negative tracking per Linear) ----
    style.text_styles.get_mut(&egui::TextStyle::Body).map(|f| f.size = 13.0);
    style.text_styles.get_mut(&egui::TextStyle::Button).map(|f| f.size = 12.0);
    style.text_styles.get_mut(&egui::TextStyle::Small).map(|f| f.size = 11.0);

    ctx.set_style(style);
}

impl Drop for MainWindow {
    fn drop(&mut self) {
        self.save_window_geometry();
    }
}
