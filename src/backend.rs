use std::ffi::c_void;
use std::mem::ManuallyDrop;

use anyhow::{bail, Context, Result};
use windows::core::PCSTR;
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
  D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
  DXGI_FORMAT, DXGI_FORMAT_R16G16_FLOAT, DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM,
  DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain3;

use crate::ui::{DebugViewMode, LogStore};

const SRVS_PER_PASS: usize = 5;
const SRV_RING_SLOTS: usize = 8;
const TOTAL_SRV_DESCRIPTORS: u32 = (SRV_RING_SLOTS * SRVS_PER_PASS) as u32;
const ROOT_CONSTANT_COUNT: u32 = 12;

// Fullscreen video + Flow-Scale-ready F4 G-Buffer visualizer + universal HUD/subtitle protection
// shader (vs_5_0 / ps_5_0, strictly 7-bit ASCII):
// 0 = Normal (HUD-Guard + Limited-RGB + RCAS), 1 = HSV Motion Wheel, 2 = Edge-Aegis Depth,
// 3 = 3-way Split-Screen (2px dividers).
const PASSTHROUGH_HLSL_SOURCE: &[u8] = br#"
cbuffer VideoConstants : register(b0) {
  float2 src_size;
  float2 dst_size;
  float2 gb_size;
  float  rcas_sharpness;
  float  expand_limited_rgb;
  float  debug_view_mode;
  uint   is_interpolated_frame;
  uint   hud_protection;
  uint   _pad0;
};

Texture2D<float4> video_tex     : register(t0);
Texture2D<float2> motion_tex    : register(t1);
Texture2D<float>  depth_tex     : register(t2);
Texture2D<float4> real_curr_tex : register(t3);
Texture2D<float4> prev_col_tex  : register(t4);
SamplerState      linear_clamp  : register(s0);

struct PSInput {
  float4 pos : SV_POSITION;
  float2 uv  : TEXCOORD0;
};

PSInput vs_main(uint vid : SV_VertexID) {
  PSInput o;
  o.uv = float2((vid << 1) & 2, vid & 2);
  o.pos = float4(o.uv.x * 2.0 - 1.0, 1.0 - o.uv.y * 2.0, 0.0, 1.0);
  return o;
}

float max3(float3 v) {
  return max(v.r, max(v.g, v.b));
}

float luma(float3 rgb) {
  return dot(rgb, float3(0.2126, 0.7152, 0.0722));
}

float3 expand_range(float3 c) {
  return (expand_limited_rgb > 0.5)
    ? saturate((c - (16.0 / 255.0)) * (255.0 / 219.0))
    : c;
}

float eval_local_mv_norm_len(float2 uv, float2 inv_gb, float2 ht_gb) {
  float2 max_gb = float2(1.0, 1.0) - ht_gb;
  float2 c_uv = clamp(uv, ht_gb, max_gb);
  float2 mv_c = motion_tex.SampleLevel(linear_clamp, c_uv, 0.0).xy * inv_gb;
  float2 mv_l = motion_tex.SampleLevel(linear_clamp, clamp(uv + float2(-2.0 * inv_gb.x, 0.0), ht_gb, max_gb), 0.0).xy * inv_gb;
  float2 mv_r = motion_tex.SampleLevel(linear_clamp, clamp(uv + float2( 2.0 * inv_gb.x, 0.0), ht_gb, max_gb), 0.0).xy * inv_gb;
  float2 mv_u = motion_tex.SampleLevel(linear_clamp, clamp(uv + float2(0.0, -2.0 * inv_gb.y), ht_gb, max_gb), 0.0).xy * inv_gb;
  float2 mv_d = motion_tex.SampleLevel(linear_clamp, clamp(uv + float2(0.0,  2.0 * inv_gb.y), ht_gb, max_gb), 0.0).xy * inv_gb;
  return max(length(mv_c), max(max(length(mv_l), length(mv_r)), max(length(mv_u), length(mv_d))));
}

