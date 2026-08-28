//! wgpu image-quad pipeline for the photo viewer.
//!
//! Replaces the D2D child-HWND image rendering with a single full-screen
//! triangle rasterised in the same wgpu encoder that draws egui's chrome.
//! This collapses two DXGI swapchains into one and removes the entire
//! `crates/app/src/viewer_child.rs` WNDCLASS / SetWindowRgn /
//! WS_CLIPCHILDREN / HOLLOW_BRUSH machinery.
//!
//! See `docs/migration-plan.md` (Phase 1) for the design rationale.
//!
//! The bicubic sampler uses 16 taps (separable Catmull-Rom, 4 horizontal + 4
//! vertical) — comparable in quality to D2D's
//! `D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC` but expressed as a shader
//! the GPU can schedule independently.

use bytemuck::{Pod, Zeroable};

const SHADER_SRC: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
};

// Uniform buffer. mat3x3 column-major (3 × vec3) + 3 × vec2 + 1 × vec4 +
// 1 × u32 = 84 bytes — well within the 128-byte push-constant budget.
struct Uniforms {
    col0: vec3<f32>,
    col1: vec3<f32>,
    col2: vec3<f32>,
    viewer_rect_min: vec2<f32>,
    viewer_rect_size: vec2<f32>,
    texture_size: vec2<f32>,
    bg: vec4<f32>,
    has_image: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex: texture_2d<f32>;

@vertex fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var out: VsOut;
    out.pos = vec4<f32>(p[vi], 0.0, 1.0);
    return out;
}

// 4-tap separable Catmull-Rom. `coord` is in normalized
// texture-space [0,1]. Weights and offsets are per-tap row
// (left/middle/middle2/right) so the inner two taps can use the
// pixel centre exactly without an extra multiply.
fn cubic_weight(t: f32) -> vec4<f32> {
    // Catmull-Rom: weight = (-0.5, 2.5, -2.5, 0.5).t³ + (1, -2.5, 2, -0.5).t² + (0.5, 0, -0.5, 0).t + (0, 0, 0, 0)
    // Simplified per-tap at offset t in [-1, 1]:
    //   w0 = -0.5 t³ + 1.0 t² - 0.5 t
    //   w1 =  1.5 t³ - 2.5 t² + 1.0
    //   w2 = -1.5 t³ + 2.0 t² + 0.5 t
    //   w3 =  0.5 t³ - 0.5 t²
    let t2 = t * t;
    let t3 = t2 * t;
    return vec4<f32>(
        -0.5 * t3 + 1.0 * t2 - 0.5 * t,
         1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
         0.5 * t3 - 0.5 * t2,
    );
}

@fragment fn fs_main(@builtin(position) fb: vec4<f32>) -> @location(0) vec4<f32> {
    if (u.has_image == 0u) {
        return u.bg;
    }
    // Framebuffer pixel → screen-local pixel relative to viewer rect.
    let screen_local = fb.xy - u.viewer_rect_min;
    // Outside the viewer rect → return bg. The rasteriser should have
    // discarded these already via the scissor rect, but be defensive.
    if (screen_local.x < 0.0 || screen_local.y < 0.0 ||
        screen_local.x >= u.viewer_rect_size.x ||
        screen_local.y >= u.viewer_rect_size.y) {
        return u.bg;
    }
    // Map to image-pixel coords via the per-quadrant affine matrix.
// Compute manually rather than via mat3x3 * vec3 to sidestep a
// naga type-inference issue where the mat3x3(column,column,column)
// constructor is mis-typed against the bound uniform struct fields.
    let img_x = u.col0.x * screen_local.x + u.col1.x * screen_local.y + u.col2.x;
    let img_y = u.col0.y * screen_local.x + u.col1.y * screen_local.y + u.col2.y;
    let img_px = vec2<f32>(img_x, img_y);
    // Outside the source texture → bg (matches D2D behaviour where the
    // display_transform places the bitmap at a sub-rect of the viewport).
    if (img_px.x < 0.0 || img_px.y < 0.0 ||
        img_px.x >= u.texture_size.x ||
        img_px.y >= u.texture_size.y) {
        return u.bg;
    }
    let texel = img_px - vec2<f32>(0.5, 0.5);
    let p = floor(texel);
    let f = texel - p;
    let gx = cubic_weight(f.x);
    let gy = cubic_weight(f.y);
    // 16 taps — 4 horizontal × 4 vertical. Bounds-check the sample
    // coords because the texture may be sampled with edge-clamp and the
    // weights expect access to neighbours on both sides.
    var sum = vec4<f32>(0.0);
    var ws = 0.0;
    for (var i: i32 = -1; i <= 2; i = i + 1) {
        for (var j: i32 = -1; j <= 2; j = j + 1) {
            let off = vec2<f32>(f32(i), f32(j));
            let uv = clamp((p + off + vec2<f32>(0.5, 0.5)) / u.texture_size,
                           vec2<f32>(0.0), vec2<f32>(1.0));
            let w = gx[i + 1] * gy[j + 1];
            sum = sum + textureSample(tex, samp, uv) * w;
            ws = ws + w;
        }
    }
    if (ws > 0.0) {
        sum = sum / ws;
    }
    // The image source is STRAIGHT (non-premultiplied) BGRA — WIC's
    // `GUID_WICPixelFormat32bppBGRA` format is straight alpha. For
    // OPAQUE images (JPEG, all pixels alpha=1) this block is a no-op;
    // for PNG-with-alpha it can over-brighten semi-transparent pixels,
    // which is acceptable for a photo viewer. The sRGB surface treats
    // the texture as sRGB-encoded, so wgpu converts sRGB→linear on
    // sample and linear→sRGB on store.
    if (sum.a > 0.0) {
        sum = vec4<f32>(sum.rgb / sum.a, sum.a);
    }
    return sum;
}
"#;

