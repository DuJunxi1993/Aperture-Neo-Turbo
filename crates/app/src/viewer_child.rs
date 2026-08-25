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

pub struct ViewerChildWindow {
    pub hwnd: HWND,
    pub gpu: Arc<GpuContext>,
    pub swapchain: SwapchainHandle,
    pub viewer: Arc<Mutex<Direct2DViewer>>,
    pub hit_rect: RECT,
    pub last_size: (u32, u32),
    /// Time the size last changed — ResizeBuffers is deferred until the
    /// size has been stable briefly (avoids per-frame resize during drags).
    pending_resize_since: Option<std::time::Instant>,
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
                // WS_EX_TRANSPARENT: the child is mouse-event transparent so
                // clicks on the image pass through to the parent winit window
                // (where the parent routes them to either the viewer logic
                // or egui). The D2D child is purely a render surface.
                WS_EX_NOPARENTNOTIFY | WS_EX_TRANSPARENT,
                VIEWER_CLASS_NAME,
                PCWSTR::from_raw(EMPTY_W.as_ptr()),
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

            Ok(Self {
                hwnd,
                gpu,
                swapchain,
                viewer,
                hit_rect: RECT {
                    left: x, top: y,
                    right: x + width as i32,
                    bottom: y + height as i32,
                },
                last_size: (width, height),
                pending_resize_since: None,
            })
        }
    }

    pub fn render(&self) -> Result<()> {
        let mut viewer = self.viewer.lock();
        let (bw, bh) = aperture_gpu::buffer_size(&self.swapchain);
        viewer.render(&self.swapchain, bw, bh)?;
        Ok(())
    }

    pub fn resize(&mut self, x: i32, y: i32, width: u32, height: u32) -> Result<()> {
        // If the size hasn't changed, nothing to do.
        if self.last_size == (width, height)
            && self.hit_rect.left == x && self.hit_rect.top == y
        {
            return Ok(());
        }

        // Reposition + retarget the viewport immediately (cheap). The
        // swapchain buffer resize is DEFERRED (see flush_pending_resize):
        // DXGI_SCALING_STRETCH displays the old buffer scaled until then,
        // so panel drags stay smooth and gap-free.
        self.viewer.lock().resize(width, height, x as f32, y as f32);

        self.hit_rect = RECT {
            left: x, top: y,
            right: x + width as i32,
            bottom: y + height as i32,
        };
        if self.last_size != (width, height) {
            self.last_size = (width, height);
            self.pending_resize_since = Some(std::time::Instant::now());
        }
        Ok(())
    }

    /// Run the deferred ResizeBuffers once the size has been stable for a
    /// moment (e.g. after a panel drag ends).
    pub fn flush_pending_resize(&mut self) {
        let Some(since) = self.pending_resize_since else { return };
        if since.elapsed() < std::time::Duration::from_millis(150) {
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

    pub fn apply_position(&self, x: i32, y: i32, width: u32, height: u32) {
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOP,
                x, y,
                width as i32, height as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.last_size
    }

    /// Convert a global cursor position to a viewer-local position
    /// Returns None if cursor is outside the viewer rect
    pub fn cursor_to_local(&self, x: i32, y: i32) -> Option<(f32, f32)> {
        if x >= self.hit_rect.left && x < self.hit_rect.right
            && y >= self.hit_rect.top && y < self.hit_rect.bottom
        {
            Some(((x - self.hit_rect.left) as f32, (y - self.hit_rect.top) as f32))
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
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: VIEWER_CLASS_NAME,
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc as *const _);
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