// Samples video_tex (`t0`), restores static HUD/subtitles from `real_curr_tex` (`t3`) when
// `hud_protection == 1 && is_interpolated_frame == 1` BEFORE Limited-RGB expansion and RCAS!
float3 sample_rgb(float2 uv, float2 ht_src, float mv_gate) {
  float2 cuv = clamp(uv, ht_src, float2(1.0, 1.0) - ht_src);
  float3 c = video_tex.SampleLevel(linear_clamp, cuv, 0.0).rgb;

  if (mv_gate > 0.001) {
    float3 real_curr = real_curr_tex.SampleLevel(linear_clamp, cuv, 0.0).rgb;
    float3 prev_col  = prev_col_tex.SampleLevel(linear_clamp, cuv, 0.0).rgb;
    float static_diff = max3(abs(real_curr - prev_col));
    float static_mask = 1.0 - smoothstep(0.010, 0.028, static_diff);
    c = lerp(c, real_curr, static_mask * mv_gate);
  }

  return expand_range(c);
}

float3 eval_video_rcas(float2 uv, float2 tx, float2 ht_src, float2 inv_gb, float2 ht_gb) {
  float mv_gate = 0.0;
  if (hud_protection != 0 && is_interpolated_frame != 0) {
    float mv_norm_len = eval_local_mv_norm_len(uv, inv_gb, ht_gb);
    mv_gate = smoothstep(0.0009, 0.0022, mv_norm_len);
  }

  float3 c = sample_rgb(uv, ht_src, mv_gate);
  if (rcas_sharpness <= 0.001) {
    return c;
  }

  float3 n = sample_rgb(uv + float2(0.0, -tx.y), ht_src, mv_gate);
  float3 s = sample_rgb(uv + float2(0.0,  tx.y), ht_src, mv_gate);
  float3 w = sample_rgb(uv + float2(-tx.x, 0.0), ht_src, mv_gate);
  float3 e = sample_rgb(uv + float2( tx.x, 0.0), ht_src, mv_gate);

  float lc = luma(c), ln = luma(n), ls = luma(s), lw = luma(w), le = luma(e);
  float mn = min(lc, min(min(ln, ls), min(lw, le)));
  float mx = max(lc, max(max(ln, ls), max(lw, le)));
  float lim = max(-mn / (4.0 * mx + 1e-5), -(1.0 - mx) / (4.0 * (1.0 - mn) + 1e-5));
  float lobe = max(lerp(0.0, -0.1875, saturate(rcas_sharpness)), lim);
  return saturate((lobe * (n + s + w + e) + c) / (4.0 * lobe + 1.0));
}

float3 hsv_to_rgb(float h, float s, float v) {
  float3 k = frac(float3(h, h + (2.0 / 3.0), h + (1.0 / 3.0))) * 6.0 - 3.0;
  return v * lerp(float3(1.0, 1.0, 1.0), saturate(abs(k) - 1.0), s);
}

// Samples `motion_tex` (`t1`) over normalized `video_uv` and scales wheel intensity via `gb_size`
// so F4 covers the full 1920x1080 image identically at both 100% and 50% Flow-Scale.
float3 eval_motion_wheel(float2 video_uv, float2 ht_src, float2 inv_gb, float2 ht_gb) {
  float2 gb_uv = clamp(video_uv, ht_gb, float2(1.0, 1.0) - ht_gb);
  float2 mv = motion_tex.Sample(linear_clamp, gb_uv).xy;
  float2 mv_norm = mv * inv_gb;
  float2 mv_src_px = mv_norm * max(src_size, float2(1.0, 1.0));
  float spd = saturate(length(mv_src_px) / 14.0);
  float hue = frac(atan2(mv_norm.y, mv_norm.x) * (1.0 / 6.2831853) + 1.0);
  float3 base_bg = sample_rgb(video_uv, ht_src, 0.0) * 0.20;
  return lerp(base_bg, hsv_to_rgb(hue, saturate(0.25 + 0.75 * spd), spd), smoothstep(0.015, 0.80, spd));
}

