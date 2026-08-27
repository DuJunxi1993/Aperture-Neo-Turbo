//! ViewerChildWindow — Direct2D rendering surface embedded as a child HWND
//!
//! The window class `ApertureNeoTurboViewer` is registered once. Rendering
//! is driven by the parent's `RedrawRequested` (calling `render()`),
//! not by WM_PAINT — that prevents the classic SetTarget-stomping bug
//! where WM_PAINT re-enters between BeginDraw/EndDraw.

use anyhow::Result;
use std::sync::Arc;
use parking_lot::Mutex;
use aperture_gpu::{GpuContext, Direct2DViewer, create_swapchain_for_hwnd, SwapchainHandle};
use windows::{
    Win32::Foundation::*,
    Win32::Graphics::Dxgi::*,
    Win32::Graphics::Gdi::*,
    Win32::UI::WindowsAndMessaging::*,
};
use windows_core::PCWSTR;
use std::sync::Once;

/// Sum of |Δw| + |Δh| above which a resize is treated as a "big jump"
/// (fullscreen toggle, large window resize). The deferred 150 ms
/// ResizeBuffers path is too slow for these transitions — the swapchain
/// buffer stays stale for the whole jump and DXGI_SCALING_STRETCH
/// non-uniformly scales it, visibly distorting the image. Big jumps
/// take the immediate path: ResizeBuffers + SetWindowPos in one call.
/// Phase 9: 200 → 100 px so mid-size layout jumps (panel toggle while
/// an animation is starting) also stay in sync.
const BIG_JUMP_PIXELS: u64 = 100;

pub struct ViewerChildWindow {
    pub hwnd: HWND,
    pub gpu: Arc<GpuContext>,
    pub swapchain: SwapchainHandle,
    pub viewer: Arc<Mutex<Direct2DViewer>>,
    pub last_size: (u32, u32),
    /// Time the size last changed — ResizeBuffers is deferred until the
    /// size has been stable briefly (avoids per-frame resize during drags).
    pending_resize_since: Option<std::time::Instant>,
    /// Last `(x, y, w, h)` actually passed to SetWindowPos on the
    /// child HWND. Distinct from `last_size` (which tracks only the
    /// requested size and is updated every resize). Comparing the
    /// guard in `apply_position` against this field is what keeps
    /// the HWND actually tracking every resize — without this, a
    /// guard that compared against a field updated by `resize()`
    /// would always short-circuit and the HWND would never move
    /// for slow drags (the cause of the resize distortion and the
    /// thumbs-panel occlusion regressions). Public because
    // `action_toggle_fullscreen` reads it to compute the path target.
    pub last_applied_rect: (i32, i32, u32, u32),
}

impl ViewerChildWindow {
    pub fn new(
        parent_hwnd: HWND,
        gpu: Arc<GpuContext>,
        viewer: Arc<Mutex<Direct2DViewer>>,
        x: i32, y: i32,
        width: u32, height: u32,
    ) -> Result<Self> {
        unsafe {
            tracing::debug!("ViewerChildWindow::new: registering class");
            register_class_once()?;
            tracing::debug!("ViewerChildWindow::new: class OK");

            let hwnd = CreateWindowExW(
                // Hit-test transparency is handled by returning HTTRANSPARENT from
                // the child's WM_NCHITTEST (see wnd_proc below). Do NOT
                // add WS_EX_TRANSPARENT here — per MSDN that style "the
                // window itself is not painted", which DWM interprets
                // as "exclude this window's DXGI surface from the
                // composition tree" and falls back to drawing the
                // parent's wgpu surface (or, on the very first
                // composition, the parent has not presented yet and
                // DWM shows the COLOR_WINDOW brush — the persistent
                // light-grey placeholder rect the user has been seeing
                // on launch).
                WS_EX_NOPARENTNOTIFY,
                VIEWER_CLASS_NAME,
                PCWSTR::from_raw(EMPTY_W.as_ptr()),
                // WS_VISIBLE is set so the child is composited from the
                // start. Combined with HOLLOW_BRUSH (see the WNDCLASS
                // setup) and an immediate self.render() at the end of
                // new(), the first composition shows our D2D content
                // (viewer.bg color = theme dark) instead of a default
                // system brush placeholder.
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_OVERLAPPED,
                x, y, width as i32, height as i32,
                parent_hwnd,
                HMENU(std::ptr::null_mut()),
                HINSTANCE(std::ptr::null_mut()),
                None,
            ).map_err(|e| anyhow::anyhow!("CreateWindowExW: {:?}", e))?;
            tracing::debug!("ViewerChildWindow::new: child HWND = {:?}", hwnd.0);

            tracing::debug!("ViewerChildWindow::new: creating swapchain");
            let swapchain = create_swapchain_for_hwnd(&gpu, hwnd, width, height)?;
            tracing::debug!("ViewerChildWindow::new: swapchain OK");

            // Update the shared viewer's viewport so the existing image (if any)
            // is rendered at the new size on the next frame.
            viewer.lock().resize(width, height, x as f32, y as f32);

            let me = Self {
                hwnd,
                gpu,
                swapchain,
                viewer,
                last_size: (width, height),
                pending_resize_since: None,
                // Pre-seed last_applied_rect to the requested rect
                // so apply_position's first call (which sets the HWND
                // via SetWindowPos) actually fires. Without this, the
                // resize() inside new() — which already updated
                // last_size and pending_resize_since — would otherwise
                // pair with apply_position to do nothing.
                last_applied_rect: (x, y, width, height),
            };
            // Paint once so the DXGI swapchain back buffer is
            // initialised with viewer.bg (theme dark) before the OS
            // triggers WM_PAINT for the WS_VISIBLE child. Without
            // this, the first composition sees an uninit back buffer
            // and DWM falls back to filling the child rect with the
            // WNDCLASS hbrBackground brush.
            //
            // DO NOT silently `let _ =` this — if BeginDraw/Clear/
            // EndDraw/Present fails, the very first back buffer is
            // left undefined and DWM samples an uninitialised surface,
            // which manifests as the COLOR_WINDOW placeholder flash.
            // Log so a regression is visible in tracing immediately.
            if let Err(e) = me.render_initial() {
                tracing::warn!("ViewerChildWindow::new: initial paint failed: {:#}", e);
            }
            Ok(me)
        }
    }

