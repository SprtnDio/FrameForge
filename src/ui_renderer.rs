use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::ptr::copy_nonoverlapping;

use anyhow::{bail, Context, Result};
use egui::epaint::{ImageDelta, Primitive, Vertex};
use egui::{ClippedPrimitive, TextureId, TexturesDelta};
use windows::core::PCSTR;
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12SerializeRootSignature, ID3D12DescriptorHeap, ID3D12Device, ID3D12GraphicsCommandList,
    ID3D12PipelineState, ID3D12Resource, ID3D12RootSignature, D3D12_BLEND_DESC,
    D3D12_BLEND_INV_SRC_ALPHA, D3D12_BLEND_ONE, D3D12_BLEND_OP_ADD, D3D12_COLOR_WRITE_ENABLE_ALL,
    D3D12_COMPARISON_FUNC_ALWAYS, D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_CULL_MODE_NONE,
    D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING, D3D12_DEPTH_STENCIL_DESC,
    D3D12_DESCRIPTOR_HEAP_DESC, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, D3D12_DESCRIPTOR_RANGE,
    D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND, D3D12_DESCRIPTOR_RANGE_TYPE_SRV, D3D12_FILL_MODE_SOLID,
    D3D12_FILTER_MIN_MAG_MIP_LINEAR, D3D12_GPU_DESCRIPTOR_HANDLE,
    D3D12_GRAPHICS_PIPELINE_STATE_DESC, D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_TYPE_UPLOAD, D3D12_INDEX_BUFFER_VIEW,
    D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA, D3D12_INPUT_ELEMENT_DESC, D3D12_INPUT_LAYOUT_DESC,
    D3D12_LOGIC_OP_NOOP, D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE, D3D12_RASTERIZER_DESC, D3D12_RANGE,
    D3D12_RENDER_TARGET_BLEND_DESC, D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_0,
    D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, D3D12_RESOURCE_BARRIER_FLAG_NONE,
    D3D12_RESOURCE_BARRIER_TYPE_TRANSITION, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_BUFFER,
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATES,
    D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_GENERIC_READ,
    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_TRANSITION_BARRIER,
    D3D12_ROOT_CONSTANTS, D3D12_ROOT_DESCRIPTOR_TABLE, D3D12_ROOT_PARAMETER,
    D3D12_ROOT_PARAMETER_0, D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
    D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE, D3D12_ROOT_SIGNATURE_DESC,
    D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT, D3D12_SHADER_BYTECODE,
    D3D12_SHADER_RESOURCE_VIEW_DESC, D3D12_SHADER_RESOURCE_VIEW_DESC_0,
    D3D12_SHADER_VISIBILITY_PIXEL, D3D12_SHADER_VISIBILITY_VERTEX, D3D12_SRV_DIMENSION_TEXTURE2D,
    D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK, D3D12_STATIC_SAMPLER_DESC,
    D3D12_SUBRESOURCE_FOOTPRINT, D3D12_TEX2D_SRV, D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
    D3D12_TEXTURE_COPY_LOCATION, D3D12_TEXTURE_COPY_LOCATION_0,
    D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
    D3D12_TEXTURE_DATA_PITCH_ALIGNMENT, D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
    D3D12_TEXTURE_LAYOUT_UNKNOWN, D3D12_VERTEX_BUFFER_VIEW, D3D12_VIEWPORT,
    D3D_ROOT_SIGNATURE_VERSION_1,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};

const MAX_UI_TEXTURES: u32 = 64;
const INITIAL_VERTEX_CAPACITY: usize = 8192;
const INITIAL_INDEX_CAPACITY: usize = 24576;

const EGUI_HLSL_SOURCE: &[u8] = br#"
cbuffer ScreenConstants : register(b0) {
    float2 screen_size_in_points;
    float2 _padding;
};

Texture2D ui_texture : register(t0);
SamplerState ui_sampler : register(s0);

struct VSInput {
    float2 pos   : POSITION;
    float2 uv    : TEXCOORD0;
    float4 color : COLOR0;
};

