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
| `crates/core/` | Platform-agnostic: decode trait, navigation, thumbnail cache, fs, file tree |
| `crates/gpu/` | WIC decode, Direct2DViewer state machine (fit/zoom/pan/rotation/slide), Animator, decode coordinator, egui-texture wrapper |
| `crates/app/` | winit main loop, egui chrome (title bar, tree, thumbnails, status bar, edge drawer), event routing, native context menus |

Entry: `crates/app/src/main.rs` — `EventLoop::<AppMessage>::with_user_event().build()`; `MainWindow::new(target, event_loop.create_proxy())`; `run_app(&mut app)`.

Main window = winit + wgpu + egui. The viewer image is rendered **through egui's own mesh/vertex pipeline** (a textured quad built from the viewer's image→screen affine), not a custom wgpu pass.

## Key Behaviors

- **Decode**: WIC primary via `IWICBitmapSourceTransform::SetTargetDimensions` (exact-target resolution); skia-safe fallback for unsupported formats.
- **Render (egui-native)**: decoded frames are uploaded as `egui::TextureHandle` (`DecodedGpuImage` wraps a handle, width/height, average_luminance); `Direct2DViewer::paint_viewer` draws the current (and, during a slide, outgoing) image as an `egui::Mesh` inside the `CentralPanel`, clipped to the viewer rect. No separate image-quad wgpu pass — the surface is cleared and fully drawn by the single egui pass.
- **Slide animation**: 0.35s ease-in-out-cubic parallel slide via the `Animator`; outgoing image exits to the trailing edge anchored at its own fit, incoming enters from the opposite edge anchored at its own fit, no cross-scaling. Non-directional loads swap instantly. Slide plays only when the target is a **full-size** cache hit and the nav queue is drained; held-arrow/intermediate steps cut directly (`SlideDir::None`).
- **Two-tier pre-decode cache**: `PredecodeCache` holds `Tier::Full` (neighbours ±1, decoded up to `min(FULL_RES_DIM, device max_texture_dimension_2d)`) and `Tier::Low` (neighbours ±2, decoded at `LOW_RES_DIM`=640px). A `Full` hit displays instantly (and slides); a `Low` hit displays immediately (direct cut) then upgrades to `Tier::Full` asynchronously (progressive sharpen via `request_full_upgrade`). The low-first path guarantees a cold step never shows a blank frame.
- **Texture lifetime (non-blocking frame fence)**: replaced images are held in a `retired` queue tagged with the render epoch they were last drawn in. `submit_wgpu_frame` advances a `safe_release_epoch` watermark (`render_epoch - RETIRE_FRAME_BUDGET`) and releases a texture only once it's that many frames old. `device.poll(Maintain::Poll)` (non-blocking) runs every frame to reap deferred destroys. With Fifo present + `desired_maximum_frame_latency: 1`, the GPU is guaranteed ≤1 frame behind, so a small frame budget is provably past any in-flight submit — and the render thread is **never blocked** (wgpu 22 exposes no non-blocking "which submission completed" query, so a frame fence is the correct tool). Three safety details: (1) the directional branch of `set_image_gpu` retires the outgoing `previous_gpu` BEFORE overwriting it (never drops it directly); (2) thumbnails decode under a `Semaphore` (~4 concurrent) so a folder of high-res images doesn't flood the machine; (3) the full-tier clamp respects the device's `max_texture_dimension_2d` and the app sets `raw_input.max_texture_side` so egui's `load_texture` never asserts.
- **Context menus (native)**: image + tree right-click menus use Win32 `TrackPopupMenu`, NOT egui popups. A right-click inside the viewer/tree posts an `AppMessage::ShowImageMenu / ShowTreeMenu` via `event_loop_proxy`; the popup is shown in `user_event`. `MAIN_HWND` is stored at window creation (`window_handle()` → `RawWindowHandle::Win32` → atomic store); it must be non-zero for the menus to anchor. `CreatePopupMenu` failure is warned and skipped (never `panic!`, since `panic="abort"` would crash).
- **Rotation / slide show**: per-quadrant affine matrices in `Direct2DViewer::display_transform`; `compute_fit`/`zoom_1_to_1`/`clamp_pan` are rotation-aware (`effective_size` swaps w/h for odd quarter-turns). Slide show = 3s timer.
- **Fullscreen**: hides title bar, floating bar, and both side columns (tree + thumbnails). Animation runs in window coords via `viewport_origin` / `set_viewport_target` / `window_target_for_viewport`. Toolbar auto-hides after idle; `Esc` exits.
- **Edge drawer**: left-edge auto-expand tree menu with a translucent handle pill (light/dark via image luminance + hysteresis), auto-width via `drawer_content_min`, and unified hit-test width `drawer_hit_width()` so the drawer, wheel-over-drawer, and click-close all agree.
- **Image load order**: thumbnails load closest-to-current first (distance-based sort); in-flight loads are dropped on folder change.

## Data Location

- Thumbnail cache: `%LOCALAPPDATA%` (SQLite, bundled). Recents/favorites persisted via settings.

## No Test Suite

No test project present. Do not attempt to run tests.

## Packaging

Inno Setup installer lives in `Installer/installer.iss` — per-user install (`{localappdata}\Programs\ApertureNeoTurbo`), `PrivilegesRequired=lowest`, bilingual (English + 简体中文), registers the exe as the default image viewer via HKCU file associations. No runtime dependencies to detect/install (unlike the C# version). Script is shared/interop with the C# `ApertureNeo` project for the file-association logic.