float3 eval_edge_aegis_depth(float2 video_uv, float2 ht_src, float2 ht_gb) {
  float2 gb_uv = clamp(video_uv, ht_gb, float2(1.0, 1.0) - ht_gb);
  float d = saturate(depth_tex.Sample(linear_clamp, gb_uv).r);
  float3 c0 = float3(0.04, 0.06, 0.24), c1 = float3(0.08, 0.48, 0.78);
  float3 c2 = float3(0.92, 0.36, 0.14), c3 = float3(1.00, 0.96, 0.76);
  float3 heat = (d < 0.30)
    ? lerp(c0, c1, d / 0.30)
    : ((d < 0.68) ? lerp(c1, c2, (d - 0.30) / 0.38) : lerp(c2, c3, (d - 0.68) / 0.32));
  return saturate(heat * lerp(0.86, 1.12, luma(sample_rgb(video_uv, ht_src, 0.0))));
}

float4 ps_main(PSInput input) : SV_TARGET {
  float sa = max(src_size.x, 1.0) / max(src_size.y, 1.0);
  float da = max(dst_size.x, 1.0) / max(dst_size.y, 1.0);
  float2 scale = (da > sa) ? float2(sa / da, 1.0) : float2(1.0, da / sa);
  float2 video_uv = (input.uv - 0.5) / scale + 0.5;
  bool in_bounds = (video_uv.x >= 0.0 && video_uv.x <= 1.0 && video_uv.y >= 0.0 && video_uv.y <= 1.0);

  float2 tx = 1.0 / max(src_size, float2(1.0, 1.0));
  float2 ht_src = tx * 0.5;
  float2 safe_gb = max(gb_size, float2(1.0, 1.0));
  float2 inv_gb = 1.0 / safe_gb;
  float2 ht_gb = inv_gb * 0.5;

  int mode = int(debug_view_mode + 0.5);
  float3 out_rgb = float3(0.0, 0.0, 0.0);

  if (mode == 1) {
    float3 mw = eval_motion_wheel(video_uv, ht_src, inv_gb, ht_gb);
    out_rgb = in_bounds ? mw : float3(0.0, 0.0, 0.0);
  } else if (mode == 2) {
    float3 dp = eval_edge_aegis_depth(video_uv, ht_src, ht_gb);
    out_rgb = in_bounds ? dp : float3(0.0, 0.0, 0.0);
  } else if (mode == 3) {
    float3 mw = eval_motion_wheel(video_uv, ht_src, inv_gb, ht_gb);
    float3 dp = eval_edge_aegis_depth(video_uv, ht_src, ht_gb);
    if (in_bounds) {
      float lh = 1.0 / max(dst_size.x * scale.x, 1.0);
      float s1 = 1.0 / 3.0;
      float s2 = 2.0 / 3.0;
      if (abs(video_uv.x - s1) <= lh || abs(video_uv.x - s2) <= lh) {
        out_rgb = float3(0.95, 0.97, 1.0);
      } else if (video_uv.x < s1) {
        out_rgb = eval_video_rcas(video_uv, tx, ht_src, inv_gb, ht_gb);
      } else if (video_uv.x < s2) {
        out_rgb = mw;
      } else {
        out_rgb = dp;
      }
    }
  } else if (in_bounds) {
    out_rgb = eval_video_rcas(video_uv, tx, ht_src, inv_gb, ht_gb);
  }

  return float4(out_rgb, 1.0);
}
"#;

