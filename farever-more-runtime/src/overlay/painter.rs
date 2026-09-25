use egui::epaint::{ClippedPrimitive, ImageDelta, Primitive, TextureId};
use egui::TexturesDelta;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::copy_nonoverlapping;
use windows::core::{s, PCSTR};
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, ID3DInclude, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};

const VERTEX_SHADER: &str = r#"
cbuffer Screen : register(b0) {
    float2 screen_size;
    float2 _padding;
};

struct VertexInput {
    float2 position : POSITION;
    float2 uv : TEXCOORD0;
    float4 color : COLOR0;
};

struct PixelInput {
    float4 position : SV_POSITION;
    float2 uv : TEXCOORD0;
    float4 color : COLOR0;
};

PixelInput main(VertexInput input) {
    PixelInput output;
    output.position = float4(
        input.position.x * 2.0 / screen_size.x - 1.0,
        1.0 - input.position.y * 2.0 / screen_size.y,
        0.0,
        1.0
    );
    output.uv = input.uv;
    output.color = input.color;
    return output;
}
"#;

const PIXEL_SHADER: &str = r#"
Texture2D texture0 : register(t0);
SamplerState sampler0 : register(s0);

struct PixelInput {
    float4 position : SV_POSITION;
    float2 uv : TEXCOORD0;
    float4 color : COLOR0;
};

float4 main(PixelInput input) : SV_TARGET {
    return input.color * texture0.Sample(sampler0, input.uv);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuVertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [u8; 4],
}

#[repr(C)]
struct ScreenConstants {
    size: [f32; 2],
    padding: [f32; 2],
}

struct Texture {
    resource: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
}

pub(super) struct Painter {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    constant_buffer: ID3D11Buffer,
    blend_state: ID3D11BlendState,
    rasterizer_state: ID3D11RasterizerState,
    depth_state: ID3D11DepthStencilState,
    sampler: ID3D11SamplerState,
    vertex_buffer: Option<ID3D11Buffer>,
    vertex_capacity: usize,
    index_buffer: Option<ID3D11Buffer>,
    index_capacity: usize,
    textures: HashMap<TextureId, Texture>,
}