struct PSInput {
    float4 pos   : SV_POSITION;
    float2 uv    : TEXCOORD0;
    float4 color : COLOR0;
};

PSInput vs_main(VSInput input) {
    PSInput output;
    output.pos = float4(
        2.0 * input.pos.x / screen_size_in_points.x - 1.0,
        1.0 - 2.0 * input.pos.y / screen_size_in_points.y,
        0.0,
        1.0
    );
    output.uv = input.uv;
    output.color = input.color;
    return output;
}

float4 ps_main(PSInput input) : SV_TARGET {
    return input.color * ui_texture.Sample(ui_sampler, input.uv);
}
"#;

struct GpuTexture {
    resource: ID3D12Resource,
    srv_slot: u32,
    width: u32,
    height: u32,
}

struct DrawBatch {
    texture_id: TextureId,
    clip_rect: RECT,
    index_count: u32,
    first_index: u32,
    base_vertex: i32,
}

/// Kompakter, maßgeschneiderter DirectX-12-Renderer für `egui` 0.29 (kompatibel mit `windows` 0.58).
///
/// Verwaltet eigene RootSignature, PipelineState, einen Shader-sichtbaren SRV-Descriptor-Heap,
/// dynamisch wachsende Upload-Buffer für Vertices/Indices sowie staging-sichere Textur-Uploads.
pub struct UiRenderer {
    pipeline_state: ID3D12PipelineState,
    root_signature: ID3D12RootSignature,
    srv_heap: ID3D12DescriptorHeap,
    srv_descriptor_size: usize,
    free_srv_slots: Vec<u32>,
    textures: HashMap<TextureId, GpuTexture>,
    vertex_buffer: ID3D12Resource,
    vertex_capacity: usize,
    index_buffer: ID3D12Resource,
    index_capacity: usize,
    /// Hält temporäre Upload-Buffer und ersetzte Ressourcen am Leben, bis der Frame per Fence
    /// auf der GPU abgeschlossen ist.
    staging_resources: Vec<ID3D12Resource>,
    pending_texture_frees: Vec<TextureId>,
}

impl UiRenderer {
    pub fn new(device: &ID3D12Device) -> Result<Self> {
        let root_signature = Self::create_root_signature(device)?;
        let pipeline_state = Self::create_pipeline_state(device, &root_signature)?;

        let srv_heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: MAX_UI_TEXTURES,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        };
        let srv_heap: ID3D12DescriptorHeap = unsafe {
            device
                .CreateDescriptorHeap(&srv_heap_desc)
                .context("Erstellung des UI-SRV-Descriptor-Heaps fehlgeschlagen")?
        };
        let srv_descriptor_size = unsafe {
            device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
        } as usize;

        let free_srv_slots: Vec<u32> = (0..MAX_UI_TEXTURES).rev().collect();

        let vertex_buffer = Self::create_upload_buffer(
            device,
            (INITIAL_VERTEX_CAPACITY * size_of::<Vertex>()) as u64,
        )?;
        let index_buffer =
            Self::create_upload_buffer(device, (INITIAL_INDEX_CAPACITY * size_of::<u32>()) as u64)?;

