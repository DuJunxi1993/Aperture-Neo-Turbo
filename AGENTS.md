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

Runtime dependencies: **none to install.** The exe is statically linked (Rust stdlib, Skia, bundled SQLite). GPU backend is **DX12** (`crates/app/src/window.rs` Backends::DX12) — uses only OS-built-in D3D12/DXGI/D2D/WIC DLLs. Contrast: the C# `ApertureNeo` is framework-dependent and needs .NET 10 Desktop Runtime + WebView2; Turbo does not.

## Architecture

| Folder | Purpose |
|--------|---------|
| `crates/core/` | Platform-agnostic: decode trait, navigation, thumbnail cache, fs, file tree |
| `crates/gpu/` | D2D device stack, WIC decode, Direct2D viewer (transform/anim), texture/cache |
| `crates/ui/` | egui chrome: title bar, floating bar, tree, thumbnails, settings, shortcuts |
| `crates/app/` | winit main loop, child-HWND orchestration, event routing, native context menus |

Entry: `crates/app/src/main.rs` — `EventLoop::<AppMessage>::with_user_event().build()`; `MainWindow::new(target, event_loop.create_proxy())`; `run_app(&mut app)`.

Main window = winit + wgpu + egui. Viewer = independent child HWND with its own DXGI swapchain + Direct2D.

## Key Behaviors

- **Decode**: WIC primary via `IWICBitmapSourceTransform::SetTargetDimensions` (exact-target resolution); skia-safe fallback for unsupported formats.
- **Render**: D2D1 bitmap uploaded from WIC; `D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC` on GPU.
- **Context menus (native)**: image + tree right-click menus use Win32 `TrackPopupMenu`, NOT egui popups. A right-click inside the viewer/tree posts an `AppMessage::ShowImageMenu / ShowTreeMenu` via `event_loop_proxy`; the popup is shown in `user_event` (a modal menu inside the winit `MouseInput` dispatch would be suppressed by winit's pump re-entrancy). `MAIN_HWND` is stored at window creation (`window_handle()` → `RawWindowHandle::Win32` → atomic store); it must be non-zero for the menus to anchor. `CreatePopupMenu` failure is warned and skipped (never `panic!`, since `panic="abort"` would crash).
- **Rotation / slide show**: per-quadrant affine matrices in `Direct2DViewer::display_transform`; `compute_fit`/`zoom_1_to_1`/`clamp_pan` are rotation-aware (`effective_size` swaps w/h for odd quarter-turns). Slide show = 3s timer.
- **Slide animation**: 420ms parallel slide, old bitmap exits to trailing edge, new enters from opposite edge, ease-in-out cubic, no cross-scaling; non-directional loads swap instantly. Direction from `TransitionDirection` (index delta of last load).
- **Fullscreen**: hides title bar, floating bar, and both side columns (tree + thumbnails). Animation runs in window coords via `viewport_origin` / `set_viewport_target` / `window_target_for_viewport`, with the final target from `compute_final_viewer_rect` (user widths). Toolbar auto-hides after idle; `Esc` exits.
- **Floating bar**: bottom-center control bar; collapses to a 40×5 pill handle when pointer leaves (grace delay + idle fallback), expands on hover.
- **Shortcuts popover (`?`)**: stays an egui `Window` with its own single-rect hole (`apply_child_holes` / `help_child_hidden`).
- **Image load order**: thumbnails load closest-to-current first (distance-based sort); `CancellationTokenSource` cancels in-flight loads on folder change.

## Data Location

- Thumbnail cache: `%LOCALAPPDATA%` (SQLite, bundled). Recents/favorites persisted via settings.

## No Test Suite

No test project present. Do not attempt to run tests.

## Packaging

Inno Setup installer lives in `Installer/installer.iss` — per-user install (`{localappdata}\Programs\ApertureNeoTurbo`), `PrivilegesRequired=lowest`, bilingual (English + 简体中文), registers the exe as the default image viewer via HKCU file associations. No runtime dependencies to detect/install (unlike the C# version). Script is shared/interop with the C# `ApertureNeo` project for the file-association logic.