impl Painter {
    pub(super) fn new(device: &ID3D11Device) -> Result<Self, String> {
        let vertex_bytecode = compile_shader(VERTEX_SHADER, b"main\0", b"vs_5_0\0")?;
        let pixel_bytecode = compile_shader(PIXEL_SHADER, b"main\0", b"ps_5_0\0")?;

        let mut vertex_shader = None;
        let mut pixel_shader = None;
        unsafe {
            device.CreateVertexShader(
                &vertex_bytecode,
                None::<&ID3D11ClassLinkage>,
                Some(&mut vertex_shader),
            )
        }
        .map_err(|error| format!("CreateVertexShader failed: {error}"))?;
        unsafe {
            device.CreatePixelShader(
                &pixel_bytecode,
                None::<&ID3D11ClassLinkage>,
                Some(&mut pixel_shader),
            )
        }
        .map_err(|error| format!("CreatePixelShader failed: {error}"))?;

        let input_elements = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("POSITION"),
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                ..Default::default()
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                Format: DXGI_FORMAT_R32G32_FLOAT,
                AlignedByteOffset: 8,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                ..Default::default()
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("COLOR"),
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                AlignedByteOffset: 16,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                ..Default::default()
            },
        ];
        let mut input_layout = None;
        unsafe {
            device.CreateInputLayout(&input_elements, &vertex_bytecode, Some(&mut input_layout))
        }
        .map_err(|error| format!("CreateInputLayout failed: {error}"))?;

        let constant_buffer = create_buffer(
            device,
            size_of::<ScreenConstants>(),
            D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            D3D11_USAGE_DEFAULT,
            0,
        )?;
        let blend_state = create_blend_state(device)?;
        let rasterizer_state = create_rasterizer_state(device)?;
        let depth_state = create_depth_state(device)?;
        let sampler = create_sampler(device)?;

        Ok(Self {
            vertex_shader: vertex_shader
                .ok_or_else(|| "CreateVertexShader returned null".to_owned())?,
            pixel_shader: pixel_shader
                .ok_or_else(|| "CreatePixelShader returned null".to_owned())?,
            input_layout: input_layout
                .ok_or_else(|| "CreateInputLayout returned null".to_owned())?,
            constant_buffer,
            blend_state,
            rasterizer_state,
            depth_state,
            sampler,
            vertex_buffer: None,
            vertex_capacity: 0,
            index_buffer: None,
            index_capacity: 0,
            textures: HashMap::new(),
        })
    }

    pub(super) fn paint(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        egui: &egui::Context,
        output: egui::FullOutput,
        target_size: [u32; 2],
    ) -> Result<(), String> {
        let egui::FullOutput {
            textures_delta,
            shapes,
            pixels_per_point,
            ..
        } = output;
        self.update_textures(device, context, &textures_delta)?;
        let primitives = egui.tessellate(shapes, pixels_per_point);
        let meshes = primitives
            .iter()
            .filter_map(|primitive| match &primitive.primitive {
                Primitive::Mesh(mesh) => Some((primitive, mesh)),
                Primitive::Callback(_) => None,
            })
            .collect::<Vec<_>>();
        let vertex_count = meshes.iter().map(|(_, mesh)| mesh.vertices.len()).sum();
        let index_count = meshes.iter().map(|(_, mesh)| mesh.indices.len()).sum();
        if vertex_count == 0 || index_count == 0 {
            self.free_textures(textures_delta);
            return Ok(());
        }

        self.ensure_buffers(device, vertex_count, index_count)?;
        self.upload_meshes(context, &meshes)?;
        let constants = ScreenConstants {
            // Mesh vertices remain in egui points. Projecting them against the
            // logical screen size lets the physical D3D viewport apply the
            // pixels-per-point scale to positions, geometry, and text alike.
            size: logical_screen_size(target_size, pixels_per_point),
            padding: [0.0; 2],
        };
        unsafe {
            context.UpdateSubresource(
                &self.constant_buffer,
                0,
                None,
                (&constants as *const ScreenConstants).cast(),
                0,
                0,
            );
            self.bind_pipeline(context);
        }

        let mut vertex_offset = 0_i32;
        let mut index_offset = 0_u32;
        for (clipped, mesh) in meshes {
            let clip = clip_rect(clipped, pixels_per_point, target_size);
            if clip.right > clip.left && clip.bottom > clip.top {
                if let Some(texture) = self.textures.get(&mesh.texture_id) {
                    unsafe {
                        context.RSSetScissorRects(Some(&[clip]));
                        context.PSSetShaderResources(0, Some(&[Some(texture.view.clone())]));
                        context.DrawIndexed(mesh.indices.len() as u32, index_offset, vertex_offset);
                    }
                }
            }
            vertex_offset += mesh.vertices.len() as i32;
            index_offset += mesh.indices.len() as u32;
        }
        unsafe { context.PSSetShaderResources(0, Some(&[None])) };
        self.free_textures(textures_delta);
        Ok(())
    }

    fn ensure_buffers(
        &mut self,
        device: &ID3D11Device,
        vertex_count: usize,
        index_count: usize,
    ) -> Result<(), String> {
        if vertex_count > self.vertex_capacity {
            self.vertex_capacity = vertex_count.next_power_of_two();
            self.vertex_buffer = Some(create_buffer(
                device,
                self.vertex_capacity * size_of::<GpuVertex>(),
                D3D11_BIND_VERTEX_BUFFER.0 as u32,
                D3D11_USAGE_DYNAMIC,
                D3D11_CPU_ACCESS_WRITE.0 as u32,
            )?);
        }
        if index_count > self.index_capacity {
            self.index_capacity = index_count.next_power_of_two();
            self.index_buffer = Some(create_buffer(
                device,
                self.index_capacity * size_of::<u32>(),
                D3D11_BIND_INDEX_BUFFER.0 as u32,
                D3D11_USAGE_DYNAMIC,
                D3D11_CPU_ACCESS_WRITE.0 as u32,
            )?);
        }
        Ok(())
    }

    fn upload_meshes(
        &self,
        context: &ID3D11DeviceContext,
        meshes: &[(&ClippedPrimitive, &egui::Mesh)],
    ) -> Result<(), String> {
        let vertices = self
            .vertex_buffer
            .as_ref()
            .ok_or_else(|| "vertex buffer is unavailable".to_owned())?;
        let indices = self
            .index_buffer
            .as_ref()
            .ok_or_else(|| "index buffer is unavailable".to_owned())?;
        let mut vertex_map: D3D11_MAPPED_SUBRESOURCE = unsafe { zeroed() };
        let mut index_map: D3D11_MAPPED_SUBRESOURCE = unsafe { zeroed() };
        unsafe {
            context.Map(
                vertices,
                0,
                D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&mut vertex_map),
            )
        }
        .map_err(|error| format!("mapping egui vertex buffer failed: {error}"))?;
        unsafe { context.Map(indices, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut index_map)) }
            .map_err(|error| {
                unsafe { context.Unmap(vertices, 0) };
                format!("mapping egui index buffer failed: {error}")
            })?;

        let mut vertex_destination = vertex_map.pData.cast::<GpuVertex>();
        let mut index_destination = index_map.pData.cast::<u32>();
        for (_, mesh) in meshes {
            for vertex in &mesh.vertices {
                unsafe {
                    vertex_destination.write(GpuVertex {
                        position: [vertex.pos.x, vertex.pos.y],
                        uv: [vertex.uv.x, vertex.uv.y],
                        color: vertex.color.to_array(),
                    });
                    vertex_destination = vertex_destination.add(1);
                }
            }
            unsafe {
                copy_nonoverlapping(mesh.indices.as_ptr(), index_destination, mesh.indices.len());
                index_destination = index_destination.add(mesh.indices.len());
            }
        }
        unsafe {
            context.Unmap(indices, 0);
            context.Unmap(vertices, 0);
        }
        Ok(())
    }

    unsafe fn bind_pipeline(&self, context: &ID3D11DeviceContext) {
        let vertex_buffer = [self.vertex_buffer.clone()];
        let stride = [size_of::<GpuVertex>() as u32];
        let offset = [0_u32];
        unsafe {
            context.IASetInputLayout(&self.input_layout);
            context.IASetVertexBuffers(
                0,
                1,
                Some(vertex_buffer.as_ptr()),
                Some(stride.as_ptr()),
                Some(offset.as_ptr()),
            );
            context.IASetIndexBuffer(self.index_buffer.as_ref(), DXGI_FORMAT_R32_UINT, 0);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex_shader, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.constant_buffer.clone())]));
            context.PSSetShader(&self.pixel_shader, None);
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.RSSetState(&self.rasterizer_state);
            context.OMSetBlendState(&self.blend_state, Some(&[0.0; 4]), u32::MAX);
            context.OMSetDepthStencilState(&self.depth_state, 0);
        }
    }

    fn update_textures(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        delta: &TexturesDelta,
    ) -> Result<(), String> {
        for (id, change) in &delta.set {
            self.update_texture(device, context, *id, change)?;
        }
        Ok(())
    }

    fn update_texture(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        id: TextureId,
        delta: &ImageDelta,
    ) -> Result<(), String> {
        let egui::ImageData::Color(image) = &delta.image;
        let pixels = image
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_array())
            .collect::<Vec<_>>();
        let width = image.size[0] as u32;
        let height = image.size[1] as u32;
        if let Some([x, y]) = delta.pos {
            let texture = self
                .textures
                .get(&id)
                .ok_or_else(|| format!("partial update for unknown egui texture {id:?}"))?;
            let region = D3D11_BOX {
                left: x as u32,
                top: y as u32,
                front: 0,
                right: x as u32 + width,
                bottom: y as u32 + height,
                back: 1,
            };
            unsafe {
                context.UpdateSubresource(
                    &texture.resource,
                    0,
                    Some(&region),
                    pixels.as_ptr().cast(),
                    width * 4,
                    0,
                )
            };
            return Ok(());
        }

        let description = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: width * 4,
            ..Default::default()
        };
        let mut resource = None;
        unsafe { device.CreateTexture2D(&description, Some(&initial), Some(&mut resource)) }
            .map_err(|error| format!("creating egui texture failed: {error}"))?;
        let resource = resource.ok_or_else(|| "CreateTexture2D returned null".to_owned())?;
        let mut view = None;
        unsafe { device.CreateShaderResourceView(&resource, None, Some(&mut view)) }
            .map_err(|error| format!("creating egui texture view failed: {error}"))?;
        self.textures.insert(
            id,
            Texture {
                resource,
                view: view.ok_or_else(|| "CreateShaderResourceView returned null".to_owned())?,
            },
        );
        Ok(())
    }

    fn free_textures(&mut self, delta: TexturesDelta) {
        for id in delta.free {
            self.textures.remove(&id);
        }
    }
}

