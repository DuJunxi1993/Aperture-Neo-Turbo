# Aperture Neo Turbo — Bugfix History (v1.0.0 → v1.0.7)

> Catalog of every non-trivial bug discovered during the v1.x stabilisation
> arc, the root cause, the fix, and the commit that landed it. Bugs are
> grouped by the subsystem they broke, not by discovery order — the
> same surface-level symptom sometimes had multiple roots.

**Stack**: Rust + wgpu 22 + egui 0.29 + Direct2D (via D3D11) + winit 0.30.
Hybrid rendering — egui chrome on a wgpu parent surface, image viewer as a
child HWND with its own DXGI swapchain. Windows-only desktop app.

---

## Why Rust + egui + DXGI is hard here

A short list of platform facts that bit us during this arc. Knowing
them upfront should save you days of debugging if you work on a
similar stack.

### Two render surfaces under one HWND

egui draws chrome in a wgpu DXGI surface attached to the main HWND.
The image viewer is a *separate* child HWND with its *own* DXGI
swapchain fed by Direct2D. They live in the same window but render
through completely separate paths. DWM composites both surfaces per
frame, but only at vblank — anything we Present between vblanks waits
for the next one. Asynchrony between two Present calls in the same
frame introduces subtle ordering artefacts that show up as flicker.

### DPI scaling vs winit logical sizes

winit's `LogicalSize<>` and `PhysicalSize<>` are easy to confuse.
If you save a PhysicalSize to JSON and feed it back as a LogicalSize
later, the OS multiplies it by the current scale factor — at 125%
DPI you get a 25% larger window on each launch, and at 100% it
silently works. The bug hides on developer machines.

### DXGI swapchain present semantics

DXGI's swapchain Present is "queue this frame, wait for vblank". The
first Present after `CreateSwapChainForHwnd` can legitimately fail
with `DXGI_ERROR_WAS_STILL_DRAWING` on some drivers — the swapchain
is still warming up, vblank is one frame away. The back buffer is
*not* initialised by `CreateSwapChainForHwnd`; the first Present is
what initialises it. If you skip Present, the buffer keeps whatever
memory the allocator gave you.

### `WS_EX_TRANSPARENT` on a DXGI child

`WS_EX_TRANSPARENT` means "the window itself is not painted" (MSDN).
For a normal GDI child this is fine. For a DXGI swapchain child, DWM
may exclude the surface from its composition tree and fall back to
the system window brush (`COLOR_WINDOW` ≈ `#F0EFEC`). Combined with
`HTTRANSPARENT` for hit-testing, the extended style is pure overhead
and will actively hide the DXGI surface.

### WM_PAINT vs DXGI Present

Windows GUI apps are built around WM_PAINT — paint when asked, redraw
on invalidate. DXGI swapchain apps paint proactively — call Present
and DWM samples. Mixing them in one window (winit/egui's
WM_PAINT-driven chrome + a DXGI child driven by `RedrawRequested`)
means `DefWindowProcW` on the child tries to `FillRect` with the
WNDCLASS background brush every time it gets a paint message. The
brush defaults to NULL → system default → `COLOR_WINDOW`. Register
the class with `HBRUSH(HOLLOW_BRUSH)` (=5) and handle
`WM_ERASEBKGND` by returning 1 to suppress the GDI paint.

### egui pixel grid vs native pixel grid

egui's `pixels_per_point` is the DPI scale factor at the winit
level. `SidePanel::exact_width()` takes logical pixels. The D2D child
takes physical pixels. Anywhere the two grids meet — viewer rect,
panel widths, snap rounding — a mistake compounds into a 1-pixel
hairline that reads as a dark/light seam or, worse, a slowly-drifting
display rect.

### Per-field semantics

Sharing one field between two callers (e.g. `resize()` writes to a
field that `apply_position()` then reads) creates a "writes always
match" footgun. `SetWindowPos` is the single point that actually
changes the on-screen state, so guard-comparisons must use the
*last-applied* field, not the *last-requested* one.

---

## 1. Rendering correctness (chrome / viewer / DWM composition)

### 1.1 Black/white flash on launch (the systemic one)