    /// Paint D2D content into the swapchain and Present once. Used
    /// from new() so the first WM_PAINT after ShowWindow finds the
    /// swapchain back buffer populated with viewer.bg (theme dark)
    /// rather than the uninitialised state DXGI leaves before the
    /// first Present.
    fn render_initial(&self) -> Result<()> {
        let mut viewer = self.viewer.lock();
        let (bw, bh) = aperture_gpu::buffer_size(&self.swapchain);
        viewer.render(&self.swapchain, bw, bh)?;
        Ok(())
    }

    pub fn render(&self) -> Result<()> {
        let mut viewer = self.viewer.lock();
        let (bw, bh) = aperture_gpu::buffer_size(&self.swapchain);
        viewer.render(&self.swapchain, bw, bh)?;
        Ok(())
    }

    pub fn resize(&mut self, x: i32, y: i32, width: u32, height: u32) -> Result<()> {
        // If the request matches the LAST APPLIED rect (not just the
        // last requested size), nothing to do. Comparing against
        // `last_applied_rect` is what keeps the HWND in sync — see
        // the field doc comment for the resize/apply_position
        // interaction.
        if self.last_applied_rect == (x, y, width, height) {
            return Ok(());
        }

        // Reposition + retarget the viewport immediately (cheap). The
        // swapchain buffer resize is DEFERRED (see flush_pending_resize):
        // DXGI_SCALING_STRETCH displays the old buffer scaled until then,
        // so panel drags stay smooth and gap-free.
        self.viewer.lock().resize(width, height, x as f32, y as f32);

        // Big-jump fast path: when the size delta exceeds BIG_JUMP_PIXELS
        // (sum of |Δw| + |Δh|), a fullscreen toggle or window-resize is
        // in progress. Deferring the ResizeBuffers call for 150 ms leaves
        // the swapchain buffer stale for the entire transition, and
        // STRETCH's non-uniform aspect during that gap visibly distorts
        // the image. Forcing an immediate ResizeBuffers + SWP_FRAMECHANGED
        // keeps the buffer in step with the HWND through the whole jump.
        let (old_w, old_h) = self.last_size;
        let delta = (width as i64 - old_w as i64).unsigned_abs()
            + (height as i64 - old_h as i64).unsigned_abs();
        if delta > BIG_JUMP_PIXELS {
            // Reset last_size delta to zero *before* resize_swapchain,
            // so the deferred path doesn't immediately re-fire after us.
            self.last_size = (width, height);
            self.pending_resize_since = None;
            let _ = aperture_gpu::resize_swapchain(&mut self.swapchain, width, height);
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd, HWND_TOP, x, y, width as i32, height as i32,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
            // We just did the SetWindowPos ourselves; record it.
            self.last_applied_rect = (x, y, width, height);
        } else {
            // Small per-frame delta: defer ResizeBuffers. apply_position
            // is called right after resize by every callsite, and it
            // will fire the actual SetWindowPos based on its own guard
            // against last_applied_rect.
            self.last_size = (width, height);
            self.pending_resize_since = Some(std::time::Instant::now());
        }
        Ok(())
    }

    /// Run the deferred ResizeBuffers once the size has been stable for a
    /// moment (e.g. after a panel drag ends).
    ///
    /// Phase 9: flush IMMEDIATELY while the viewer is transitioning
    /// (slide / rect path animation) — presenting a stale buffer during
    /// an animation lets DXGI_STRETCH apply its non-uniform scale on
    /// top of the animated transform, which reads as bounce/skew. The
    /// 150 ms debounce remains for plain drags, where smoothness
    /// matters more than buffer freshness.
    pub fn flush_pending_resize(&mut self) {
        let Some(since) = self.pending_resize_since else { return };
        let transitioning = self.viewer.lock().is_transitioning();
        if !transitioning && since.elapsed() < std::time::Duration::from_millis(150) {
            return;
        }
        self.pending_resize_since = None;
        let (width, height) = self.last_size;
        let resize_ok = aperture_gpu::resize_swapchain(&mut self.swapchain, width, height).is_ok();
        if !resize_ok {
            tracing::warn!("ResizeBuffers failed for {}x{}, recreating swapchain", width, height);
            match aperture_gpu::create_swapchain_for_hwnd(&self.gpu, self.hwnd, width, height) {
                Ok(new_handle) => {
                    self.swapchain = new_handle;
                }
                Err(e) => {
                    tracing::error!("recreate swapchain failed: {:#}", e);
                }
            }
        }
    }