fn create_buffer(
    device: &ID3D11Device,
    byte_width: usize,
    bind_flags: u32,
    usage: D3D11_USAGE,
    cpu_access: u32,
) -> Result<ID3D11Buffer, String> {
    let byte_width = u32::try_from(byte_width)
        .map_err(|_| "egui GPU buffer exceeds D3D11's 32-bit size limit".to_owned())?;
    let description = D3D11_BUFFER_DESC {
        ByteWidth: byte_width,
        Usage: usage,
        BindFlags: bind_flags,
        CPUAccessFlags: cpu_access,
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&description, None, Some(&mut buffer)) }
        .map_err(|error| format!("CreateBuffer failed: {error}"))?;
    buffer.ok_or_else(|| "CreateBuffer returned null".to_owned())
}

fn create_blend_state(device: &ID3D11Device) -> Result<ID3D11BlendState, String> {
    let target = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: BOOL(1),
        // egui vertex and texture colors are already premultiplied. Keeping
        // them premultiplied through the render target is also what the
        // DirectComposition swap chain expects.
        SrcBlend: D3D11_BLEND_ONE,
        DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ONE,
        DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut description = D3D11_BLEND_DESC::default();
    description.RenderTarget[0] = target;
    let mut state = None;
    unsafe { device.CreateBlendState(&description, Some(&mut state)) }
        .map_err(|error| format!("CreateBlendState failed: {error}"))?;
    state.ok_or_else(|| "CreateBlendState returned null".to_owned())
}