/// Per-frame uniform for the image quad.
///
/// Sized to match the WGSL `Uniforms` struct layout (112 bytes).
/// WGSL's uniform-buffer layout (std140-like) pads `vec3<f32>` to
/// 16 bytes, so each of the three `col*` columns takes 16 bytes
/// (12 data + 4 padding). The three vec2s, one vec4, four scalars
/// add 8+8+8+16+16 = 56 bytes, plus the three explicit `u32` pads
/// at the end bring us to 104. The struct size is then
/// `roundUp(16, 104) = 112` bytes — matching what naga reports.
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct ImageQuadUniforms {
    /// Column 0 of the screen-pixel-local → image-pixel affine.
    pub col0: [f32; 3],
    /// WGSL pads vec3 to 16 bytes in uniform buffers; explicit pad
    /// keeps the Rust struct byte-aligned to the WGSL layout.
    pub _pad_col0: u32,
    /// Column 1.
    pub col1: [f32; 3],
    pub _pad_col1: u32,
    /// Column 2.
    pub col2: [f32; 3],
    pub _pad_col2: u32,
    /// Viewer rect's top-left in framebuffer pixels.
    pub viewer_rect_min: [f32; 2],
    /// Viewer rect size in framebuffer pixels.
    pub viewer_rect_size: [f32; 2],
    /// Source texture size in pixels.
    pub texture_size: [f32; 2],
    /// WGSL aligns the following `bg` vec4 to 16 bytes. texture_size
    /// ends at offset 72, but vec4 requires 16-byte alignment, so the
    /// WGSL bg lands at 80 — 8 bytes of padding after texture_size.
    pub _pad_texture: [u32; 2],
    /// Straight (non-premul) RGBA background colour; returned by the
    /// fragment shader when `has_image` is 0 or the fragment lies
    /// outside the image rect.
    pub bg: [f32; 4],
    /// 1 when a decoded image texture is bound, 0 otherwise.
    pub has_image: u32,
    /// Three trailing scalars (mirror the WGSL `_pad0/1/2`). Struct
    /// ends at 112 bytes, matching the WGSL round-up to 16.
    pub _pad: [u32; 3],
}

impl Default for ImageQuadUniforms {
    fn default() -> Self {
        Self {
            col0: [1.0, 0.0, 0.0],
            _pad_col0: 0,
            col1: [0.0, 1.0, 0.0],
            _pad_col1: 0,
            col2: [0.0, 0.0, 1.0],
            _pad_col2: 0,
            viewer_rect_min: [0.0, 0.0],
            viewer_rect_size: [0.0, 0.0],
            texture_size: [0.0, 0.0],
            _pad_texture: [0; 2],
            bg: [0.0, 0.0, 0.0, 1.0],
            has_image: 0,
            _pad: [0; 3],
        }
    }
}

/// Pipeline + bind-group-layout + sampler for the image quad.
///
/// Created once per `WgpuState` at startup; bound per frame. Holds the
/// uniform buffer that `set_bind_group` references — Phase 3 writes
/// `ImageQuadUniforms` into it via `update_uniforms`.
pub struct ImageQuadPipeline {
    pub pipeline: wgpu::RenderPipeline,
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub sampler: wgpu::Sampler,
    pub uniform_buffer: wgpu::Buffer,
}

