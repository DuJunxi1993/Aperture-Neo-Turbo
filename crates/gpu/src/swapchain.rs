//! DXGI SwapChain — binds the D2D device context to an HWND

use windows::{
    Win32::Foundation::*,
    Win32::Graphics::Direct2D::*,
    Win32::Graphics::Direct2D::Common::*,
    Win32::Graphics::Dxgi::*,
    Win32::Graphics::Dxgi::Common::*,
};
use crate::device::GpuContext;
use anyhow::Result;
use std::sync::Arc;

pub struct SwapchainHandle {
    pub swapchain: IDXGISwapChain1,
    pub d2d_bitmap_target: Option<ID2D1Bitmap1>,
    pub gpu: std::sync::Weak<GpuContext>,
    pub hwnd: HWND,
}

pub fn create_swapchain_for_hwnd(
    gpu: &Arc<GpuContext>,
    hwnd: HWND,
    width: u32,
    height: u32,
) -> Result<SwapchainHandle> {
    unsafe {
        // Use the modern IDXGIFactory2 + DXGI_SWAP_CHAIN_DESC1 path with
        // FLIP_DISCARD. The legacy DXGI_SWAP_CHAIN_DESC with DISCARD works
        // for create but ResizeBuffers then fails with DXGI_ERROR_INVALID_CALL.
        let dxgi_device: IDXGIDevice = gpu.dxgi_device.clone();
        let adapter: IDXGIAdapter = dxgi_device.GetAdapter()?;
        let factory: IDXGIFactory2 = adapter.GetParent()?;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: BOOL(0),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            // STRETCH lets the compositor scale a stale-size buffer when the
            // HWND is resized before ResizeBuffers runs — panel drags stay
            // gap-free and ResizeBuffers can be deferred until the size
            // settles.
            Scaling: DXGI_SCALING_STRETCH,
            Flags: 0u32,
        };

        let swapchain: IDXGISwapChain1 = factory.CreateSwapChainForHwnd(
            &gpu.d3d_device,
            hwnd,
            &desc,
            None,
            None,
        ).map_err(|e| anyhow::anyhow!("CreateSwapChainForHwnd: {:?}", e))?;
        tracing::debug!("create_swapchain: CreateSwapChainForHwnd OK");

        let d2d_bitmap_target = create_d2d_target(gpu, &swapchain)?;
        // Set the initial target on the D2D device context.
        gpu.d2d_dc.SetTarget(&d2d_bitmap_target);

        Ok(SwapchainHandle {
            swapchain,
            d2d_bitmap_target: Some(d2d_bitmap_target),
            gpu: Arc::downgrade(gpu),
            hwnd,
        })
    }
}

fn create_d2d_target(
    gpu: &GpuContext,
    swapchain: &IDXGISwapChain1,
) -> Result<ID2D1Bitmap1> {
    unsafe {
        let surface: IDXGISurface = swapchain.GetBuffer(0)
            .map_err(|e| anyhow::anyhow!("GetBuffer: {:?}", e))?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_UNKNOWN,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: core::mem::ManuallyDrop::new(None),
        };
        let target = gpu.d2d_dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))
            .map_err(|e| anyhow::anyhow!("CreateBitmapFromDxgiSurface: {:?}", e))?;
        tracing::debug!("create_d2d_target: OK");
        Ok(target)
    }
}

pub fn resize_swapchain(handle: &mut SwapchainHandle, width: u32, height: u32) -> Result<()> {
    unsafe {
        // 1) Unbind the D2D target so the swapchain's buffers can be released.
        if let Some(gpu) = handle.gpu.upgrade() {
            gpu.d2d_dc.SetTarget(None);
        }
        // 2) Drop the old target so the swapchain buffer is fully released.
        handle.d2d_bitmap_target = None;

        // 3) Resize the swapchain buffers.
        let hr = handle.swapchain.ResizeBuffers(
            2u32,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_SWAP_CHAIN_FLAG(0),
        );

        // 4) Re-create the D2D bitmap target and re-bind it — even on
        //    ResizeBuffers failure we must restore a valid target so the
        //    D2D device context doesn't render to nothing.
        if let Some(gpu) = handle.gpu.upgrade() {
            if let Ok(new_target) = create_d2d_target(&gpu, &handle.swapchain) {
                gpu.d2d_dc.SetTarget(&new_target);
                handle.d2d_bitmap_target = Some(new_target);
            }
        }

        if hr.is_err() {
            return Err(anyhow::anyhow!("ResizeBuffers failed: {:?}", hr));
        }
        Ok(())
    }
}

pub fn present(handle: &SwapchainHandle) -> Result<()> {
    unsafe {
        // SyncInterval 1 = wait for vertical blank (tear-free animation).
        // The very first Present after CreateSwapChainForHwnd can
        // legitimately fail with DXGI_ERROR_WAS_STILL_DRAWING on
        // some drivers (the swapchain is still warming up; vblank is
        // one frame away). That failure leaves the back buffer
        // uninitialised and DWM samples garbage for the very first
        // composition — the launch flash. Retry with a short
        // back-off so the very first paint always lands.
        let mut hr = handle.swapchain.Present(1, DXGI_PRESENT(0));
        if hr.is_err() {
            const WAS_STILL_DRAWING: u32 = 0x887A000A;
            let mut backoff_ms: u32 = 2;
            for _ in 0..6 {
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms.into()));
                hr = handle.swapchain.Present(1, DXGI_PRESENT(0));
                if !hr.is_err() {
                    break;
                }
                if hr.0 as u32 != WAS_STILL_DRAWING {
                    break;
                }
                backoff_ms = backoff_ms.saturating_mul(2).min(32);
            }
        }
        if hr.is_err() {
            return Err(anyhow::anyhow!("Present failed: {:?}", hr));
        }
        Ok(())
    }
}

/// Current swapchain buffer dimensions (may lag the HWND during deferred
/// resizes — the renderer letterboxes into this aspect to stay undistorted).
pub fn buffer_size(handle: &SwapchainHandle) -> (u32, u32) {
    unsafe {
        match handle.swapchain.GetDesc() {
            Ok(desc) => (desc.BufferDesc.Width, desc.BufferDesc.Height),
            Err(_) => (0, 0),
        }
    }
}
