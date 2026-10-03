# Aperture Neo Turbo

## Build

```bash
cargo build --release
cargo run --release -- "C:\path\to\image.jpg"   # open a file/folder at startup
```

- Workspace root: `D:\Development\aperture-neo-turbo` (run `cargo` from here)
- Output: `target/release/aperture-neo-turbo.exe` (~10 MB, statically linked)
- Windows-only (`net10.0-windows` equivalent: winit 0.30 + wgpu/DX12 + Direct2D + WIC)
- The exe embeds an icon + version resource via `crates/app/build.rs` (winresource) reading `assets/apertureneo_turbo.ico`; version comes from `crates/app/Cargo.toml` `[package] version`.
- Release profile: `opt-level=3`, `lto="fat"`, `codegen-units=1`, `panic="abort"`, `strip="symbols"`. Console (release) is suppressed via `windows_subsystem = "windows"` in `main.rs`.

## Dependencies

Runtime dependencies: **none to install.** The exe is statically linked (Rust stdlib, bundled SQLite). GPU backend is **DX12** (`crates/app/src/window.rs` Backends::DX12) — uses only OS-built-in D3D12/DXGI/WIC DLLs. Contrast: the C# `ApertureNeo` is framework-dependent and needs .NET 10 Desktop Runtime + WebView2; Turbo does not.

## Architecture

| Folder | Purpose |
|--------|---------|
| `crates/core/` | Platform-agnostic: decode trait, navigation, thumbnail cache (SQLite, bundled rusqlite), fs traversal, persistent settings, domain models |
| `crates/gpu/` | WIC decode, viewer state machine (fit/zoom/pan/rotation/slide), animator (ease curves + slide), decode coordinator (two-tier PredecodeCache + tiered upgrade), `egui::TextureHandle` wrapper |
| `crates/app/` | winit main loop (`ApplicationHandler`), egui chrome (title bar, tree, thumbnails, status bar, fullscreen bar, edge drawer, settings dropdown, shortcuts popover), event routing, native Win32 integrations (wallpaper, Explorer reveal, clipboard, registry), file-tree widget, on-screen icons |

Entry: `crates/app/src/main.rs` — `tokio::runtime::Builder::new_multi_thread()` for the decode coordinator's blocking spawn; argv → `LaunchTarget::None / Folder / SingleImage { path, width, height }`; `winit::event_loop::EventLoop::new()` with `ControlFlow::Poll`; `MainWindow::new(target)`; `event_loop.run_app(&mut app)`. `MainWindow` implements `ApplicationHandler` (`resumed` → `init_window`, `window_event` → shortcut/drop/cursor/router fan-out, `about_to_wait` → redraw request).

Main window = winit + wgpu + egui, all on a **single egui pass**. The viewer image is rendered through egui's own mesh/vertex pipeline — `Direct2DViewer::paint_viewer` builds a textured `egui::Mesh` from the viewer's image→screen affine and the surface is cleared + fully drawn by the same egui pass (no separate image-quad wgpu pass). The renderer's GPU backend is **DX12 only** (`wgpu::Backends::DX12`); there is no longer a Direct2D child HWND (removed when the egui-popup stack was restored — see "Context menus").

## Key Behaviors

