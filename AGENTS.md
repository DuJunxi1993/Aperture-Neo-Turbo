# Aperture Neo Turbo

## Build Status

✅ **P1 Skeleton Complete** — Workspace compiles cleanly. Release binary `target/release/aperture-neo-turbo.exe` (712 KB) runs and prints version + architecture summary.

GPU implementation is **stubbed** — type structure is correct, but actual `D3D11CreateDevice` / `D2D1CreateBitmapFromWicBitmap` / `SetTransform` calls are placeholders. See `PLAN.md` for P2 details.

## Build

```bash
cargo build --release
cargo run --release -- "C:\path\to\folder"  # opens folder (P3+)
```

- Workspace at root; run from `D:\Development\aperture-neo-turbo`
- Output: `target/release/aperture-neo-turbo.exe`
- Windows-only (DXGI, WIC, Direct2D)

## Architecture

| Folder | Purpose |
|--------|---------|
| `crates/core/` | Platform-agnostic: `IImageLoader` trait, navigation, thumbnail cache, fs |
| `crates/gpu/` | D3D11 device + Direct2D renderer + WIC bitmap decode + viewer state |
| `crates/ui/` | egui chrome: title bar, floating bar, tree, thumbnails, settings, shortcuts |
| `crates/app/` | winit main loop, child HWND orchestration, event routing |

Entry: `crates/app/src/main.rs`. Main window = winit + wgpu + egui (P5+). Viewer = independent child HWND with its own DXGI swapchain (P2+).

## Key Behaviors (planned)

- **Decode**: WIC primary (DXVA when available) via `IWICBitmapSourceTransform::SetTargetDimensions` for exact-target resolution; skia-safe fallback for unsupported formats.
- **Render**: D2D1 bitmap uploaded from WIC; `D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC` for GPU-side scaling.
- **Animation**: GPU transform lerped from CPU. 420ms slide = 25 frames × 1 SetTransform + 1 DrawBitmap ≈ <2ms total CPU.
- **Viewer child window**: independent DXGI swapchain — no sync with egui's wgpu device.

## No Test Suite

No tests in skeleton phase. Add `#[test]` modules per crate once features stabilize.

## Dependencies

- **windows 0.58** — Win32 bindings (D2D, D3D11, DXGI, WIC, WinUI shell)
- **winit 0.30** — window/event loop (P2+)
- **egui 0.29 + egui-wgpu** — chrome UI (P5+)
- **wgpu 22** — main window surface (P5+)
- **skia-safe 0.78** — CPU fallback decoder (P2+)
- **rusqlite 0.32 (bundled)** — thumbnail cache DB
- **notify 6** — file system watch
- **tokio 1 (sync, macros, rt-multi-thread)** — async runtime
- **parking_lot** — fast mutex/rwlock

`Cargo.toml` workspace root defines all versions.