        Ok(Self {
            pipeline_state,
            root_signature,
            srv_heap,
            srv_descriptor_size,
            free_srv_slots,
            textures: HashMap::new(),
            vertex_buffer,
            vertex_capacity: INITIAL_VERTEX_CAPACITY,
            index_buffer,
            index_capacity: INITIAL_INDEX_CAPACITY,
            staging_resources: Vec::new(),
            pending_texture_frees: Vec::new(),
        })
    }

    /// Zeichnet sämtliche `egui`-Texture-Deltas und Clipped-Meshes in die geöffnete Command-List.
    ///
    /// Voraussetzung: Der aktuelle Backbuffer befindet sich bereits im Zustand
    /// `D3D12_RESOURCE_STATE_RENDER_TARGET` und ist per `OMSetRenderTargets` gebunden.
    pub fn render(
        &mut self,
        device: &ID3D12Device,
        cmd_list: &ID3D12GraphicsCommandList,
        textures_delta: &TexturesDelta,
        primitives: &[ClippedPrimitive],
        screen_width_px: u32,
        screen_height_px: u32,
        pixels_per_point: f32,
    ) -> Result<()> {
        // Da Dx12Context vor jedem neuen Frame auf den Fence gewartet hat, können alle
        // Upload-Buffer und freigegebenen Texturen des vorherigen Frames sicher gelöscht werden.
        self.staging_resources.clear();
        let previous_frees = std::mem::take(&mut self.pending_texture_frees);
        for tex_id in previous_frees {
            if let Some(removed) = self.textures.remove(&tex_id) {
                self.free_srv_slots.push(removed.srv_slot);
            }
        }

        // 1. Neue oder aktualisierte Texturen (Font-Atlas & Icons) hochladen.
        for (tex_id, delta) in &textures_delta.set {
            self.update_texture(device, cmd_list, *tex_id, delta)?;
        }
        self.pending_texture_frees
            .extend(textures_delta.free.iter().copied());

        if screen_width_px == 0 || screen_height_px == 0 || primitives.is_empty() {
            return Ok(());
        }

        let ppp = if pixels_per_point > 0.0 {
            pixels_per_point
        } else {
            1.0
        };

        // 2. Gesamtanzahl an Vertices & Indices ermitteln.
        let mut total_vertices = 0usize;
        let mut total_indices = 0usize;
        for clipped in primitives {
            if let Primitive::Mesh(mesh) = &clipped.primitive {
                total_vertices += mesh.vertices.len();
                total_indices += mesh.indices.len();
            }
        }

        if total_vertices == 0 || total_indices == 0 {
            return Ok(());
        }

        self.ensure_buffer_capacities(device, total_vertices, total_indices)?;

        // 3. Vertices und Indices gesammelt in die Upload-Buffer kopieren und Draw-Batches aufbauen.
        let mut batches: Vec<DrawBatch> = Vec::with_capacity(primitives.len());
        unsafe {
            let empty_read_range = D3D12_RANGE { Begin: 0, End: 0 };

            let mut vb_ptr: *mut c_void = std::ptr::null_mut();
            self.vertex_buffer
                .Map(0, Some(&empty_read_range), Some(&mut vb_ptr))
                .context("Vertex-Buffer Map fehlgeschlagen")?;
            let vb_dst = vb_ptr as *mut Vertex;

            let mut ib_ptr: *mut c_void = std::ptr::null_mut();
            self.index_buffer
                .Map(0, Some(&empty_read_range), Some(&mut ib_ptr))
                .context("Index-Buffer Map fehlgeschlagen")?;
            let ib_dst = ib_ptr as *mut u32;

            let mut vertex_offset = 0usize;
            let mut index_offset = 0usize;

            for clipped in primitives {
                let Primitive::Mesh(mesh) = &clipped.primitive else {
                    continue;
                };
                if mesh.vertices.is_empty() || mesh.indices.is_empty() {
                    continue;
                }

                let min_x = (clipped.clip_rect.min.x * ppp)
                    .floor()
                    .clamp(0.0, screen_width_px as f32) as i32;
                let min_y = (clipped.clip_rect.min.y * ppp)
                    .floor()
                    .clamp(0.0, screen_height_px as f32) as i32;
                let max_x = (clipped.clip_rect.max.x * ppp)
                    .ceil()
                    .clamp(0.0, screen_width_px as f32) as i32;
                let max_y = (clipped.clip_rect.max.y * ppp)
                    .ceil()
                    .clamp(0.0, screen_height_px as f32) as i32;

                if max_x <= min_x || max_y <= min_y {
                    continue;
                }

                copy_nonoverlapping(
                    mesh.vertices.as_ptr(),
                    vb_dst.add(vertex_offset),
                    mesh.vertices.len(),
                );
                copy_nonoverlapping(
                    mesh.indices.as_ptr(),
                    ib_dst.add(index_offset),
                    mesh.indices.len(),
                );

                batches.push(DrawBatch {
                    texture_id: mesh.texture_id,
                    clip_rect: RECT {
                        left: min_x,
                        top: min_y,
                        right: max_x,
                        bottom: max_y,
                    },
                    index_count: mesh.indices.len() as u32,
                    first_index: index_offset as u32,
                    base_vertex: vertex_offset as i32,
                });

                vertex_offset += mesh.vertices.len();
                index_offset += mesh.indices.len();
            }

            self.index_buffer.Unmap(0, None);
            self.vertex_buffer.Unmap(0, None);
        }

        if batches.is_empty() {
            return Ok(());
        }

        // 4. Pipeline, RootSignature, Viewport und Buffer setzen und Batches zeichnen.
        let screen_size_in_points: [f32; 4] = [
            (screen_width_px as f32) / ppp,
            (screen_height_px as f32) / ppp,
            0.0,
            0.0,
        ];

        let viewport = D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: screen_width_px as f32,
            Height: screen_height_px as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };

        unsafe {
            let vb_view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: self.vertex_buffer.GetGPUVirtualAddress(),
                SizeInBytes: (self.vertex_capacity * size_of::<Vertex>()) as u32,
                StrideInBytes: size_of::<Vertex>() as u32,
            };
            let ib_view = D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: self.index_buffer.GetGPUVirtualAddress(),
                SizeInBytes: (self.index_capacity * size_of::<u32>()) as u32,
                Format: DXGI_FORMAT_R32_UINT,
            };

            cmd_list.SetPipelineState(&self.pipeline_state);
            cmd_list.SetGraphicsRootSignature(&self.root_signature);
            cmd_list.SetDescriptorHeaps(&[Some(self.srv_heap.clone())]);
            cmd_list.RSSetViewports(&[viewport]);
            cmd_list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            cmd_list.IASetVertexBuffers(0, Some(&[vb_view]));
            cmd_list.IASetIndexBuffer(Some(&ib_view as *const D3D12_INDEX_BUFFER_VIEW));
            cmd_list.SetGraphicsRoot32BitConstants(
                0,
                4,
                screen_size_in_points.as_ptr() as *const c_void,
                0,
            );

            for batch in &batches {
                let Some(gpu_tex) = self.textures.get(&batch.texture_id) else {
                    continue;
                };
                let gpu_handle = self.srv_gpu_handle(gpu_tex.srv_slot);
                cmd_list.RSSetScissorRects(&[batch.clip_rect]);
                cmd_list.SetGraphicsRootDescriptorTable(1, gpu_handle);
                cmd_list.DrawIndexedInstanced(
                    batch.index_count,
                    1,
                    batch.first_index,
                    batch.base_vertex,
                    0,
                );
            }
        }

        Ok(())
    }

    fn ensure_buffer_capacities(
        &mut self,
        device: &ID3D12Device,
        required_vertices: usize,
        required_indices: usize,
    ) -> Result<()> {
        if required_vertices > self.vertex_capacity {
            let new_cap = required_vertices.next_power_of_two().max(INITIAL_VERTEX_CAPACITY);
            let new_vb =
                Self::create_upload_buffer(device, (new_cap * size_of::<Vertex>()) as u64)?;
            let old_vb = std::mem::replace(&mut self.vertex_buffer, new_vb);
            self.staging_resources.push(old_vb);
            self.vertex_capacity = new_cap;
        }

        if required_indices > self.index_capacity {
            let new_cap = required_indices.next_power_of_two().max(INITIAL_INDEX_CAPACITY);
            let new_ib = Self::create_upload_buffer(device, (new_cap * size_of::<u32>()) as u64)?;
            let old_ib = std::mem::replace(&mut self.index_buffer, new_ib);
            self.staging_resources.push(old_ib);
            self.index_capacity = new_cap;
        }

        Ok(())
    }

    fn update_texture(
        &mut self,
        device: &ID3D12Device,
        cmd_list: &ID3D12GraphicsCommandList,
        tex_id: TextureId,
        delta: &ImageDelta,
    ) -> Result<()> {
        let patch_width = delta.image.width() as u32;
        let patch_height = delta.image.height() as u32;
        if patch_width == 0 || patch_height == 0 {
            return Ok(());
        }

        let rgba_bytes: Vec<u8> = match &delta.image {
            egui::ImageData::Color(color_img) => color_img
                .pixels
                .iter()
                .flat_map(|c| c.to_array())
                .collect(),
            egui::ImageData::Font(font_img) => font_img
                .srgba_pixels(None)
                .flat_map(|c| c.to_array())
                .collect(),
        };

        let (dst_x, dst_y, is_partial) = match delta.pos {
            Some([x, y]) => (x as u32, y as u32, true),
            None => (0u32, 0u32, false),
        };

        // Falls es ein Voll-Update ist oder die Textur noch nicht existiert: Textur (neu) anlegen.
        if !is_partial || !self.textures.contains_key(&tex_id) {
            let srv_slot = if let Some(existing) = self.textures.remove(&tex_id) {
                let slot = existing.srv_slot;
                self.staging_resources.push(existing.resource);
                slot
            } else {
                self.free_srv_slots
                    .pop()
                    .context("Maximale Anzahl an UI-Texturen im SRV-Heap überschritten")?
            };

            let default_heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let tex_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Alignment: 0,
                Width: patch_width as u64,
                Height: patch_height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_NONE,
            };

            let mut tex_opt: Option<ID3D12Resource> = None;
            unsafe {
                device
                    .CreateCommittedResource(
                        &default_heap,
                        D3D12_HEAP_FLAG_NONE,
                        &tex_desc,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut tex_opt,
                    )
                    .context("Erstellung der UI-Textur (DEFAULT Heap) fehlgeschlagen")?;
            }
            let texture = tex_opt.context("CreateCommittedResource lieferte keine UI-Textur")?;

            let srv_desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                        PlaneSlice: 0,
                        ResourceMinLODClamp: 0.0,
                    },
                },
            };

            let cpu_handle = self.srv_cpu_handle(srv_slot);
            unsafe {
                device.CreateShaderResourceView(
                    &texture,
                    Some(&srv_desc as *const D3D12_SHADER_RESOURCE_VIEW_DESC),
                    cpu_handle,
                );
            }

            self.upload_region_to_texture(
                device,
                cmd_list,
                &texture,
                0,
                0,
                patch_width,
                patch_height,
                &rgba_bytes,
                false,
            )?;

            self.textures.insert(
                tex_id,
                GpuTexture {
                    resource: texture,
                    srv_slot,
                    width: patch_width,
                    height: patch_height,
                },
            );
        } else {
            // Partielles Sub-Update eines bestehenden Font-Atlas / Bildbereichs.
            let Some(existing) = self.textures.get(&tex_id) else {
                return Ok(());
            };
            if dst_x + patch_width > existing.width || dst_y + patch_height > existing.height {
                return Ok(());
            }
            let tex_clone = existing.resource.clone();
            self.upload_region_to_texture(
                device,
                cmd_list,
                &tex_clone,
                dst_x,
                dst_y,
                patch_width,
                patch_height,
                &rgba_bytes,
                true,
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_region_to_texture(
        &mut self,
        device: &ID3D12Device,
        cmd_list: &ID3D12GraphicsCommandList,
        texture: &ID3D12Resource,
        dst_x: u32,
        dst_y: u32,
        width: u32,
        height: u32,
        rgba_bytes: &[u8],
        transition_from_shader_read: bool,
    ) -> Result<()> {
        let unaligned_row_bytes = (width as usize) * 4;
        let pitch_align = D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as usize;
        let aligned_row_pitch = (unaligned_row_bytes + pitch_align - 1) & !(pitch_align - 1);
        let upload_size = aligned_row_pitch * (height as usize);

        let upload_buffer = Self::create_upload_buffer(device, upload_size as u64)?;

        unsafe {
            let empty_read_range = D3D12_RANGE { Begin: 0, End: 0 };
            let mut mapped_ptr: *mut c_void = std::ptr::null_mut();
            upload_buffer
                .Map(0, Some(&empty_read_range), Some(&mut mapped_ptr))
                .context("Map auf Textur-Upload-Buffer fehlgeschlagen")?;
            let dst_base = mapped_ptr as *mut u8;

            for row in 0..(height as usize) {
                let src_row = &rgba_bytes[row * unaligned_row_bytes..(row + 1) * unaligned_row_bytes];
                copy_nonoverlapping(
                    src_row.as_ptr(),
                    dst_base.add(row * aligned_row_pitch),
                    unaligned_row_bytes,
                );
            }

            upload_buffer.Unmap(0, None);

            if transition_from_shader_read {
                let to_copy = Self::transition_barrier(
                    texture,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                );
                cmd_list.ResourceBarrier(&[to_copy]);
            }

            let src_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&upload_buffer),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            Width: width,
                            Height: height,
                            Depth: 1,
                            RowPitch: aligned_row_pitch as u32,
                        },
                    },
                },
            };

            let dst_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };

            cmd_list.CopyTextureRegion(&dst_loc, dst_x, dst_y, 0, &src_loc, None);

            let to_shader_read = Self::transition_barrier(
                texture,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            );
            cmd_list.ResourceBarrier(&[to_shader_read]);
        }

        // Upload-Buffer bis zum Abschluss des aktuellen Frames am Leben halten!
        self.staging_resources.push(upload_buffer);
        Ok(())
    }

    fn create_upload_buffer(device: &ID3D12Device, byte_size: u64) -> Result<ID3D12Resource> {
        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: byte_size.max(256),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };

        let mut buffer_opt: Option<ID3D12Resource> = None;
        unsafe {
            device
                .CreateCommittedResource(
                    &heap_props,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer_opt,
                )
                .context("CreateCommittedResource (UPLOAD Buffer) fehlgeschlagen")?;
        }
        buffer_opt.context("CreateCommittedResource lieferte keinen Upload-Buffer")
    }

    fn create_root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature> {
        let srv_range = [D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
        }];

        let root_params = [
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 0,
                        RegisterSpace: 0,
                        Num32BitValues: 4,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_VERTEX,
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: srv_range.len() as u32,
                        pDescriptorRanges: srv_range.as_ptr(),
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
            },
        ];

        let static_sampler = [D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 0,
            ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            BorderColor: D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
            ShaderRegister: 0,
            RegisterSpace: 0,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        }];

        let root_desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: root_params.len() as u32,
            pParameters: root_params.as_ptr(),
            NumStaticSamplers: static_sampler.len() as u32,
            pStaticSamplers: static_sampler.as_ptr(),
            Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
        };

        let mut sig_blob: Option<ID3DBlob> = None;
        let mut err_blob: Option<ID3DBlob> = None;
        let serialize_res = unsafe {
            D3D12SerializeRootSignature(
                &root_desc,
                D3D_ROOT_SIGNATURE_VERSION_1,
                &mut sig_blob,
                Some(&mut err_blob),
            )
        };

        if let Err(err) = serialize_res {
            let details = Self::blob_to_string(err_blob.as_ref());
            bail!("D3D12SerializeRootSignature fehlgeschlagen ({err}): {details}");
        }

        let blob = sig_blob.context("Kein RootSignature-Blob erhalten")?;
        let bytes = unsafe {
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
        };

        unsafe {
            device
                .CreateRootSignature(0, bytes)
                .context("ID3D12Device::CreateRootSignature fehlgeschlagen")
        }
    }

    fn create_pipeline_state(
        device: &ID3D12Device,
        root_signature: &ID3D12RootSignature,
    ) -> Result<ID3D12PipelineState> {
        let vs_blob = Self::compile_hlsl(b"vs_main\0", b"vs_5_0\0")?;
        let ps_blob = Self::compile_hlsl(b"ps_main\0", b"ps_5_0\0")?;

        let input_elements = [
            D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(b"POSITION\0".as_ptr()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(b"TEXCOORD\0".as_ptr()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 8,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(b"COLOR\0".as_ptr()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                InputSlot: 0,
                AlignedByteOffset: 16,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];

        let mut blend_desc = D3D12_BLEND_DESC {
            AlphaToCoverageEnable: BOOL(0),
            IndependentBlendEnable: BOOL(0),
            ..Default::default()
        };
        // egui liefert premultiplied sRGBA-Farben -> SrcBlend = ONE, DestBlend = INV_SRC_ALPHA.
        blend_desc.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: BOOL(1),
            LogicOpEnable: BOOL(0),
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };

        let rasterizer_desc = D3D12_RASTERIZER_DESC {
            FillMode: D3D12_FILL_MODE_SOLID,
            CullMode: D3D12_CULL_MODE_NONE,
            FrontCounterClockwise: BOOL(0),
            DepthClipEnable: BOOL(1),
            ..Default::default()
        };

        let depth_stencil_desc = D3D12_DEPTH_STENCIL_DESC {
            DepthEnable: BOOL(0),
            StencilEnable: BOOL(0),
            ..Default::default()
        };

        let mut pso_desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: unsafe { std::mem::transmute_copy(root_signature) },
            VS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: unsafe { vs_blob.GetBufferPointer() },
                BytecodeLength: unsafe { vs_blob.GetBufferSize() },
            },
            PS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: unsafe { ps_blob.GetBufferPointer() },
                BytecodeLength: unsafe { ps_blob.GetBufferSize() },
            },
            BlendState: blend_desc,
            SampleMask: u32::MAX,
            RasterizerState: rasterizer_desc,
            DepthStencilState: depth_stencil_desc,
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: input_elements.as_ptr(),
                NumElements: input_elements.len() as u32,
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        pso_desc.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;

        unsafe {
            device
                .CreateGraphicsPipelineState(&pso_desc)
                .context("ID3D12Device::CreateGraphicsPipelineState für egui fehlgeschlagen")
        }
    }

    fn compile_hlsl(entry_nul: &[u8], target_nul: &[u8]) -> Result<ID3DBlob> {
        let mut code_blob: Option<ID3DBlob> = None;
        let mut error_blob: Option<ID3DBlob> = None;

        let compile_res = unsafe {
            D3DCompile(
                EGUI_HLSL_SOURCE.as_ptr() as *const c_void,
                EGUI_HLSL_SOURCE.len(),
                PCSTR(b"frameforge_egui.hlsl\0".as_ptr()),
                None,
                None::<&ID3DInclude>,
                PCSTR(entry_nul.as_ptr()),
                PCSTR(target_nul.as_ptr()),
                D3DCOMPILE_OPTIMIZATION_LEVEL3,
                0,
                &mut code_blob,
                Some(&mut error_blob),
            )
        };

        if let Err(err) = compile_res {
            let details = Self::blob_to_string(error_blob.as_ref());
            bail!("D3DCompile fehlgeschlagen ({err}): {details}");
        }

        code_blob.context("D3DCompile lieferte keinen Shader-Code-Blob")
    }

    fn blob_to_string(blob: Option<&ID3DBlob>) -> String {
        let Some(blob) = blob else {
            return String::from("<keine Details>");
        };
        unsafe {
            let ptr = blob.GetBufferPointer() as *const u8;
            let len = blob.GetBufferSize();
            if ptr.is_null() || len == 0 {
                return String::from("<leer>");
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            String::from_utf8_lossy(slice).trim().to_string()
        }
    }

    fn srv_cpu_handle(&self, slot: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let base = unsafe { self.srv_heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + (slot as usize) * self.srv_descriptor_size,
        }
    }

    fn srv_gpu_handle(&self, slot: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        let base = unsafe { self.srv_heap.GetGPUDescriptorHandleForHeapStart() };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + (slot as u64) * (self.srv_descriptor_size as u64),
        }
    }

    fn transition_barrier(
        resource: &ID3D12Resource,
        state_before: D3D12_RESOURCE_STATES,
        state_after: D3D12_RESOURCE_STATES,
    ) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: unsafe { std::mem::transmute_copy(resource) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: state_before,
                    StateAfter: state_after,
                }),
            },
        }
    }
}