fn create_rasterizer_state(device: &ID3D11Device) -> Result<ID3D11RasterizerState, String> {
    let description = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        DepthClipEnable: BOOL(1),
        ScissorEnable: BOOL(1),
        ..Default::default()
    };
    let mut state = None;
    unsafe { device.CreateRasterizerState(&description, Some(&mut state)) }
        .map_err(|error| format!("CreateRasterizerState failed: {error}"))?;
    state.ok_or_else(|| "CreateRasterizerState returned null".to_owned())
}

fn create_depth_state(device: &ID3D11Device) -> Result<ID3D11DepthStencilState, String> {
    let description = D3D11_DEPTH_STENCIL_DESC {
        DepthEnable: BOOL(0),
        DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ZERO,
        DepthFunc: D3D11_COMPARISON_ALWAYS,
        StencilEnable: BOOL(0),
        ..Default::default()
    };
    let mut state = None;
    unsafe { device.CreateDepthStencilState(&description, Some(&mut state)) }
        .map_err(|error| format!("CreateDepthStencilState failed: {error}"))?;
    state.ok_or_else(|| "CreateDepthStencilState returned null".to_owned())
}

fn create_sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState, String> {
    let description = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        ComparisonFunc: D3D11_COMPARISON_ALWAYS,
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    let mut state = None;
    unsafe { device.CreateSamplerState(&description, Some(&mut state)) }
        .map_err(|error| format!("CreateSamplerState failed: {error}"))?;
    state.ok_or_else(|| "CreateSamplerState returned null".to_owned())
}

fn compile_shader(source: &str, entry: &[u8], target: &[u8]) -> Result<Vec<u8>, String> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR::null(),
            None,
            None::<&ID3DInclude>,
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(error) = result {
        let diagnostic = errors
            .as_ref()
            .map(blob_bytes)
            .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
            .unwrap_or_default();
        return Err(format!("D3DCompile failed: {error}; {diagnostic}"));
    }
    code.as_ref()
        .map(blob_bytes)
        .map(ToOwned::to_owned)
        .ok_or_else(|| "D3DCompile returned no shader bytecode".to_owned())
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

fn clip_rect(clipped: &ClippedPrimitive, pixels_per_point: f32, target_size: [u32; 2]) -> RECT {
    RECT {
        left: (clipped.clip_rect.min.x * pixels_per_point)
            .floor()
            .clamp(0.0, target_size[0] as f32) as i32,
        top: (clipped.clip_rect.min.y * pixels_per_point)
            .floor()
            .clamp(0.0, target_size[1] as f32) as i32,
        right: (clipped.clip_rect.max.x * pixels_per_point)
            .ceil()
            .clamp(0.0, target_size[0] as f32) as i32,
        bottom: (clipped.clip_rect.max.y * pixels_per_point)
            .ceil()
            .clamp(0.0, target_size[1] as f32) as i32,
    }
}

fn logical_screen_size(target_size: [u32; 2], pixels_per_point: f32) -> [f32; 2] {
    [
        target_size[0] as f32 / pixels_per_point,
        target_size[1] as f32 / pixels_per_point,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_tracks_pixels_per_point_after_resize() {
        assert_eq!(logical_screen_size([1920, 1080], 1.0), [1920.0, 1080.0]);
        assert_eq!(logical_screen_size([3840, 2160], 2.0), [1920.0, 1080.0]);
    }
}