**Symptom**: Window briefly shows a black/white split rectangle on
launch before settling into the proper image. User reported the
pattern as consistent across light and dark themes — the colours
matched a system default (light gray ≈ `#F0EFEC` for white; theme
panel_bg for black).

**Root cause** (four layers, all needed fixing):

1. **DWM composition race** — wgpu parent HWND is created and shown
   in `init_window`. DWM may do a composition before winit's event
   pump fires `WindowEvent::Resized` → `init_renderer` →
   `present_wgpu_surface_for_init`. The first composition samples an
   uninitialised wgpu surface.
2. **WM_PAINT FillRect on viewer child** — even after both surfaces
   exist, `ShowWindow(SW_SHOW)` triggers WM_PAINT on the child →
   `DefWindowProcW` → `FillRect(hbrBackground=COLOR_WINDOW)` over
   the DXGI swapchain. The child appears as a `#F0EFEC` light
   rectangle until the next per-frame render overpaints.
3. **wgpu surface never Present-ed before first DWM sample** — even
   after `init_renderer` runs, no Present has happened on the parent
   surface. The user sees the OS default for one frame.
4. **Linear surface + srgb_to_linear over-darken** — on DX12 drivers
   that report only linear formats, applying
   `srgb_to_linear(pal.canvas_clear)` renders near-black instead of
   the authored panel_bg. Explains the "black" half of the flash.

**Fix**:
- Layer 1: `WindowAttributes::default().with_visible(false)` +
  `set_visible(true)` after `present_wgpu_surface_for_init()`.
- Layer 2: `hbrBackground = HBRUSH(HOLLOW_BRUSH.0)` +
  `WM_ERASEBKGND → return 1`; remove `WS_EX_TRANSPARENT`.
- Layer 3: `present_wgpu_surface_for_init()` clears and Presents once.
- Layer 4: `WgpuState.surface_is_srgb: bool`; LoadOp `Clear` only
  applies `srgb_to_linear` when true.
- Bonus: `swapchain::present` retries `DXGI_ERROR_WAS_STILL_DRAWING`
  with 2→32ms backoff up to 6 attempts.

**Commits**: `a4f4b1f` (layer 2), `9459965` (layers 3+4), `e40b718`
(layers 1 + linear-clear + WAS_STILL_DRAWING retry).

### 1.2 Image distorts on resize

**Symptom**: User drags the main window border. The displayed image
gets visually distorted (squashed / stretched / wrong aspect ratio).

**Root cause**: `apply_position` had a guard that compared against
`hit_rect`, but `resize()` *also* wrote to `hit_rect` on every call.
The guard therefore always matched → `SetWindowPos` was never called
outside the `BIG_JUMP_PIXELS > 100` branch. The HWND stayed at the
old size while the parent wgpu surface reconfigured and egui
relayouted. `DXGI_SCALING_STRETCH` non-uniformly scaled the new
buffer into the old HWND extent.

**Fix**: Rename `hit_rect` → `last_applied_rect`. Drop the write in
`resize()`. Write `last_applied_rect` only after a real
`SetWindowPos` in `apply_position()` and the big-jump branch in
`resize()` for parity. The guard now compares against what was
actually applied last.

**Commit**: `e40b718`.

### 1.3 Thumbs panel hidden behind viewer after close+reopen

**Symptom**: User closes thumbs panel → reopen → the thumbs appear
squeezed into the bottom-right corner; the viewer stays occupying
the area that used to be the thumbs' column.

**Root cause**: Same root cause as 1.2. With the broken
`apply_position` guard, when thumbs reopen, `thumb_panel.anim`
ticks from 0 to `THUMB_WIDTH` over ~120 ms. Each frame `child.resize`
is called with a shrinking `vw`. The big-jump branch never fires
(per-frame delta is small). `apply_position`'s guard always
short-circuits. The HWND stays at its closed-state size (full
viewport width), covering the egui thumb column.

**Fix**: Same as 1.2.

**Commit**: `e40b718`.

### 1.4 Viewer aspect-ratio mismatch on single-image launch

**Symptom**: Opening a single image (Explorer double-click) showed
black bars (letterbox) on left/right or top/bottom.

