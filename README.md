# Aperture Neo Turbo

**High-performance GPU-native image viewer for Windows.**

Rust rewrite of [Aperture Neo](https://github.com/your-org/ApertureNeo) focused on maximum decode + render throughput. All GPU paths (decode → upload → composite → animate) stay on the GPU; CPU only handles transform parameters and I/O.

## Goals

| Metric | Aperture Neo (WPF) | **Turbo (Rust)** |
|--------|-------------------|------------------|
| 8K JPEG first paint | ~180ms | **<20ms** |
| 60fps wheel zoom | ~180ms CPU/frame | **<2ms CPU/frame** |
| 420ms slide transition | ~150ms single-core | **<1ms CPU total** |
| Cold start | ~800ms | **<300ms** |
| Idle memory | ~85MB | **~30MB** |
| Install size | ~22MB (with OCR) | **~12MB** |

## Architecture

```
┌─ Main Window (winit + wgpu + egui) ─────────────────┐
│ ┌──────────┐ ┌─────────────┐ ┌─────────────────────┐ │
│ │ Tree     │ │ Thumbnails  │ │ Viewer Child HWND  │ │
│ │ (egui)   │ │ (egui)      │ │ (Direct2D)         │ │
│ │          │ │             │ │   ↓               │ │
│ └──────────┘ └─────────────┘ │ IDXGISwapChain1   │ │
│ ┌─────────────────────────────────────────┐ │   ↓          │ │
│ │ FloatingBar / TitleBar (egui)            │ │ Direct2D    │ │
│ └─────────────────────────────────────────┘ │ (GPU only) │ │
└──────────────────────────────────────────────────────────────┘
```

**Key design decisions:**
1. **Independent swapchain** for the viewer — no GPU context overlap with egui/wgpu, zero synchronization cost
3. **WIC DXVA primary** decoder — hardware-accelerated JPEG/HEIC/AVIF decode
4. **D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC** — GPU texture sampling, no CPU scaling
5. **GPU-driven animations** — `SetTransform` + `DrawBitmap` per frame, all on GPU

## Workspace Layout

```
crates/
├── core/    # Platform-agnostic: decode trait, nav, cache, fs
├── gpu/     # D3D11 + Direct2D rendering engine
├── ui/      # egui-based chrome (title bar, panels, settings)
└── app/     # Main binary: winit window + child HWND orchestration
```

## Build

```bash
# Debug
cargo build

# Release (lto, stripped)
cargo build --release
```

Output: `target/release/aperture-neo-turbo.exe`

## Run

```bash
# Open default folder (last opened)
cargo run --release

# Open specific folder
cargo run --release -- "C:\Users\you\Pictures"
```

## Requirements

- Windows 10 1809+ or later (WIC DXVA, DXGI 1.2+)
- GPU with DirectX 11.1+ support
- ~30MB disk space

## Phase Plan

| Phase | Deliverable | Status |
|-------|-------------|--------|
| P1 | winit + DXGI swapchain + D2D init | ✅ Done |
| P2 | WIC DXVA primary + skia-safe fallback | ✅ Compiles (GPU code live) |
| P3 | Direct2D viewer + transform | ✅ Compiles |
| P4 | Slide/zoom GPU animations | ✅ Compiles (animator done) |
| P5 | winit + egui chrome integration | Skeleton ✅ |
| P6 | SQLite thumbnail cache | Skeleton ✅ |
| P7 | File system watch + navigation | Skeleton ✅ |
| P8 | HiDPI, multi-monitor, fullscreen | TBD |
| P9 | Inno Setup installer | TBD |

## Performance Constraints

- CPU < 3% during smooth pan/zoom at 4K
- No GC pauses (Rust ownership model)
- All composition on GPU; CPU only for transform math
- Target: <1ms CPU per frame at 4K 60fps

## License

TBD