#[repr(C)]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
struct PassthroughRootConstants {
  src_width: f32,
  src_height: f32,
  dst_width: f32,
  dst_height: f32,
  gb_width: f32,
  gb_height: f32,
  rcas_sharpness: f32,
  expand_limited_rgb: f32,
  debug_view_mode: f32,
  is_interpolated_frame: u32,
  hud_protection: u32,
  _pad0: u32,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct SyntheticGBufferInputs<'a> {
  pub color_input: ID3D12Resource,
  pub motion_vectors: ID3D12Resource,
  pub depth_buffer: ID3D12Resource,
  pub real_curr_color: Option<&'a ID3D12Resource>,
  pub prev_color: Option<&'a ID3D12Resource>,
  pub gbuffer_size: (u32, u32),
  pub is_interpolated_frame: bool,
  pub hud_protection: bool,
  pub color_width: u32,
  pub color_height: u32,
  pub output_width: u32,
  pub output_height: u32,
  pub delta_time_ms: f32,
  pub jitter_offset: [f32; 2],
  pub expand_limited_rgb: bool,
  pub rcas_sharpness: f32,
  pub debug_view_mode: DebugViewMode,
}

pub trait FrameGenBackend {
  fn name(&self) -> &'static str;
  fn initialize(
    &mut self,
    device: &ID3D12Device,
    swapchain: &IDXGISwapChain3,
    width: u32,
    height: u32,
    logs: &mut LogStore,
  ) -> Result<()>;
  fn dispatch(
    &mut self,
    device: &ID3D12Device,
    cmd_list: &ID3D12GraphicsCommandList,
    inputs: &SyntheticGBufferInputs<'_>,
  ) -> Result<()>;
  fn resize(&mut self, width: u32, height: u32) -> Result<()>;
}

#[derive(Default)]
pub struct PassthroughBackend {
  pipeline_state: Option<ID3D12PipelineState>,
  root_signature: Option<ID3D12RootSignature>,
  srv_heap: Option<ID3D12DescriptorHeap>,
  srv_descriptor_size: usize,
  ring_cursor: usize,
  initialized: bool,
  width: u32,
  height: u32,
}

impl PassthroughBackend {
  pub fn new() -> Self {
    Self::default()
  }

  #[allow(dead_code)]
  pub fn dimensions(&self) -> (u32, u32) {
    (self.width, self.height)
  }