**Root cause**: `init_h` only added `TOOLBAR_HEIGHT` to image height;
`STATUS_BAR_HEIGHT` was forgotten. Window width was independent of
image aspect. The viewer rect's aspect ratio didn't match the image
aspect ratio → `compute_fit` scaled with `scale_x.min(scale_y)` and
left letterbox bands.

**Fix**: Keep `win_w / (win_h - chrome_h) == iw / ih` so the image
fills the viewer rect with no letterbox. Clamp to monitor 90% and
a minimum window size. Use unified `MIN_W / MIN_H` (later relaxed to
480×320) so `set_fullscreen` doesn't overflow.

**Commits**: `1d35ae3` (initial), refined in `e1bfa5b`, `1100369`.

### 1.5 Viewer in light theme shows as default brush on launch

**Symptom**: User saw viewer bg as white ~`#F0EFEC` in light theme.

**Root cause**: `Direct2DViewer.bg` was hardcoded
`[0.059, 0.063, 0.067]` (dark). `render_frame` synchronised it to
`pal.d2d_clear` per frame, but on the very first paint (before
`render_frame` ran) the default applied, while `WNDCLASS.hbrBackground =
NULL` filled the uninitialised swapchain back buffer with the
COLOR_WINDOW brush.

**Fix**: Same as 1.1 (HOLLOW_BRUSH + WS_EX_TRANSPARENT removal +
sRGB-aware clear). Once the swapchain is initialised with our D2D
content before the first composition, the hardcoded dark fallback
never has a chance to show.

---

## 2. Window state (size / persistence / fullscreen)

### 2.1 Window size grows on every launch at 125% DPI

**Symptom**: User closes the app at minimum size, reopens — window
is slightly bigger. Repeat. The window drifts to monitor size.

**Root cause**: `save_window_geometry()` wrote `window.inner_size()`
(physical pixels) into settings.json. `init_window()` then read that
value back and passed it to `LogicalSize::new(stored_w, stored_h)`. At
100% DPI winit's logical→physical round-trip is a no-op so the bug
hid. At 125% it multiplied the stored physical value by 1.25 each
launch.

**Fix**: Save in logical pixels via `to_logical(scale_factor)`; add
a migration check that detects a stored value exceeding
`1.5 * monitor_logical_90%` (the tell-tale sign of a leftover
physical value) and divides by scale to recover.

**Commit**: `9459965`.

### 2.2 Settings persistence has no migration path

**Symptom**: After DPI fix, old settings.json files from v1.0.x
releases stored physical pixels. After the DPI fix, the window
would suddenly shrink or be off-screen on first launch.

**Root cause**: `init_window` reads the stored value and assumes
it's logical. If old builds wrote physical, the new build reads
physical as logical and the OS applies the current scale factor on
top.

**Fix**: Detect the case where `stored > 1.5 * max_w` or
`> 1.5 * max_h` and divide by `scale_factor` to recover the logical
value. Log a `tracing::warn!` for diagnostics.

**Commit**: `9459965`.

### 2.3 Fullscreen / borderless transition overflows monitor

**Symptom**: Pressing F11 with a single-image launch (large image)
made the window overflow the monitor.

**Root cause**: `min_inner_size = image+chrome` prevented the OS
from shrinking the window below image size. `Borderless(None)`
fullscreen on some Windows/winit versions still honours
`min_inner_size`, producing a window larger than the monitor.

**Fix**: Unify `min_inner_size = MIN_W/MIN_H` (later relaxed to
480×320) so fullscreen never collides with image size. Initial
window size stays at image+chrome (fits-to-image behaviour) but the
user can resize smaller.

**Commits**: `1d35ae3`, `e1bfa5b`.

### 2.4 Minimal window size too conservative

**Symptom**: User could not shrink the window below 960×600, which
felt restrictive for a single-image viewer.

**Fix**: Relax `MIN_W = 960 → 480`, `MIN_H = 600 → 320`. Still big
enough to hold the chrome (40 + 48) plus a few pixels of viewer.

**Commit**: `1100369`.

---

## 3. User input and interactions

### 3.1 Native right-click menus never opened

**Symptom**: Right-clicking an image or a folder in the tree did
nothing.

**Root cause**: `MAIN_HWND` was read everywhere via
`MAIN_HWND.load(...)` but never *written*. It stayed at 0 forever.
`show_native_image_menu` / `show_native_tree_menu` checked
`if hwnd_raw == 0 { return; }` and bailed before the menu could show.

