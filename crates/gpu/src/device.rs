//! GPU device initialization — D3D11 + WIC

use windows::{
    Win32::Foundation::*,
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
    pub wic_factory: IWICImagingFactory,
}

impl GpuContext {
    pub fn new() -> Result<Arc<Self>> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            tracing::debug!("D3D11CreateDevice: starting");

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

            tracing::debug!("GpuContext: casting D3D11 -> IDXGIDevice");
            let dxgi_device: IDXGIDevice = d3d_device.cast()
                .map_err(|e| anyhow::anyhow!("D3D11->IDXGIDevice cast failed: {:?}", e))?;
            tracing::debug!("GpuContext: cast OK");

            let wic_factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| anyhow::anyhow!("CoCreateInstance WIC factory: {:?}", e))?;

            Ok(Arc::new(Self {
                d3d_device,
                d3d_context,
                dxgi_device,
                wic_factory,
            }))
        }
    }
}