- **Decode**: WIC primary via `IWICBitmapSourceTransform::SetTargetDimensions` (exact-target resolution). Decoded frames are uploaded as `egui::TextureHandle`s wrapped in `DecodedGpuImage { handle, width, height, average_luminance }`. `crates/gpu/Cargo.toml` still declares the `skia-safe` workspace dep for backwards-compatible feature flags, but the live decode path is WIC only.
- **Render (egui-native, single pass)**: `Direct2DViewer::paint_viewer` draws the current image, plus the outgoing one during a slide, as `egui::Mesh`es inside `CentralPanel`, clipped to the viewer rect. The surface is cleared and fully drawn by the single egui pass (`srgb_to_linear` only applied when the DX12 swapchain reports a linear format, see `WgpuState::surface_is_srgb`).
- **Slide animation**: parallel slide via the `Animator` (`EaseInOut` discrete fit↔1:1, `EaseOut` repeated zoom steps). Outgoing image exits to the trailing edge **anchored at its own fit** (its own offset/zoom/rotation captured before `compute_fit` overwrites them); incoming enters from the opposite edge anchored at its own fit; no cross-scaling. Non-directional loads swap instantly. A directional slide plays only when the target is a full-size cache hit and the nav queue is drained; held-arrow/intermediate steps cut directly (`SlideDir::None`).
- **Two-tier pre-decode cache** (`crates/gpu/src/coordinator.rs`): `PredecodeCache` holds `Tier::Full` (neighbours ±1, decoded up to `min(FULL_RES_DIM=4096, device max_texture_dimension_2d)`) and `Tier::Low` (neighbours ±2, decoded at `LOW_RES_DIM=640`). A `Full` hit displays instantly (and slides); a `Low` hit displays immediately (direct cut) then upgrades to `Tier::Full` asynchronously via `request_full_upgrade` (progressive sharpen). The low-first path guarantees a cold step never shows a blank frame.
- **Texture lifetime (non-blocking frame fence)**: replaced images are held in a `retired` `VecDeque<(Arc<DecodedGpuImage>, render_epoch)>` in `Direct2DViewer`. Each frame `submit_wgpu_frame` advances `render_epoch`, calls `viewer.set_render_epoch`, and `tick_release` releases entries whose tagged epoch ≤ `safe_release_epoch = render_epoch - RETIRE_FRAME_BUDGET (4)`. `device.poll(Maintain::Poll)` (non-blocking) runs every frame to reap deferred destroys. With `PresentMode::Fifo` + `desired_maximum_frame_latency: 1`, the GPU is ≤1 frame behind, so a 4-frame budget is provably past any in-flight submit and the render thread is never blocked (wgpu 22 exposes no non-blocking "which submission completed" query, so a frame fence is the correct tool). Three safety details: (1) `set_image_gpu`'s directional branch retires the outgoing `previous_gpu` BEFORE overwriting it (never drops it directly); (2) thumbnails decode under a `Semaphore` (~4 concurrent) so a folder of high-res images doesn't flood the machine; (3) the full-tier clamp respects the device's `max_texture_dimension_2d` and the app sets `raw_input.max_texture_side = device.limits().max_texture_dimension_2d` so egui's `load_texture` never asserts.
- **Context menus (egui-drawn, Linear theme)**: image and tree right-click menus are **egui popups**, not Win32. `enum CtxMenu { Image { pos }, Tree { pos, path, root_idx, is_favorite } }` is set on right-click and consumed each frame by `MainWindow::draw_context_menu` (free fn, takes `egui::Context` + `Palette` + `&mut Vec<UiAction>` so the egui closure doesn't need `&mut self`). Replaced wholesale on each new right-click → old menu closes automatically. The right-button-down that opened the menu is ignored via `ctx_menu_open_at` grace window. Clicking an item pushes a `UiAction`; clicking outside closes (any-frame `pointer.any_click()` against `ctx_menu_rect`). `MAIN_HWND` still exists for the **non-menu** Win32 calls below (wallpaper, Explorer reveal, clipboard, client-rect, registry); it's stored at window creation via `RawWindowHandle::Win32`. `panic = "abort"` is in effect, so `CreatePopupMenu`-style failures are warned and skipped — this concern now applies to `ShellExecuteW`/clipboard paths, not menu paths.
- **Rotation / slide show**: per-quadrant affine matrices in `Direct2DViewer::display_transform`; `compute_fit` / `zoom_step` / `zoom_continuous` / `clamp_pan` are rotation-aware (`effective_size` swaps w/h for odd quarter-turns). `tick_rotation` interpolates `rotation_deg` from a `RotAnim` so the image spins smoothly instead of snapping to 0/90/180/270. Slide show = 3 s timer (`slide_show_running` + `slide_show_last` in `MainWindow`).
- **Fullscreen**: hides title bar, floating fullscreen bar, and both side columns (tree + thumbnails). The floating bar auto-hides after idle; `Esc` exits. The fullscreen rect uses the **Win32 `GetClientRect`** (not winit's logical size) so HiDPI/125% scaling doesn't mis-size the surface. Animation runs in window coords via the `viewport_origin` field on `Direct2DViewer`; the old `set_viewport_target` / `window_target_for_viewport` helpers are no longer used.
- **Edge drawer**: left-edge auto-expand tree menu with a translucent handle pill (light/dark chosen by `image_luminance` + 200 ms debounce + hysteresis), auto-width via `drawer_content_min` (longest row's content min, separate from the user-draggable `drawer_user_width`), and a SINGLE animated width (`drawer_width_anim`, ~150 ms ease) used by drawing, hit-test (`drawer_hit_width()`), wheel-over-drawer routing, and click-close — so the visible width, click z-order, and scroll target never diverge.
- **Image load order**: thumbnails load closest-to-current first (distance-based sort in `DecodeCoordinator::prefetch_*`); in-flight loads are dropped on folder change.

## Data Location

- Thumbnail cache: `%LOCALAPPDATA%` (SQLite, bundled). Recents/favorites persisted via settings.

## No Test Suite

No test project present. Do not attempt to run tests.

## Packaging

Inno Setup installer lives in `Installer/installer.iss` — per-user install (`{localappdata}\Programs\ApertureNeoTurbo`), `PrivilegesRequired=lowest`, bilingual (English + 简体中文), registers the exe as the default image viewer via HKCU file associations. No runtime dependencies to detect/install (unlike the C# version). Script is shared/interop with the C# `ApertureNeo` project for the file-association logic.