  fn create_root_signature(device: &ID3D12Device, logs: &mut LogStore) -> Result<ID3D12RootSignature> {
    let srv = [D3D12_DESCRIPTOR_RANGE {
      RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
      NumDescriptors: SRVS_PER_PASS as u32,
      BaseShaderRegister: 0,
      RegisterSpace: 0,
      OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    }];
    let params = [
      D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
          Constants: D3D12_ROOT_CONSTANTS {
            ShaderRegister: 0,
            RegisterSpace: 0,
            Num32BitValues: ROOT_CONSTANT_COUNT,
          },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
      },
      D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
          DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
            NumDescriptorRanges: 1,
            pDescriptorRanges: srv.as_ptr(),
          },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
      },
    ];
    let sampler = [D3D12_STATIC_SAMPLER_DESC {
      Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
      AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
      AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
      AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
      MipLODBias: 0.0_f32,
      MaxAnisotropy: 0,
      ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
      BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_BLACK,
      MinLOD: 0.0_f32,
      MaxLOD: f32::MAX,
      ShaderRegister: 0,
      RegisterSpace: 0,
      ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
    }];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
      NumParameters: params.len() as u32,
      pParameters: params.as_ptr(),
      NumStaticSamplers: sampler.len() as u32,
      pStaticSamplers: sampler.as_ptr(),
      Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };
    let (mut sig_blob, mut err_blob) = (None, None);
    if let Err(err) = unsafe {
      D3D12SerializeRootSignature(
        &desc,
        D3D_ROOT_SIGNATURE_VERSION_1,
        &mut sig_blob,
        Some(&mut err_blob),
      )
    } {
      let d = Self::blob_str(err_blob.as_ref());
      logs.error(format!(
        "[Shader] Passthrough-RootSignature fehlgeschlagen ({err}): {d}"
      ));
      bail!("Passthrough-RootSignature fehlgeschlagen ({err}): {d}");
    }
    let blob = sig_blob.context("Kein RootSignature-Blob fuer PassthroughBackend")?;
    let bytes = unsafe {
      std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    };
    unsafe {
      device
        .CreateRootSignature(0, bytes)
        .context("CreateRootSignature (PassthroughBackend) fehlgeschlagen")
    }
  }

  fn create_pipeline_state(
    device: &ID3D12Device,
    root_sig: &ID3D12RootSignature,
    logs: &mut LogStore,
  ) -> Result<ID3D12PipelineState> {
    let vs_blob = Self::compile_hlsl(b"vs_main\0", b"vs_5_0\0", logs)?;
    let ps_blob = Self::compile_hlsl(b"ps_main\0", b"ps_5_0\0", logs)?;
    let mut blend = D3D12_BLEND_DESC {
      AlphaToCoverageEnable: BOOL(0),
      IndependentBlendEnable: BOOL(0),
      ..Default::default()
    };
    blend.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
      BlendEnable: BOOL(0),
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
    let mut pso_desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
      pRootSignature: ManuallyDrop::new(Some(root_sig.clone())),
      VS: D3D12_SHADER_BYTECODE {
        pShaderBytecode: unsafe { vs_blob.GetBufferPointer() },
        BytecodeLength: unsafe { vs_blob.GetBufferSize() },
      },
      PS: D3D12_SHADER_BYTECODE {
        pShaderBytecode: unsafe { ps_blob.GetBufferPointer() },
        BytecodeLength: unsafe { ps_blob.GetBufferSize() },
      },
      BlendState: blend,
      SampleMask: u32::MAX,
      RasterizerState: D3D12_RASTERIZER_DESC {
        FillMode: D3D12_FILL_MODE_SOLID,
        CullMode: D3D12_CULL_MODE_NONE,
        FrontCounterClockwise: BOOL(0),
        DepthClipEnable: BOOL(1),
        ..Default::default()
      },
      DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
        DepthEnable: BOOL(0),
        StencilEnable: BOOL(0),
        ..Default::default()
      },
      InputLayout: D3D12_INPUT_LAYOUT_DESC {
        pInputElementDescs: std::ptr::null(),
        NumElements: 0,
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
    let res = unsafe { device.CreateGraphicsPipelineState(&pso_desc) };
    unsafe {
      ManuallyDrop::drop(&mut pso_desc.pRootSignature);
    }
    res.context("CreateGraphicsPipelineState (PassthroughBackend) fehlgeschlagen")
  }

  fn create_srv_2d(
    device: &ID3D12Device,
    res: &ID3D12Resource,
    fmt: DXGI_FORMAT,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
  ) {
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
      Format: fmt,
      ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
      Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
      Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
        Texture2D: D3D12_TEX2D_SRV {
          MostDetailedMip: 0,
          MipLevels: 1,
          PlaneSlice: 0,
          ResourceMinLODClamp: 0.0_f32,
        },
      },
    };
    unsafe {
      device.CreateShaderResourceView(res, Some(&desc), handle);
    }
  }

  fn compile_hlsl(entry_nul: &[u8], target_nul: &[u8], logs: &mut LogStore) -> Result<ID3DBlob> {
    let (mut code_blob, mut err_blob) = (None, None);
    let res = unsafe {
      D3DCompile(
        PASSTHROUGH_HLSL_SOURCE.as_ptr() as *const c_void,
        PASSTHROUGH_HLSL_SOURCE.len(),
        PCSTR(b"frameforge_passthrough_video.hlsl\0".as_ptr()),
        None,
        None::<&ID3DInclude>,
        PCSTR(entry_nul.as_ptr()),
        PCSTR(target_nul.as_ptr()),
        D3DCOMPILE_OPTIMIZATION_LEVEL3,
        0,
        &mut code_blob,
        Some(&mut err_blob),
      )
    };
    if let Err(err) = res {
      let d = Self::blob_str(err_blob.as_ref());
      logs.error(format!(
        "[Shader] HLSL '{}' fehlgeschlagen ({err}): {d}",
        String::from_utf8_lossy(entry_nul).trim_end_matches('\0')
      ));
      bail!("D3DCompile (Passthrough) fehlgeschlagen ({err}): {d}");
    }
    code_blob.context("Kein Shader-Blob fuer PassthroughBackend erhalten")
  }

  fn blob_str(blob: Option<&ID3DBlob>) -> String {
    let Some(b) = blob else {
      return String::from("<keine Details>");
    };
    unsafe {
      let (ptr, len) = (b.GetBufferPointer() as *const u8, b.GetBufferSize());
      if ptr.is_null() || len == 0 {
        String::from("<leer>")
      } else {
        String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len))
          .trim()
          .to_string()
      }
    }
  }
}

