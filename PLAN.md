# Aperture Neo Turbo — Implementation Plan

## Phase 1: Skeleton ✅ DONE

Workspace + 4 crates compile clean.

## Phase 2: Real Direct2D + WIC Decode ✅ DONE

GPU device stack + WIC decode + D2D bitmap upload all compile and produce a valid GPU context. RTX 4070 detected.

## Phase 3: winit + Direct2D child HWND ✅ DONE (with one important fix)

The window now opens and stays running. Key fixes during this phase:

- `IDXGIFactory` comes from `IDXGIDevice → IDXGIAdapter → IDXGIFactory2` (NOT `IDXGIDevice::GetParent()` which only returns base IDXGIFactory)
- Use legacy `IDXGIFactory::CreateSwapChain` (instead of `IDXGIFactory2::CreateSwapChainForHwnd`) which is more forgiving on child HWNDs
- `CreateBitmapFromDxgiSurface` requires `DXGI_FORMAT_UNKNOWN` (not `TYPELESS`) + `D2D1_BITMAP_OPTIONS_CANNOT_DRAW` for render-target DXGI surfaces

Implemented:
- winit::EventLoop with Poll control flow
- WGPU surface (DX12 backend) for main window chrome
- Child HWND with WNDCLASSEXW + WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS
- Direct2D swapchain for child HWND (independent GPU context from wgpu's)
- Keyboard handler for Arrow/PgUp/Dn/Home/End/F keys
- Mouse wheel forwarding to `Direct2DViewer::on_wheel`
- Double-click detection for fit/actual toggle
- Settings persistence (last window size) to `%APPDATA%\ApertureNeoTurbo\settings.json`

## Phase 4: egui Rendering + Thumbnail Pipeline ⏳ NEXT

The current `render_frame` does only a wgpu clear — egui paint_jobs path was simplified due to egui_wgpu 0.29 API friction. To complete:

1. Re-introduce egui-wgpu rendering on the wgpu surface (cleanly handle the borrow conflict between `update_buffers` and `begin_render_pass` — egui_wgpu's own `winit.rs` uses a single encoder)
2. Wire `FloatingBar`, `TitleBar`, `TreePanel`, `ThumbPanel` into egui CentralPanel
3. Implement thumbnail decode pipeline (call `WicLoader::load` from `tokio::spawn_blocking`, cache to `ThumbCache`)
4. Wire `NavigationService::` → `coordinator.request_current()` on arrow keys

## Phase 5: Polish ⏳

- HiDPI: handle `WM_DPICHANGED` in child HWND
- Multi-monitor: `EnumDisplayMonitors` for fullscreen
- Fullscreen: `SetWindowLongPtr(GWL_STYLE, 0)` + maximize
- Folder load on startup (if last_folder is set in settings)

## Phase 6: Installer

Inno Setup script in `installer/installer.iss`:
- Per-user install (no admin required)
- Detect existing ApertureNeoTurbo installation
- Offer upgrade with data preservation

---

## Current binary status

```
$ target\release\aperture-neo-turbo.exe = 9.7 MB
$ Window opens (1400×900), stays running
$ Direct2D child HWND created (1080×872 viewer area)
$ Direct2D swapchain created on child HWND
$ winit events flow through
```

## Remaining work to MVP

1. egui rendering on wgpu surface (currently just clears)
2. Hook keyboard nav + wheel to viewer
3. Connect NavigationService → DecodeCoordinator
4. Implement thumbnail pipeline
5. HiDPI + fullscreen polish

Estimated: ~2-3 days of focused work.