**Fix**: Store the HWND from `window.window_handle()` once in
`init_window`:

```rust
if let Ok(handle) = window.window_handle() {
    if let RawWindowHandle::Win32(win32) = handle.as_raw() {
        MAIN_HWND.store(win32.hwnd.get() as isize, Ordering::Relaxed);
    }
}
```

**Commit**: `fef610a`.

### 3.2 Arrow keys don't "brake" on release

**Symptom**: Holding an arrow key to quickly advance images, then
releasing, the image keeps advancing several frames after release.

**Root cause**: WM_KEYDOWN for arrows fired `handle_navigation`
synchronously each time. OS auto-repeat floods the queue with
KEYDOWNs at ~30Hz. KEYDOWNs queued before WM_KEYUP continued firing
after release — the user perceived this as "inertia".

**First fix attempt (withdrawn)**: Rate-limit KEYDOWN drops inside a
250ms window. Worked but felt artificial — single tap didn't feel
immediate.

**Final fix** (user-requested behaviour — "brake on release, not
limit speed"):

- Arrow KEYDOWN — first press in a fresh hold fires
  `handle_navigation` immediately (tap-on-down feel). Always
  record into `(arrow_held, pending_nav)`.
- Arrow KEYUP — clears `arrow_held` and `pending_nav`.
- Per-frame dispatcher in `render_frame` — if `pending_nav` is
  `Some` AND `arrow_held` matches AND `≥ 200ms` since last arrow
  nav, fire one `handle_navigation`. KEYUP stops the dispatcher on
  the next frame. No rate-limit on KEYDOWN itself.

**Commits**: `e1bfa5b` (rate-limit, reverted in `8abc7a8`).

---

## 4. Releases and major milestones

| Version | Date       | Theme |
|---------|------------|-------|
| v1.0.0  | 2026-08-25 | First tagged release: native menus, turbo icon, Inno Setup installer, defaults sized for 960×600+ |
| v1.0.1  | 2026-08-27 | Hotfix: installer embedded from local `target/release` |
| v1.0.2  | 2026-08-27 | First attempt at single-image aspect fix (later refined) |
| v1.0.3  | 2026-08-27 | WS_VISIBLE-deferred viewer ShowWindow (only fixed the D2D child side of the launch flash) |
| v1.0.4  | 2026-08-27 | Added swapchain Pre-Present before ShowWindow (still had WM_PAINT FillRect overlay) |
| v1.0.5  | 2026-08-27 | HOLLOW_BRUSH on WNDCLASS (definitively fixed the D2D-side flash) |
| v1.0.6  | 2026-08-27 | DPI-aware save/restore + wgpu `present_wgpu_surface_for_init` + sRGB preference + WS_EX_transparent removal |
| v1.0.7  | 2026-08-27 | **All regressions + flash residuals fixed in one shot**: `apply_position` uses `last_applied_rect`, `with_visible(false)` + `set_visible(true)`, sRGB-aware clear, `WAS_STILL_DRAWING` retry. This is the build that finally passed the user's "open / close / open" test cleanly. |

---

## Lessons

These are the meta-lessons from the v1.0.x arc. They should apply to
any Rust + egui + DXGI / Metal / Vulkan hybrid renderer.

- **Hybrid DXGI + egui rendering is timing-fragile.** DWM samples at
  vblank. Your surfaces must Present before that first sample or
  the user sees whatever the OS falls back to (COLOR_WINDOW brush).
  Three layers of defence (hide-until-ready, present immediately,
  HOLLOW_BRUSH) is the minimum for a hidden flash that survives
  edge cases.

- **DPI math fails silently at 100% DPI.** Always unit-test at 125%
  and 175% DPI before declaring window-state fixes done. If you
  save a size to JSON, decide upfront whether it's logical or
  physical and document it in the field name. Migration code for
  legacy files is non-optional.

- **Reuse of a field for two purposes is a bug waiting to happen.**
  `hit_rect` was used both as "what we requested" (in resize) and
  "what we last applied" (in apply_position). The guard that
  compared against it always short-circuited. Separate fields for
  separate semantics, named by what they actually represent
  (`last_size` vs `last_applied_rect`).

- **HOLLOW_BRUSH is the correct default for DXGI swapchain child
  windows.** NULL means system default = COLOR_WINDOW = the symptom
  the user saw.

- **`WS_EX_TRANSPARENT` on a DXGI child is almost always wrong.** It
  tells DWM to not paint the child, which means the DXGI surface is
  excluded from the composition tree. Use `HTTRANSPARENT` for
  hit-test transparency; do not use `WS_EX_TRANSPARENT` for DXGI
  children.

- **First Present after `CreateSwapChainForHwnd` can fail.**
  `DXGI_ERROR_WAS_STILL_DRAWING` is a real possibility on some
  drivers. Always retry with a back-off.

- **Don't be clever about user-facing behaviour.** We first tried
  rate-limiting arrow keypresses; they felt laggy. The user asked
  for "stop on release, don't limit speed". The pending-nav + KEYUP
  brake is exactly that — every press feels immediate, but the
  per-frame dispatcher (gated by 200ms = one slide animation)
  enforces the rate at which advance actually fires.

- **Compose before you reveal.** Create the HWND hidden, do all
  GPU/driver init synchronously, Present both surfaces once, then
  call `set_visible(true)`. This collapses the whole "first DWM
  composition" timing race into a single deterministic moment.

---

## v1.0.8 — egui-native migration pitfalls

The biggest regression arc in this codebase: the viewer moved from a
custom wgpu image-quad pipeline to being painted as an `egui::Mesh` inside
the central panel. Most of the bugs below would be re-introduced by
anyone touching the rendering pipeline without re-reading this section.

### 1. Cross-frame texture use-after-free (`OBJECT_DELETED_WHILE_STILL_IN_USE`)

**Symptom.** Navigating between images (especially rapid Next/Prev
through a folder whose ±1 neighbours are cached full-size) crashed
with `D3D12 ERROR ... ID3D12Resource ... referenced by GPU operations
in-flight ... [EXECUTION ERROR #921: OBJECT_DELETED_WHILE_STILL_IN_USE]`
and exit code `0xCFFFFFFF`.

**Root cause.** The viewer was migrated to `DecodedGpuImage`
holding an `egui::TextureHandle`. The mesh we draw only stores a
`TextureId`. The texture's real lifetime is the `Arc<DecodedGpuImage>`
held by the viewer. Two paths dropped that Arc prematurely:

1. `set_image_gpu` *directional* branch overwrote `previous_gpu`
   with `self.previous_gpu = outgoing`, releasing the old `previous_gpu`
   straight away — even though the slide still needed it for that frame's
   outgoing paint.
2. A frame-count retire budget (`RETIRE_FRAMES`) is a heuristic, not a
   fence: under heavy decode the GPU can lag more than N frames, so
   the texture can still be in flight when we let egui free it.

**Fix.** A *real fence* via wgpu 22's `Maintain::WaitForSubmissionIndex`,
plus routing every outgoing image through a `retired: VecDeque<(Arc, epoch)>`
queue instead of dropping it. Released only after a submit we know has
completed. First attempt blocked on `WaitForSubmissionIndex` — see #2.

### 2. Reentrant lock deadlock via `WaitForSubmissionIndex`

**Symptom.** Process hung at `0xcfffffff` exit, never self-exiting.
Reproduced with ~12 Right-arrows via Win32 `PostMessage` on `C:\…\folder_a`.

**Root cause.** wgpu 22 exposes **no non-blocking** "which submission
completed" query. We first tried `Maintain::WaitForSubmissionIndex` on
the main thread, but:

- The egui render closure took `viewer.lock()` to call `retire_epoch_pending()`.
- The Rust temporary-lifetime rules keep that `parking_lot::MutexGuard`
  alive across the entire `if let` block — including the blocking
  `WaitForSubmissionIndex` call AND the subsequent
  `viewer.lock().release_retired_through(...)`, which is a **reentrant
  lock on the same thread**. `parking_lot` hangs forever on reentry.

**Fix.** Removed the blocking wait entirely. `submit_wgpu_frame`
advances `safe_release_epoch = render_epoch - RETIRE_FRAME_BUDGET` and
releases only entries whose tag epoch is ≤ that watermark. With
`PresentMode::Fifo` + `desired_maximum_frame_latency: 1`, the GPU is
guaranteed ≤ 1 frame behind, so a small `RETIRE_FRAME_BUDGET` (4) is
provably past every in-flight submit. The render thread **never blocks**.
Also fixed the `viewer.lock().retire_epoch_pending()` pattern by scoping
the guard so it doesn't span the release call.

### 3. egui `load_texture` debug_asserts on oversized textures

**Symptom.** Panic in `egui-0.29.1/src/context.rs:2043`:
`Texture "aperture_main" has size 1242x2688, but the maximum texture
side is 2048`.

**Root cause.** `Context::load_texture` enforces a hard `debug_assert`
against `RawInput.max_texture_side` (default `None` ⇒ assumed 2048). The
app fed it textures decoded from 8K source images (1024-8192px on the
long edge) — well over the device's actual `max_texture_dimension_2d`.

**Fix.** Two parts:

1. The coordinator's full-tier clamp now uses
   `(viewport_max * 2).clamp(1080, FULL_RES_DIM.min(max_texture_dim))`,
   where `max_texture_dim` comes from `device.limits()..max_texture_dimension_2d`
   passed through `DecodeCoordinator::new(…)`. `FULL_RES_DIM = 4096`
   is the editorial ceiling.
2. `build_egui_ui` now sets `raw_input.max_texture_side = Some(limit as
   usize)` each frame so egui's `load_texture` uses the real limit.

### 4. 8K thumbnail thread flood

**Symptom.** Opening `C:\Users\1234_\Pictures\4K壁纸` (279 images, up to
8K) froze the app: thumbnails never loaded, UI wedged.

**Root cause.** `decode_thumb_blocking(200, path)` calls
`decode_file(path, 200, 200)` which uses WIC's
`CreateBitmapScaler` to scale from the **full source**. With ~30 thumbs
requesting simultaneously, each one internally decoded ~30 MP of BGRA,
dozens of WIC allocations in flight at once. System thrash + blocked
main thread + stalled decode-pipeline.

**Fix.** A tiny `parking_lot::Mutex<usize>` + `Condvar` semaphore
(`THUMB_DECODE_PERMITS = 4`) around the `std::thread::spawn` body so
only 4 thumbs decode concurrently. The remaining requests queue in
`thumbs_pending` and pick up a permit as soon as one frees. Main thread
never blocks on the semaphore.

### 5. EXIF Orientation is silently ignored

**Status.** **Known gap, unfixed.** WIC decodes source pixels
unrotated; portrait photos from phones/DSLRs render sideways. egui
renders whatever pixels WIC gave it; the viewer has no per-image
rotation metadata. `decode_file` preserves source aspect ratio but not
EXIF `0x0112`. Fixing this requires reading the EXIF `Orientation` tag
in WIC and either rotating the BGRA pixels CPU-side at decode time, or
storing an `orientation` per `DecodedImage` and applying it in
`paint_viewer` (similar to the existing `rotation` state). Track via
follow-up; touched again in the next round.

---

## Files referenced

- `crates/app/src/window.rs` — main window, wgpu surface, egui build,
  submission fence, render loop
- `crates/app/src/texture_cache.rs` — thumbnail cache, semaphore-bounded
  decode, egui::TextureHandle lifecycle
- `crates/gpu/src/viewer.rs` — Direct2DViewer state machine, image→screen
  affine, retire queue + fence release
- `crates/gpu/src/coordinator.rs` — DecodeCoordinator, two-tier cache, full
  clamp, progressive low→full upgrade
- `crates/gpu/src/texture.rs` — DecodedGpuImage (egui TextureHandle), BGRA→RGBA
  swap, average luminance
- `crates/gpu/src/animator.rs` — slide state machine (Animator)
- `crates/gpu/src/lib.rs` — module exports
- `crates/app/src/event_router.rs` — translates winit events into egui
  InputEvents
- `crates/core/src/settings.rs` — JSON-backed settings persistence
  (window size, theme, recents, favorites)
- `Installer/installer.iss` — Inno Setup script (per-user, no admin,
  bilingual EN + 简体中文, file-association registration)