//! GPU device initialization — D3D11 + Direct2D + WIC

use windows::{
    Win32::Foundation::*,
    Win32::Graphics::Direct2D::*,
    Win32::Graphics::Direct2D::Common::*,
    Win32::Graphics::Direct3D::*,
    Win32::Graphics::Direct3D11::*,
    Win32::Graphics::Dxgi::*,
    Win32::Graphics::Imaging::*,
    Win32::System::Com::*,
};
use windows_core::Interface;
use std::sync::Arc;
use anyhow::Result;

pub struct GpuContext {
    pub d3d_device: ID3D11Device,
    pub d3d_context: ID3D11DeviceContext,
    pub dxgi_device: IDXGIDevice,
    pub d2d_factory: ID2D1Factory1,
    pub d2d_device: ID2D1Device,
    pub d2d_dc: ID2D1DeviceContext,
    pub wic_factory: IWICImagingFactory,
}

impl GpuContext {
    pub fn new() -> Result<Arc<Self>> {
        unsafe {
// 1. Initialize COM (ignore result; another thread may have already initialized)
// COINIT_APARTMENTTHREADED is what WIC needs for STA threading.
// We don't fail if it's already initialized with a different mode.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            tracing::debug!("D3D11CreateDevice: starting");

            // 2. D3D11 device
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
            let hr = D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            );
            if hr.is_err() {
                return Err(anyhow::anyhow!("D3D11CreateDevice failed: hr={:?}", hr));
            }
            tracing::debug!("D3D11CreateDevice: OK");
            let d3d_device = device.ok_or_else(|| anyhow::anyhow!("D3D11 device is null"))?;
            let d3d_context = context.ok_or_else(|| anyhow::anyhow!("D3D11 context is null"))?;

            // 3. DXGI device from D3D11 — query IDXGIDevice via QueryInterface
            tracing::debug!("GpuContext: casting D3D11 -> IDXGIDevice");
            let dxgi_device: IDXGIDevice = d3d_device.cast()
                .map_err(|e| anyhow::anyhow!("D3D11->IDXGIDevice cast failed: {:?}", e))?;
            tracing::debug!("GpuContext: cast OK");

// 4. D2D1 factory
            tracing::debug!("GpuContext: creating D2D1 factory");
            let d2d_factory: ID2D1Factory1 = D2D1CreateFactory(
                D2D1_FACTORY_TYPE_SINGLE_THREADED,
                None,
            ).map_err(|e| anyhow::anyhow!("D2D1CreateFactory: {:?}", e))?;
            tracing::debug!("GpuContext: D2D1 factory OK");

            // 5. D2D device + device context
            tracing::debug!("GpuContext: calling CreateDevice");
            let d2d_device = d2d_factory.CreateDevice(&dxgi_device)
                .map_err(|e| anyhow::anyhow!("CreateDevice: {:?}", e))?;
            tracing::debug!("GpuContext: CreateDevice OK");
            tracing::debug!("GpuContext: calling CreateDeviceContext");
            let d2d_dc = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                .map_err(|e| anyhow::anyhow!("CreateDeviceContext: {:?}", e))?;
            tracing::debug!("GpuContext: CreateDeviceContext OK");

            // 6. WIC factory
            let wic_factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| anyhow::anyhow!("CoCreateInstance WIC factory: {:?}", e))?;

            Ok(Arc::new(Self {
                d3d_device,
                d3d_context,
                dxgi_device,
                d2d_factory,
                d2d_device,
                d2d_dc,
                wic_factory,
            }))
        }
    }

    pub fn device(&self) -> &ID2D1DeviceContext {
        &self.d2d_dc
    }
}