    pub fn apply_position(&mut self, x: i32, y: i32, width: u32, height: u32) {
        // Skip the SetWindowPos if the rect matches the LAST APPLIED
        // rect (not just the last requested size — that's stored in
        // last_size and is bumped on every resize() call regardless
        // of whether anything actually moved). Comparing against
        // `last_applied_rect` means SetWindowPos fires once when the
        // requested rect actually changes, and is a no-op for the
        // many redundant apply_position calls per frame when nothing
        // has changed.
        //
        // This MUST be &mut self and MUST update `last_applied_rect`
        // after a successful SetWindowPos — the resize/apply_position
        // pair used to compare against `hit_rect`, which resize() also
        // updated, so the guard always short-circuited and the HWND
        // never tracked per-frame resizes. That broke resize distortion
        // and the thumbs-panel reopen occlusion regression.
        if self.last_applied_rect == (x, y, width, height) {
            return;
        }
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOP,
                x, y,
                width as i32, height as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.last_applied_rect = (x, y, width, height);
    }

    pub fn size(&self) -> (u32, u32) {
        self.last_size
    }

    /// Convert a global cursor position to a viewer-local position
    /// Returns None if cursor is outside the viewer rect
    pub fn cursor_to_local(&self, x: i32, y: i32) -> Option<(f32, f32)> {
        let (lx, ly, lw, lh) = self.last_applied_rect;
        let r = lx + lw as i32;
        let b = ly + lh as i32;
        if x >= lx && x < r && y >= ly && y < b {
            Some(((x - lx) as f32, (y - ly) as f32))
        } else {
            None
        }
    }
}

const VIEWER_CLASS_NAME: PCWSTR = PCWSTR::from_raw(VIEWER_CLASS_W.as_ptr());
const VIEWER_CLASS_W: [u16; 22] = [
    b'A' as u16, b'p' as u16, b'e' as u16, b'r' as u16, b't' as u16, b'u' as u16, b'r' as u16,
    b'e' as u16, b'N' as u16, b'e' as u16, b'o' as u16, b'T' as u16, b'u' as u16, b'r' as u16,
    b'b' as u16, b'o' as u16, b'V' as u16, b'i' as u16, b'e' as u16, b'w' as u16, b'e' as u16,
    0u16,
];
const EMPTY_W: [u16; 1] = [0u16];

static REGISTERED: Once = Once::new();

unsafe fn register_class_once() -> Result<()> {
    REGISTERED.call_once(|| {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(std::ptr::null_mut()),
            hCursor: HCURSOR(std::ptr::null_mut()),
            // HOLLOW_BRUSH (no GDI background paint) instead of the
            // default NULL/system-COLOR_WINDOW brush. With NULL, the
            // WM_PAINT that ShowWindow(SW_SHOW) triggers makes
            // DefWindowProcW FillRect the window with the system
            // window brush (~#F0EFEC) on top of our DXGI swapchain —
            // the user sees a light "placeholder" rect until the next
            // render_frame overpaints it (the black/white launch
            // flash we kept chasing). HOLLOW_BRUSH tells
            // DefWindowProcW not to paint any background, so DWM
            // composites the DXGI swapchain content directly.
            hbrBackground: HBRUSH(HOLLOW_BRUSH.0 as *mut core::ffi::c_void),
            lpszClassName: VIEWER_CLASS_NAME,
            ..Default::default()
        };
        let atom = RegisterClassExW(&wc as *const _);
        if atom == 0 {
            // Class registration failed. Subsequent CreateWindowExW
            // would fail too, but logging here gives the failure
            // visibility without the per-window noise. Most likely
            // cause: a stale class with the same name from a previous
            // process whose window still owns the desktop; usually
            // resolves on the next launch.
            tracing::warn!(
                "RegisterClassExW({}) failed: {}",
                String::from_utf16_lossy(&VIEWER_CLASS_W),
                std::io::Error::last_os_error(),
            );
        }
    });
    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // Direct2D rendering is driven by the parent window's
            // RedrawRequested, so swallow WM_PAINT to prevent
            // re-entering the render path.
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => LRESULT(0),
        // Make the child fully transparent to hit-testing: all mouse
        // messages (move / click / wheel) fall through to the parent winit
        // window. WS_EX_TRANSPARENT alone does NOT do this for child
        // windows — without HTTRANSPARENT the child swallows every mouse
        // event over the image (breaking pan, wheel-zoom and the
        // fullscreen chrome wake-up).
        WM_NCHITTEST => LRESULT(-1), // HTTRANSPARENT
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}