# Aperture Neo Turbo

**High-performance GPU-native image viewer for Windows.**
或许是性能最好的Windows图片查看器之一，没有为了兼容性牺牲渲染和响应速度。

Rust rewrite of [Aperture Neo](https://github.com/DuJunxi1993/Aperture-Neo) focused on maximum decode + render throughput. All GPU paths (decode → upload → composite → animate) stay on the GPU; CPU only handles transform parameters and I/O.

> v1.1.2 — independent project (code and repo), ships its own Inno Setup installer. No runtime dependencies to install (statically linked exe).

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
┌─ Main Window (winit + wgpu + egui, single egui pass) ─────┐
│ ┌──────────┐ ┌─────────────┐ ┌─────────────────────────┐ │
│ │ Tree     │ │ Thumbnails  │ │        Viewer           │ │
│ │ (egui)   │ │ (egui)      │ │  textured egui::Mesh    │ │
│ │          │ │             │ │  in the same pass as    │ │
│ │          │ │             │ │  the chrome             │ │
│ └──────────┘ └─────────────┘ └─────────────────────────┘ │
│ ┌─────────────────────────────────────────┐               │
│ │ FloatingBar / TitleBar / StatusBar      │               │
│ └─────────────────────────────────────────┘               │
└───────────────────────────────────────────────────────────┘
```

**Key design decisions:**
1. **Single egui pass** — the viewer image is drawn as a textured `egui::Mesh` inside the same pass as the chrome; no separate image-quad pass and no Direct2D child HWND
2. **DX12-only backend** (`wgpu::Backends::DX12`) — relies solely on OS-built-in D3D12/DXGI/WIC DLLs, no third-party GPU runtime
3. **WIC primary decoder** — `IWICBitmapSourceTransform::SetTargetDimensions` decodes straight to the target resolution, with no full-size intermediate
4. **Two-tier pre-decode cache** — neighbours ±1 decode at up to 4096 px (full tier), neighbours ±2 at 640 px (low tier); a low hit shows immediately and upgrades asynchronously
5. **Rotation-aware transforms** — per-quadrant affine matrices; fit, zoom and pan all account for quarter-turn rotations

## Workspace Layout

```
crates/
├── core/    # Platform-agnostic: decode trait, nav, SQLite cache, fs
├── gpu/     # WIC decode, viewer state machine, animator, decode coordinator
└── app/     # Main binary: winit window, egui chrome, Win32 integrations
```

## Build

```bash
# Debug
cargo build

# Release (lto, stripped)
cargo build --release
```

Output: `target/release/Aperture-Neo-Turbo.exe`

## Run

```bash
# Open default folder (last opened)
cargo run --release

# Open specific folder
cargo run --release -- "C:\Users\you\Pictures"
```

## Requirements

- Windows 10 1809+ or later (WIC DXVA, DXGI 1.2+)
- GPU with DirectX 12 support (feature level 11_0+)
- ~30MB disk space

## Phase Plan

| Phase | Deliverable | Status |
|-------|-------------|--------|
| P1 | winit + DXGI swapchain + D2D init | ✅ Done |
| P2 | WIC DXVA primary decoder | ✅ Live (skia-safe fallback removed) |
| P3 | Viewer transform + rendering | ✅ Live (egui-native textured mesh) |
| P4 | Slide/zoom GPU animations | ✅ Live (`Animator`, ease curves) |
| P5 | winit + egui chrome integration | ✅ Live |
| P6 | SQLite thumbnail cache | ✅ Live |
| P7 | File system watch + navigation | ✅ Done |
| P8 | HiDPI, multi-monitor, fullscreen, rotation, slide show | ✅ Done |
| P9 | Inno Setup installer + default image viewer | ✅ v1.0.0 |

## Performance Constraints

- CPU < 3% during smooth pan/zoom at 4K
- No GC pauses (Rust ownership model)
- All composition on GPU; CPU only for transform math
- Target: <1ms CPU per frame at 4K 60fps

## License

MIT