impl FrameGenBackend for PassthroughBackend {
  fn name(&self) -> &'static str {
    "PassthroughBackend"
  }

  fn initialize(
    &mut self,
    device: &ID3D12Device,
    _swapchain: &IDXGISwapChain3,
    width: u32,
    height: u32,
    logs: &mut LogStore,
  ) -> Result<()> {
    if width == 0 || height == 0 {
      bail!("Ungueltige Initialisierungsaufloesung: {}x{}", width, height);
    }
    let root_signature = Self::create_root_signature(device, logs)?;
    let pipeline_state = Self::create_pipeline_state(device, &root_signature, logs)?;
    let hdesc = D3D12_DESCRIPTOR_HEAP_DESC {
      Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
      NumDescriptors: TOTAL_SRV_DESCRIPTORS,
      Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
      NodeMask: 0,
    };
    let srv_heap: ID3D12DescriptorHeap = unsafe {
      device
        .CreateDescriptorHeap(&hdesc)
        .context("Passthrough-SRV-Heap fehlgeschlagen")?
    };
    let srv_descriptor_size = unsafe {
      device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
    } as usize;
    logs.info(
      "[FrameForge][Shader] Passthrough-, Flow-Scale-F4-Visualizer & universeller HUD-/Untertitel-Schutz bereit.",
    );
    self.root_signature = Some(root_signature);
    self.pipeline_state = Some(pipeline_state);
    self.srv_heap = Some(srv_heap);
    self.srv_descriptor_size = srv_descriptor_size;
    self.ring_cursor = 0;
    self.width = width;
    self.height = height;
    self.initialized = true;
    Ok(())
  }

  fn dispatch(
    &mut self,
    device: &ID3D12Device,
    cmd_list: &ID3D12GraphicsCommandList,
    inputs: &SyntheticGBufferInputs<'_>,
  ) -> Result<()> {
    if !self.initialized {
      bail!("{} wurde vor dispatch() nicht initialisiert", self.name());
    }
    let (Some(pso), Some(root_sig), Some(srv_heap)) =
      (&self.pipeline_state, &self.root_signature, &self.srv_heap)
    else {
      bail!("GPU-Pipeline-Ressourcen im PassthroughBackend fehlen");
    };
    if inputs.output_width == 0
      || inputs.output_height == 0
      || inputs.color_width == 0
      || inputs.color_height == 0
    {
      return Ok(());
    }
    let _ = (inputs.delta_time_ms, inputs.jitter_offset);

    let gb_w = if inputs.gbuffer_size.0 > 0 {
      inputs.gbuffer_size.0
    } else {
      inputs.color_width
    };
    let gb_h = if inputs.gbuffer_size.1 > 0 {
      inputs.gbuffer_size.1
    } else {
      inputs.color_height
    };

    let real_curr_res: &ID3D12Resource = inputs.real_curr_color.unwrap_or(&inputs.color_input);
    let prev_col_res: &ID3D12Resource = inputs.prev_color.unwrap_or(real_curr_res);

    let c = PassthroughRootConstants {
      src_width: inputs.color_width as f32,
      src_height: inputs.color_height as f32,
      dst_width: inputs.output_width as f32,
      dst_height: inputs.output_height as f32,
      gb_width: gb_w as f32,
      gb_height: gb_h as f32,
      rcas_sharpness: (inputs.rcas_sharpness / 100.0_f32).clamp(0.0_f32, 1.0_f32),
      expand_limited_rgb: if inputs.expand_limited_rgb {
        1.0_f32
      } else {
        0.0_f32
      },
      debug_view_mode: inputs.debug_view_mode.shader_index() as f32,
      is_interpolated_frame: if inputs.is_interpolated_frame {
        1_u32
      } else {
        0_u32
      },
      hud_protection: if inputs.hud_protection { 1_u32 } else { 0_u32 },
      _pad0: 0,
    };

    let vp = D3D12_VIEWPORT {
      TopLeftX: 0.0_f32,
      TopLeftY: 0.0_f32,
      Width: inputs.output_width as f32,
      Height: inputs.output_height as f32,
      MinDepth: 0.0_f32,
      MaxDepth: 1.0_f32,
    };
    let sc = RECT {
      left: 0,
      top: 0,
      right: inputs.output_width as i32,
      bottom: inputs.output_height as i32,
    };

    let slot_base = (self.ring_cursor % SRV_RING_SLOTS) * SRVS_PER_PASS;
    self.ring_cursor = self.ring_cursor.wrapping_add(1);

    unsafe {
      let cpu_start = srv_heap.GetCPUDescriptorHandleForHeapStart();
      let gpu_start = srv_heap.GetGPUDescriptorHandleForHeapStart();
      let stride = self.srv_descriptor_size;

      let cpu_slot = |offset: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: cpu_start.ptr + (slot_base + offset) * stride,
      };
      let gpu_table = D3D12_GPU_DESCRIPTOR_HANDLE {
        ptr: gpu_start.ptr + ((slot_base * stride) as u64),
      };

      Self::create_srv_2d(device, &inputs.color_input, DXGI_FORMAT_R8G8B8A8_UNORM, cpu_slot(0));
      Self::create_srv_2d(
        device,
        &inputs.motion_vectors,
        DXGI_FORMAT_R16G16_FLOAT,
        cpu_slot(1),
      );
      Self::create_srv_2d(device, &inputs.depth_buffer, DXGI_FORMAT_R32_FLOAT, cpu_slot(2));
      Self::create_srv_2d(device, real_curr_res, DXGI_FORMAT_R8G8B8A8_UNORM, cpu_slot(3));
      Self::create_srv_2d(device, prev_col_res, DXGI_FORMAT_R8G8B8A8_UNORM, cpu_slot(4));

      cmd_list.SetDescriptorHeaps(&[Some(srv_heap.clone())]);
      cmd_list.SetPipelineState(pso);
      cmd_list.SetGraphicsRootSignature(root_sig);
      cmd_list.RSSetViewports(&[vp]);
      cmd_list.RSSetScissorRects(&[sc]);
      cmd_list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
      cmd_list.SetGraphicsRoot32BitConstants(
        0,
        ROOT_CONSTANT_COUNT,
        &c as *const PassthroughRootConstants as *const c_void,
        0,
      );
      cmd_list.SetGraphicsRootDescriptorTable(1, gpu_table);
      cmd_list.DrawInstanced(3, 1, 0, 0);
    }
    Ok(())
  }

  fn resize(&mut self, width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
      bail!("Ungueltige Zielaufloesung: {}x{}", width, height);
    }
    self.width = width;
    self.height = height;
    Ok(())
  }
}