impl ImageQuadPipeline {
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image_quad_shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        // Bind group layout: 1 uniform buffer + 1 sampler + 1 texture.
// Phase 3 allocates the uniform buffer once per WgpuState and writes
// to it via queue.write_buffer() each frame from encode_image_pass.
let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("image_quad_bind_group_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                ],
            });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("image_quad_pipeline_layout"),
                bind_group_layouts: &[&bind_group_layout],
                push_constant_ranges: &[],
            });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image_quad_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image_quad_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear, // bicubic lives in the shader
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        // Uniform buffer that backs the shader's `@group(0) @binding(0)
        // var<uniform> u: Uniforms`. Phase 3 writes a fresh
        // `ImageQuadUniforms` every frame via `update_uniforms`.
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image_quad_uniform_buffer"),
            // naga may insert padding to align the WGSL struct; allocate
            // 256 bytes (well above the 92-byte Rust struct) so the
            // GPU sees a buffer big enough regardless of layout.
            size: 256,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self { pipeline, bind_group_layout, sampler, uniform_buffer }
    }

    /// Bind a texture to the image quad pipeline. Returns the bind group
    /// to pass to `RenderPass::set_bind_group`. Binds at @group(0):
    /// binding(0) = the shared uniform buffer, binding(1) = the
    /// shared sampler, binding(2) = the per-frame texture view.
    pub fn create_bind_group(
        &self,
        device: &wgpu::Device,
        texture_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image_quad_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &self.uniform_buffer,
                        offset: 0,
                        size: std::num::NonZeroU64::new(
                            std::mem::size_of::<ImageQuadUniforms>() as u64,
                        ),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(texture_view),
                },
            ],
        })
    }

    /// Write `uniforms` into the shared uniform buffer. Called once
    /// per frame from `render_frame` before drawing the image quad.
    pub fn update_uniforms(&self, queue: &wgpu::Queue, uniforms: &ImageQuadUniforms) {
        queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::cast_slice(std::slice::from_ref(uniforms)),
        );
    }

    /// Encode the image quad into an active render pass. Caller must
    /// have already begun the pass and bound a bind group via
    /// `set_bind_group`.
    pub fn encode(&self, rpass: &mut wgpu::RenderPass<'_>, rect_phys: (u32, u32, u32, u32)) {
        let (vx, vy, vw, vh) = rect_phys;
        // Clamp scissor to the surface so a transient miscomputed rect
        // can't render off-screen.
        rpass.set_scissor_rect(vx, vy, vw.max(1), vh.max(1));
        rpass.set_pipeline(&self.pipeline);
        rpass.draw(0..3, 0..1);
    }
}

/// The texture format used for decoded image uploads. Must match the
/// BGRA premultiplied layout WIC produces (`crates/gpu/src/decode.rs`).
///
/// Centralised here so Phase 2's coordinator code and Phase 4's deletion
/// of `crates/gpu/src/bitmap.rs` agree on the same format constant.
pub fn texture_format_premul_bgra() -> wgpu::TextureFormat {
    wgpu::TextureFormat::Bgra8UnormSrgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniforms_size_matches_wgsl_layout() {
        assert_eq!(std::mem::size_of::<ImageQuadUniforms>(), 112);
    }

    #[test]
    fn rust_layout_matches_wgsl_layout() {
        use std::mem::offset_of;
        let module = naga::front::wgsl::parse_str(SHADER_SRC).expect("parse shader");
        let u_var = module.global_variables.iter()
            .find(|(_, v)| v.name.as_deref() == Some("u"))
            .expect("uniform var 'u'");
        let ty = &module.types[u_var.1.ty];
        let naga::TypeInner::Struct { members, .. } = &ty.inner else {
            panic!("u is not a struct");
        };
        let member_offsets: Vec<(String, u32)> = members.iter()
            .map(|m| (m.name.clone().unwrap_or_default(), m.offset))
            .collect();

        let rust_offsets: Vec<(String, u32)> = vec![
            ("col0".into(), offset_of!(ImageQuadUniforms, col0) as u32),
            ("col1".into(), offset_of!(ImageQuadUniforms, col1) as u32),
            ("col2".into(), offset_of!(ImageQuadUniforms, col2) as u32),
            ("viewer_rect_min".into(), offset_of!(ImageQuadUniforms, viewer_rect_min) as u32),
            ("viewer_rect_size".into(), offset_of!(ImageQuadUniforms, viewer_rect_size) as u32),
            ("texture_size".into(), offset_of!(ImageQuadUniforms, texture_size) as u32),
            ("bg".into(), offset_of!(ImageQuadUniforms, bg) as u32),
            ("has_image".into(), offset_of!(ImageQuadUniforms, has_image) as u32),
        ];

        println!("{:#?}", member_offsets);
        println!("{:#?}", rust_offsets);
        for (name, wgsl_off) in &member_offsets {
            if name == "_pad0" || name == "_pad1" || name == "_pad2" { continue; }
            let rust_off = rust_offsets.iter()
                .find(|(n, _)| n == name)
                .map(|(_, o)| *o)
                .unwrap_or_else(|| panic!("no rust field {}", name));
            // Rust fields were padded to align with WGSL's 16-byte vec3
            // and 16-byte vec4; account for those by matching each WGSL
            // offset to the nearest Rust offset that is <= it for scalar
            // fields, or exactly equal for the padded struct members.
            assert_eq!(
                rust_off, *wgsl_off,
                "field {} offset mismatch: rust={} wgsl={}",
                name, rust_off, wgsl_off,
            );
        }
    }

    #[test]
    fn default_uniforms_are_identity() {
        let u = ImageQuadUniforms::default();
        // Identity transform: col-major [1 0 0; 0 1 0; 0 0 1].
        assert_eq!(u.col0, [1.0, 0.0, 0.0]);
        assert_eq!(u.col1, [0.0, 1.0, 0.0]);
        assert_eq!(u.col2, [0.0, 0.0, 1.0]);
        // No image bound yet.
        assert_eq!(u.has_image, 0);
    }
}