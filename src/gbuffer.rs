use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};

use anyhow::{bail, Context, Result};
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DInclude};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
  DXGI_FORMAT, DXGI_FORMAT_R16G16_FLOAT, DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM,
  DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};

use crate::dx12::BUFFER_COUNT;
use crate::ui::LogStore;

pub const SHADER_READ_STATES: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(
  D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0,
);

const PASS1_SLOT: usize = 0;
const PASS2_SLOT: usize = 5;
const TOTAL_DESCRIPTORS: u32 = 10;
const TIMER_SLOTS: usize = BUFFER_COUNT;
const ROOT_CONSTANT_COUNT: u32 = 12;

// 2-Pass Compute Shader (cs_5_0, strictly 7-bit ASCII):
// Supports both 100% Native G-Buffer (gb_dims == video_dims) and 50% Flow-Scale (gb_dims == video_dims / 2),
// plus optional `fast_search` turbo mode (5-Tap Diamond in Pass 1 + lean 5-Tap Cross & direct LK in Pass 2).
// Motion vectors (`mv_uav`, R16G16_FLOAT) are always output in `gb_dims` pixel units so that
// `uv_prev = uv_curr + mv / float2(gb_dims)` holds identically at both 100% and 50% Flow-Scale.
const GBUFFER_HLSL_SOURCE: &[u8] = br#"
cbuffer GBufferConstants : register(b0) {
  uint2 gb_dims;
  uint2 coarse_dims;
  uint2 video_dims;
  uint  fast_search;
  float expand_limited_rgb;
  float motion_sensitivity;
  float edge_aegis_strength;
  float noise_deadzone;
  float search_px_scale;
};

Texture2D<float4>   curr_color    : register(t0);
Texture2D<float4>   prev_color    : register(t1);
Texture2D<float2>   coarse_mv_srv : register(t2);
RWTexture2D<float2> mv_uav        : register(u0);
RWTexture2D<float>  depth_uav     : register(u1);
SamplerState        linear_clamp  : register(s0);

float to_luma(float3 rgb) {
  float3 c = (expand_limited_rgb > 0.5)
    ? saturate((rgb - (16.0 / 255.0)) * (255.0 / 219.0))
    : rgb;
  return dot(c, float3(0.2126, 0.7152, 0.0722));
}

int2 clamp_c(int2 q) {
  return clamp(q, int2(0, 0), int2(max(int(coarse_dims.x) - 1, 0), max(int(coarse_dims.y) - 1, 0)));
}

float samp_luma(Texture2D<float4> t, float2 uv) {
  float2 half_vid = 0.5 / max(float2(video_dims), float2(1.0, 1.0));
  float2 cuv = clamp(uv, half_vid, float2(1.0, 1.0) - half_vid);
  return to_luma(t.SampleLevel(linear_clamp, cuv, 0.0).rgb);
}

float samp_curr_gb(int2 gb_p, float2 inv_gb) {
  float2 max_p = float2(max(int(gb_dims.x) - 1, 0), max(int(gb_dims.y) - 1, 0));
  float2 cp = clamp(float2(gb_p), float2(0.0, 0.0), max_p);
  return samp_luma(curr_color, (cp + 0.5) * inv_gb);
}

float box4(Texture2D<float4> t, float2 gb_px, float2 inv_gb) {
  return 0.25 * (
    samp_luma(t, (gb_px + float2(-1.0, -1.0)) * inv_gb) +
    samp_luma(t, (gb_px + float2( 1.0, -1.0)) * inv_gb) +
    samp_luma(t, (gb_px + float2(-1.0,  1.0)) * inv_gb) +
    samp_luma(t, (gb_px + float2( 1.0,  1.0)) * inv_gb)
  );
}

float coarse_sad(float2 b, float2 mv, float2 inv_gb, float pen_scale, float c0, float cn, float cs, float cw, float ce) {
  float2 p = b + mv;
  return abs(c0 - box4(prev_color, p, inv_gb)) * 2.0
    + abs(cn - box4(prev_color, p + float2( 0.0, -4.0), inv_gb))
    + abs(cs - box4(prev_color, p + float2( 0.0,  4.0), inv_gb))
    + abs(cw - box4(prev_color, p + float2(-4.0,  0.0), inv_gb))
    + abs(ce - box4(prev_color, p + float2( 4.0,  0.0), inv_gb))
    + length(mv) * pen_scale;
}

[numthreads(8, 8, 1)]
void cs_coarse_flow(uint3 dtid : SV_DispatchThreadID) {
  if (dtid.x >= coarse_dims.x || dtid.y >= coarse_dims.y) {
    return;
  }

  float2 inv_gb = 1.0 / max(float2(gb_dims), float2(1.0, 1.0));
  float px_scale = clamp(search_px_scale, 0.25, 1.0);
  float pen_scale = 0.00075 / px_scale;
  float2 b = float2(dtid.xy) * 4.0 + 2.0;

  float c0 = box4(curr_color, b, inv_gb);
  float cn = box4(curr_color, b + float2( 0.0, -4.0), inv_gb);
  float cs = box4(curr_color, b + float2( 0.0,  4.0), inv_gb);
  float cw = box4(curr_color, b + float2(-4.0,  0.0), inv_gb);
  float ce = box4(curr_color, b + float2( 4.0,  0.0), inv_gb);

  float contrast = max(max(abs(c0 - cn), abs(c0 - cs)), max(abs(c0 - cw), abs(c0 - ce)));
  float zcost = coarse_sad(b, float2(0.0, 0.0), inv_gb, pen_scale, c0, cn, cs, cw, ce);
  if (zcost < noise_deadzone || contrast < 0.007) {
    mv_uav[dtid.xy] = float2(0.0, 0.0);
    return;
  }

  float r_scale = clamp(motion_sensitivity, 0.5, 1.5);
  float2 best_mv = float2(0.0, 0.0);
  float best_cost = zcost;

  if (fast_search != 0) {
    // Turbo 5-Tap Diamond Search (center `zcost` + 4 cardinal taps per logarithmic step)
    static const float2 CARDINAL[4] = {
      float2(1.0, 0.0), float2(-1.0, 0.0), float2(0.0, 1.0), float2(0.0, -1.0)
    };
    float step0 = max(round(12.0 * px_scale * r_scale), 2.0);
    float step1 = max(round(5.0 * px_scale * r_scale), 1.0);
    float step2 = max(round(2.0 * px_scale), 1.0);
    float steps[3] = { step0, step1, step2 };

    [unroll] for (int s = 0; s < 3; ++s) {
      float2 cmv = best_mv;
      float st = steps[s];
      [unroll] for (int d = 0; d < 4; ++d) {
        float2 cand = cmv + CARDINAL[d] * st;
        float cost = coarse_sad(b, cand, inv_gb, pen_scale, c0, cn, cs, cw, ce);
        if (cost < best_cost) {
          best_cost = cost;
          best_mv = cand;
        }
      }
    }
  } else {
    // Full Wide-Range Hexagonal + 8-Way Diamond Search
    static const float2 HEX[6] = {
      float2(1.0, 0.0), float2(-1.0, 0.0), float2(0.5, 0.8660254),
      float2(-0.5, 0.8660254), float2(0.5, -0.8660254), float2(-0.5, -0.8660254)
    };
    float hr0 = max(18.0 * px_scale * r_scale, 3.0);
    float hr1 = max(9.0 * px_scale * r_scale, 1.5);
    float hrs[2] = { hr0, hr1 };

    [unroll] for (int s = 0; s < 2; ++s) {
      float2 cmv = best_mv;
      [unroll] for (int h = 0; h < 6; ++h) {
        float2 cand = round(cmv + HEX[h] * hrs[s]);
        float cost = coarse_sad(b, cand, inv_gb, pen_scale, c0, cn, cs, cw, ce);
        if (cost < best_cost) {
          best_cost = cost;
          best_mv = cand;
        }
      }
    }

    static const float2 DIA[8] = {
      float2(1.0, 0.0), float2(-1.0, 0.0), float2(0.0, 1.0), float2(0.0, -1.0),
      float2(1.0, 1.0), float2(-1.0, 1.0), float2(1.0, -1.0), float2(-1.0, -1.0)
    };
    float dr0 = max(round(4.0 * px_scale), 2.0);
    float dr1 = max(round(2.0 * px_scale), 1.0);
    float drs[2] = { dr0, dr1 };

    [unroll] for (int r = 0; r < 2; ++r) {
      float2 cmv = best_mv;
      [unroll] for (int d = 0; d < 8; ++d) {
        float2 cand = cmv + DIA[d] * drs[r];
        float cost = coarse_sad(b, cand, inv_gb, pen_scale, c0, cn, cs, cw, ce);
        if (cost < best_cost) {
          best_cost = cost;
          best_mv = cand;
        }
      }
    }
  }

  mv_uav[dtid.xy] = (best_cost > zcost * 0.87) ? float2(0.0, 0.0) : best_mv;
}

float full_err(float2 cpx, float2 mv, float2 inv_gb, float lc, float ln, float ls, float lw, float le) {
  float2 uv = (cpx + mv) * inv_gb;
  return abs(lc - samp_luma(prev_color, uv)) * 2.0
    + abs(ln - samp_luma(prev_color, uv + float2(0.0, -inv_gb.y)))
    + abs(ls - samp_luma(prev_color, uv + float2(0.0,  inv_gb.y)))
    + abs(lw - samp_luma(prev_color, uv + float2(-inv_gb.x, 0.0)))
    + abs(le - samp_luma(prev_color, uv + float2( inv_gb.x, 0.0)));
}

float fast_cross_err(float2 cpx, float2 mv, float2 inv_gb, float lc, float lw, float le, float ln, float ls) {
  float2 uv = (cpx + mv) * inv_gb;
  float pc = samp_luma(prev_color, uv);
  float ph = 0.5 * (samp_luma(prev_color, uv + float2(-inv_gb.x, 0.0)) + samp_luma(prev_color, uv + float2(inv_gb.x, 0.0)));
  float pv = 0.5 * (samp_luma(prev_color, uv + float2(0.0, -inv_gb.y)) + samp_luma(prev_color, uv + float2(0.0, inv_gb.y)));
  return abs(lc - pc) * 3.0 + abs(0.5 * (lw + le) - ph) + abs(0.5 * (ln + ls) - pv);
}

[numthreads(8, 8, 1)]
void cs_refine_gbuffer(uint3 dtid : SV_DispatchThreadID) {
  if (dtid.x >= gb_dims.x || dtid.y >= gb_dims.y) {
    return;
  }

  int2 p = int2(dtid.xy);
  int2 qc = clamp_c(p / 4);
  float2 inv_gb = 1.0 / max(float2(gb_dims), float2(1.0, 1.0));
  float px_scale = clamp(search_px_scale, 0.25, 1.0);
  float inv_px_scale = 1.0 / px_scale;
  float pen_scale = 0.0006 * inv_px_scale;
  float2 cpx = float2(p) + 0.5;

  float l_tl = samp_curr_gb(p + int2(-1, -1), inv_gb);
  float l_tc = samp_curr_gb(p + int2( 0, -1), inv_gb);
  float l_tr = samp_curr_gb(p + int2( 1, -1), inv_gb);
  float l_ml = samp_curr_gb(p + int2(-1,  0), inv_gb);
  float l_mc = samp_curr_gb(p,                inv_gb);
  float l_mr = samp_curr_gb(p + int2( 1,  0), inv_gb);
  float l_bl = samp_curr_gb(p + int2(-1,  1), inv_gb);
  float l_bc = samp_curr_gb(p + int2( 0,  1), inv_gb);
  float l_br = samp_curr_gb(p + int2( 1,  1), inv_gb);

  float berr = 0.0;
  float zerr = 0.0;
  float bg_w = 0.0;
  float shear = 0.0;
  float2 bmv = float2(0.0, 0.0);
  float2 bg_sum = float2(0.0, 0.0);
  float2 c_mv = coarse_mv_srv.Load(int3(qc, 0));
  float2 rmv = float2(0.0, 0.0);

  if (fast_search != 0) {
    // Lean 5-Tap Cross neighborhood refinement + single-fetch Lucas-Kanade sub-pixel update
    zerr = fast_cross_err(cpx, float2(0.0, 0.0), inv_gb, l_mc, l_ml, l_mr, l_tc, l_bc);
    berr = zerr;

    static const int2 CROSS5[5] = {
      int2(0, 0), int2(-1, 0), int2(1, 0), int2(0, -1), int2(0, 1)
    };
    [unroll] for (int k = 0; k < 5; ++k) {
      int2 off = CROSS5[k];
      float2 cand = coarse_mv_srv.Load(int3(clamp_c(qc + off), 0));
      if (k > 0) {
        bg_sum += cand;
        bg_w += 1.0;
        shear = max(shear, length(cand - c_mv));
      }
      float e = fast_cross_err(cpx, cand, inv_gb, l_mc, l_ml, l_mr, l_tc, l_bc) + length(cand) * pen_scale;
      if (e < berr) {
        berr = e;
        bmv = cand;
      }
    }

    if (berr < zerr * 0.91 || length(bmv) > 0.25 * px_scale) {
      float2 wuv = (cpx + bmv) * inv_gb;
      float p_mc = samp_luma(prev_color, wuv);
      float ix = 0.5 * (l_mr - l_ml);
      float iy = 0.5 * (l_bc - l_tc);
      float it = l_mc - p_mc;
      float2 lk = clamp(
        float2(ix, iy) * (it / (ix * ix + iy * iy + 0.015)) * clamp(motion_sensitivity, 0.5, 1.5),
        float2(-0.75, -0.75),
        float2(0.75, 0.75)
      );
      rmv = bmv + lk;
    }
  } else {
    // Full 3x3 neighborhood candidate selection + symmetric Lucas-Kanade + verification
    zerr = full_err(cpx, float2(0.0, 0.0), inv_gb, l_mc, l_tc, l_bc, l_ml, l_mr);
    berr = zerr;

    [unroll] for (int ny = -1; ny <= 1; ++ny) {
      [unroll] for (int nx = -1; nx <= 1; ++nx) {
        float2 cand = coarse_mv_srv.Load(int3(clamp_c(qc + int2(nx, ny)), 0));
        if (nx != 0 || ny != 0) {
          bg_sum += cand;
          bg_w += 1.0;
          shear = max(shear, length(cand - c_mv));
        }
        float e = full_err(cpx, cand, inv_gb, l_mc, l_tc, l_bc, l_ml, l_mr) + length(cand) * pen_scale;
        if (e < berr) {
          berr = e;
          bmv = cand;
        }
      }
    }

    if (berr < zerr * 0.91 || length(bmv) > 0.25 * px_scale) {
      rmv = bmv;
      float2 wuv = (cpx + bmv) * inv_gb;
      float p_mc = samp_luma(prev_color, wuv);
      float ix = 0.25 * ((l_mr - l_ml) + (samp_luma(prev_color, wuv + float2(inv_gb.x, 0.0)) - samp_luma(prev_color, wuv - float2(inv_gb.x, 0.0))));
      float iy = 0.25 * ((l_bc - l_tc) + (samp_luma(prev_color, wuv + float2(0.0, inv_gb.y)) - samp_luma(prev_color, wuv - float2(0.0, inv_gb.y))));
      float it = l_mc - p_mc;
      float2 lk = clamp(
        float2(ix, iy) * (it / (ix * ix + iy * iy + 0.012)) * clamp(motion_sensitivity, 0.5, 1.5),
        float2(-0.85, -0.85),
        float2(0.85, 0.85)
      );
      if (full_err(cpx, bmv + lk, inv_gb, l_mc, l_tc, l_bc, l_ml, l_mr) <= berr * 1.02) {
        rmv = bmv + lk;
      }
    }
  }

  mv_uav[dtid.xy] = rmv;

  // Edge Aegis Pseudo-Depth (R32_FLOAT, Reversed-Z [0.05 .. 0.99]), resolution-normalized
  float2 bg = (bg_w > 0.0) ? (bg_sum / bg_w) : float2(0.0, 0.0);
  float mv_disc = (length(rmv - bg) + 0.45 * shear) * inv_px_scale;
  float spd = length(rmv) * inv_px_scale;
  float sx = (l_tr + 2.0 * l_mr + l_br) - (l_tl + 2.0 * l_ml + l_bl);
  float sy = (l_bl + 2.0 * l_bc + l_br) - (l_tl + 2.0 * l_tc + l_tr);
  float sobel = saturate(sqrt(sx * sx + sy * sy) * 1.35);

  float2 uvn = cpx * inv_gb;
  float2 cd = (uvn - float2(0.5, 0.58)) * float2(1.15, 0.95);
  float base_d = lerp(0.07, 0.18, uvn.y * uvn.y) + exp(-dot(cd, cd) * 4.2) * 0.12;
  float par = saturate(spd * clamp(motion_sensitivity, 0.5, 1.5) * (1.0 / 16.0)) * 0.28;
  float aegis = (saturate(mv_disc / 6.5) * (0.40 + 0.60 * sobel) + sobel * saturate(spd * 0.12) * 0.22)
    * clamp(edge_aegis_strength, 0.0, 2.0);
  depth_uav[dtid.xy] = clamp(base_d + par + aegis, 0.05, 0.99);
}
"#;

#[repr(C)]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
struct GBufferRootConstants {
  gb_width: u32,
  gb_height: u32,
  coarse_width: u32,
  coarse_height: u32,
  video_width: u32,
  video_height: u32,
  fast_search: u32,
  expand_limited_rgb: f32,
  motion_sensitivity: f32,
  edge_aegis_strength: f32,
  noise_deadzone: f32,
  search_px_scale: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GBufferParams {
  pub enabled: bool,
  pub expand_limited_rgb: bool,
  pub motion_sensitivity: f32,
  pub edge_aegis_strength: f32,
  pub half_res_flow_scale: bool,
  pub fast_search: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GBufferStats {
  pub enabled: bool,
  pub active: bool,
  pub width: u32,
  pub height: u32,
  pub video_width: u32,
  pub video_height: u32,
  pub half_res_flow_scale: bool,
  pub fast_search: bool,
  pub gpu_compute_ms: f32,
}

struct GpuTimestampRing {
  query_heap: ID3D12QueryHeap,
  readback_buffer: ID3D12Resource,
  freq_hz: u64,
  slot_written: [bool; TIMER_SLOTS],
  smoothed_ms: f32,
  has_sample: bool,
}

struct SizedGBufferTextures {
  coarse_mv_texture: ID3D12Resource,
  motion_vectors_texture: ID3D12Resource,
  depth_buffer_texture: ID3D12Resource,
  bound_color_ptrs: (usize, usize),
  video_width: u32,
  video_height: u32,
  width: u32,
  height: u32,
  coarse_width: u32,
  coarse_height: u32,
  half_res_flow_scale: bool,
}

pub struct SyntheticGBuffer {
  pso_coarse_flow: ID3D12PipelineState,
  pso_refine_gbuffer: ID3D12PipelineState,
  root_signature: ID3D12RootSignature,
  descriptor_heap: ID3D12DescriptorHeap,
  descriptor_size: usize,
  dummy_motion_vectors: ID3D12Resource,
  dummy_depth_buffer: ID3D12Resource,
  dummy_coarse_srv: ID3D12Resource,
  scratch_uav_1x1: ID3D12Resource,
  sized: Option<SizedGBufferTextures>,
  retired_resources: Vec<ID3D12Resource>,
  gpu_timer: Option<GpuTimestampRing>,
  last_enabled: bool,
  last_fast_search: bool,
}

impl SyntheticGBuffer {
  pub fn new(
    device: &ID3D12Device,
    command_queue: &ID3D12CommandQueue,
    logs: &mut LogStore,
  ) -> Result<Self> {
    let root_signature = Self::create_root_signature(device, logs)?;
    let pso_coarse_flow =
      Self::create_compute_pso(device, &root_signature, b"cs_coarse_flow\0", logs)?;
    let pso_refine_gbuffer =
      Self::create_compute_pso(device, &root_signature, b"cs_refine_gbuffer\0", logs)?;

    let hdesc = D3D12_DESCRIPTOR_HEAP_DESC {
      Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
      NumDescriptors: TOTAL_DESCRIPTORS,
      Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
      NodeMask: 0,
    };
    let descriptor_heap: ID3D12DescriptorHeap = unsafe {
      device
        .CreateDescriptorHeap(&hdesc)
        .context("SyntheticGBuffer-DescriptorHeap fehlgeschlagen")?
    };
    let descriptor_size = unsafe {
      device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
    } as usize;

    let dummy_motion_vectors = Self::create_tex2d(
      device,
      1,
      1,
      DXGI_FORMAT_R16G16_FLOAT,
      D3D12_RESOURCE_FLAG_NONE,
      SHADER_READ_STATES,
    )?;
    let dummy_depth_buffer = Self::create_tex2d(
      device,
      1,
      1,
      DXGI_FORMAT_R32_FLOAT,
      D3D12_RESOURCE_FLAG_NONE,
      SHADER_READ_STATES,
    )?;
    let dummy_coarse_srv = Self::create_tex2d(
      device,
      1,
      1,
      DXGI_FORMAT_R16G16_FLOAT,
      D3D12_RESOURCE_FLAG_NONE,
      D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
    )?;
    let scratch_uav_1x1 = Self::create_tex2d(
      device,
      1,
      1,
      DXGI_FORMAT_R32_FLOAT,
      D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
      D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
    )?;
    let gpu_timer = Self::try_create_gpu_timer(device, command_queue, logs);

    logs.info(
      "[FrameForge][GBuffer] 2-Pass Compute-Pipeline bereit (100% / 50% Flow-Scale + Fast-Diamond-Turbo).",
    );
    Ok(Self {
      pso_coarse_flow,
      pso_refine_gbuffer,
      root_signature,
      descriptor_heap,
      descriptor_size,
      dummy_motion_vectors,
      dummy_depth_buffer,
      dummy_coarse_srv,
      scratch_uav_1x1,
      sized: None,
      retired_resources: Vec::new(),
      gpu_timer,
      last_enabled: true,
      last_fast_search: false,
    })
  }

  pub fn compute_target_dimensions(
    video_width: u32,
    video_height: u32,
    half_res_flow_scale: bool,
  ) -> (u32, u32) {
    let vw = video_width.max(1);
    let vh = video_height.max(1);
    if half_res_flow_scale {
      ((vw / 2).max(1), (vh / 2).max(1))
    } else {
      (vw, vh)
    }
  }

  #[allow(dead_code)]
  pub fn needs_resize(
    &self,
    video_width: u32,
    video_height: u32,
    half_res_flow_scale: bool,
  ) -> bool {
    let (gb_w, gb_h) =
      Self::compute_target_dimensions(video_width, video_height, half_res_flow_scale);
    self
      .sized
      .as_ref()
      .map(|s| {
        s.width != gb_w
          || s.height != gb_h
          || s.video_width != video_width.max(1)
          || s.video_height != video_height.max(1)
          || s.half_res_flow_scale != half_res_flow_scale
      })
      .unwrap_or(true)
  }

  #[allow(clippy::too_many_arguments)]
  pub fn ensure_resources(
    &mut self,
    device: &ID3D12Device,
    video_width: u32,
    video_height: u32,
    half_res_flow_scale: bool,
    curr_color: &ID3D12Resource,
    prev_color: &ID3D12Resource,
    mut logs: Option<&mut LogStore>,
  ) -> Result<()> {
    let (vw, vh) = (video_width.max(1), video_height.max(1));
    let (gb_w, gb_h) = Self::compute_target_dimensions(vw, vh, half_res_flow_scale);
    let (coarse_w, coarse_h) = ((gb_w / 4).max(1), (gb_h / 4).max(1));
    let ptrs = (
      Interface::as_raw(curr_color) as usize,
      Interface::as_raw(prev_color) as usize,
    );

    let recreate = self
      .sized
      .as_ref()
      .map(|s| {
        s.width != gb_w
          || s.height != gb_h
          || s.video_width != vw
          || s.video_height != vh
          || s.half_res_flow_scale != half_res_flow_scale
      })
      .unwrap_or(true);

    if recreate {
      if let Some(old) = self.sized.take() {
        self.retired_resources.push(old.coarse_mv_texture);
        self.retired_resources.push(old.motion_vectors_texture);
        self.retired_resources.push(old.depth_buffer_texture);
      }

      let uav = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
      let coarse_mv_texture = Self::create_tex2d(
        device,
        coarse_w,
        coarse_h,
        DXGI_FORMAT_R16G16_FLOAT,
        uav,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
      )?;
      let motion_vectors_texture = Self::create_tex2d(
        device,
        gb_w,
        gb_h,
        DXGI_FORMAT_R16G16_FLOAT,
        uav,
        SHADER_READ_STATES,
      )?;
      let depth_buffer_texture = Self::create_tex2d(
        device,
        gb_w,
        gb_h,
        DXGI_FORMAT_R32_FLOAT,
        uav,
        SHADER_READ_STATES,
      )?;

      if let Some(log) = logs.as_deref_mut() {
        let scale_lbl = if half_res_flow_scale {
          "50% Flow-Scale"
        } else {
          "100% Nativ"
        };
        log.info(format!(
          "[FrameForge][GBuffer] Zieltexturen ({scale_lbl}): {}x{} (Video {}x{}) | Coarse: {}x{}.",
          gb_w, gb_h, vw, vh, coarse_w, coarse_h
        ));
      }

      self.sized = Some(SizedGBufferTextures {
        coarse_mv_texture,
        motion_vectors_texture,
        depth_buffer_texture,
        bound_color_ptrs: (0, 0),
        video_width: vw,
        video_height: vh,
        width: gb_w,
        height: gb_h,
        coarse_width: coarse_w,
        coarse_height: coarse_h,
        half_res_flow_scale,
      });
    }

    if self
      .sized
      .as_ref()
      .map(|s| s.bound_color_ptrs != ptrs)
      .unwrap_or(false)
    {
      self.write_descriptors(device, curr_color, prev_color);
      if let Some(s) = &mut self.sized {
        s.bound_color_ptrs = ptrs;
      }
    }
    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  pub fn dispatch(
    &mut self,
    device: &ID3D12Device,
    cmd_list: &ID3D12GraphicsCommandList,
    frame_slot: usize,
    curr_color: &ID3D12Resource,
    prev_color: &ID3D12Resource,
    video_width: u32,
    video_height: u32,
    params: GBufferParams,
    half_res_flow_scale: bool,
    fast_search: bool,
  ) -> Result<()> {
    self.last_enabled = params.enabled;
    self.last_fast_search = fast_search;
    if !params.enabled || video_width == 0 || video_height == 0 {
      return Ok(());
    }

    self.ensure_resources(
      device,
      video_width,
      video_height,
      half_res_flow_scale,
      curr_color,
      prev_color,
      None,
    )?;

    let Some(sized) = self.sized.as_ref() else {
      return Ok(());
    };

    let slot = frame_slot % TIMER_SLOTS;
    let qbase = (slot * 2) as u32;
    let search_px_scale = (sized.width as f32 / (sized.video_width.max(1) as f32)).clamp(0.25_f32, 1.0_f32);
    let c = GBufferRootConstants {
      gb_width: sized.width,
      gb_height: sized.height,
      coarse_width: sized.coarse_width,
      coarse_height: sized.coarse_height,
      video_width: sized.video_width,
      video_height: sized.video_height,
      fast_search: if fast_search { 1_u32 } else { 0_u32 },
      expand_limited_rgb: if params.expand_limited_rgb {
        1.0_f32
      } else {
        0.0_f32
      },
      motion_sensitivity: params.motion_sensitivity.clamp(0.5_f32, 1.5_f32),
      edge_aegis_strength: params.edge_aegis_strength.clamp(0.0_f32, 2.0_f32),
      noise_deadzone: 0.048_f32,
      search_px_scale,
    };

    unsafe {
      cmd_list.SetDescriptorHeaps(&[Some(self.descriptor_heap.clone())]);
      cmd_list.SetComputeRootSignature(&self.root_signature);
      if let Some(t) = &mut self.gpu_timer {
        cmd_list.EndQuery(&t.query_heap, D3D12_QUERY_TYPE_TIMESTAMP, qbase);
      }
      cmd_list.ResourceBarrier(&[
        Self::transition(
          &sized.motion_vectors_texture,
          SHADER_READ_STATES,
          D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        ),
        Self::transition(
          &sized.depth_buffer_texture,
          SHADER_READ_STATES,
          D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        ),
      ]);
      cmd_list.SetComputeRoot32BitConstants(
        0,
        ROOT_CONSTANT_COUNT,
        &c as *const GBufferRootConstants as *const c_void,
        0,
      );

      cmd_list.SetPipelineState(&self.pso_coarse_flow);
      cmd_list.SetComputeRootDescriptorTable(1, self.gpu_handle(PASS1_SLOT));
      cmd_list.SetComputeRootDescriptorTable(2, self.gpu_handle(PASS1_SLOT + 3));
      cmd_list.Dispatch(sized.coarse_width.div_ceil(8), sized.coarse_height.div_ceil(8), 1);

      cmd_list.ResourceBarrier(&[
        Self::uav_barrier(&sized.coarse_mv_texture),
        Self::transition(
          &sized.coarse_mv_texture,
          D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
          D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        ),
      ]);

      cmd_list.SetPipelineState(&self.pso_refine_gbuffer);
      cmd_list.SetComputeRootDescriptorTable(1, self.gpu_handle(PASS2_SLOT));
      cmd_list.SetComputeRootDescriptorTable(2, self.gpu_handle(PASS2_SLOT + 3));
      cmd_list.Dispatch(sized.width.div_ceil(8), sized.height.div_ceil(8), 1);

      cmd_list.ResourceBarrier(&[
        Self::uav_barrier(&sized.motion_vectors_texture),
        Self::uav_barrier(&sized.depth_buffer_texture),
        Self::transition(
          &sized.coarse_mv_texture,
          D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
          D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        ),
        Self::transition(
          &sized.motion_vectors_texture,
          D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
          SHADER_READ_STATES,
        ),
        Self::transition(
          &sized.depth_buffer_texture,
          D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
          SHADER_READ_STATES,
        ),
      ]);

      if let Some(t) = &mut self.gpu_timer {
        cmd_list.EndQuery(&t.query_heap, D3D12_QUERY_TYPE_TIMESTAMP, qbase + 1);
        cmd_list.ResolveQueryData(
          &t.query_heap,
          D3D12_QUERY_TYPE_TIMESTAMP,
          qbase,
          2,
          &t.readback_buffer,
          (slot * size_of::<[u64; 2]>()) as u64,
        );
        t.slot_written[slot] = true;
      }
    }
    Ok(())
  }

  pub fn read_completed_gpu_timestamp(&mut self, completed_slot: usize) {
    self.retired_resources.clear();
    let Some(t) = &mut self.gpu_timer else {
      return;
    };
    let slot = completed_slot % TIMER_SLOTS;
    if !t.slot_written[slot] || t.freq_hz == 0 {
      return;
    }
    t.slot_written[slot] = false;
    let off = slot * size_of::<[u64; 2]>();
    let range = D3D12_RANGE {
      Begin: off,
      End: off + size_of::<[u64; 2]>(),
    };
    unsafe {
      let mut ptr: *mut c_void = std::ptr::null_mut();
      if t.readback_buffer.Map(0, Some(&range), Some(&mut ptr)).is_ok() && !ptr.is_null() {
        let ts = *((ptr as *const u8).add(off) as *const [u64; 2]);
        t.readback_buffer.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
        let ms = (((ts[1].saturating_sub(ts[0]) as f64) * 1000.0_f64 / (t.freq_hz as f64)) as f32)
          .clamp(0.0_f32, 100.0_f32);
        t.smoothed_ms = if !t.has_sample {
          t.has_sample = true;
          ms
        } else {
          t.smoothed_ms * 0.82_f32 + ms * 0.18_f32
        };
      }
    }
  }

  pub fn set_enabled_state(&mut self, enabled: bool) {
    self.last_enabled = enabled;
  }

  pub fn reset_signal(&mut self) {
    self.retired_resources.clear();
    if let Some(t) = &mut self.gpu_timer {
      t.slot_written = [false; TIMER_SLOTS];
    }
  }

  pub fn dimensions(&self) -> (u32, u32) {
    self
      .sized
      .as_ref()
      .map(|s| (s.width, s.height))
      .unwrap_or((0, 0))
  }

  pub fn motion_vectors_resource(&self) -> ID3D12Resource {
    if self.last_enabled {
      if let Some(s) = &self.sized {
        return s.motion_vectors_texture.clone();
      }
    }
    self.dummy_motion_vectors.clone()
  }

  pub fn depth_buffer_resource(&self) -> ID3D12Resource {
    if self.last_enabled {
      if let Some(s) = &self.sized {
        return s.depth_buffer_texture.clone();
      }
    }
    self.dummy_depth_buffer.clone()
  }

  pub fn last_gpu_time_ms(&self) -> f32 {
    if self.last_enabled {
      self
        .gpu_timer
        .as_ref()
        .map(|t| t.smoothed_ms)
        .unwrap_or(0.0_f32)
    } else {
      0.0_f32
    }
  }

  pub fn stats(&self, has_live_signal: bool) -> GBufferStats {
    let (gb_w, gb_h) = self.dimensions();
    let gpu_ms = self.last_gpu_time_ms();
    match &self.sized {
      Some(s) => {
        let active = self.last_enabled && has_live_signal;
        GBufferStats {
          enabled: self.last_enabled,
          active,
          width: gb_w,
          height: gb_h,
          video_width: s.video_width,
          video_height: s.video_height,
          half_res_flow_scale: s.half_res_flow_scale,
          fast_search: self.last_fast_search,
          gpu_compute_ms: if active { gpu_ms } else { 0.0_f32 },
        }
      }
      None => GBufferStats {
        enabled: self.last_enabled,
        fast_search: self.last_fast_search,
        ..Default::default()
      },
    }
  }

  fn write_descriptors(&self, dev: &ID3D12Device, curr: &ID3D12Resource, prev: &ID3D12Resource) {
    let Some(s) = self.sized.as_ref() else {
      return;
    };
    for base in [PASS1_SLOT, PASS2_SLOT] {
      self.write_srv(dev, base, curr, DXGI_FORMAT_R8G8B8A8_UNORM);
      self.write_srv(dev, base + 1, prev, DXGI_FORMAT_R8G8B8A8_UNORM);
    }
    self.write_srv(
      dev,
      PASS1_SLOT + 2,
      &self.dummy_coarse_srv,
      DXGI_FORMAT_R16G16_FLOAT,
    );
    self.write_uav(dev, PASS1_SLOT + 3, &s.coarse_mv_texture, DXGI_FORMAT_R16G16_FLOAT);
    self.write_uav(dev, PASS1_SLOT + 4, &self.scratch_uav_1x1, DXGI_FORMAT_R32_FLOAT);
    self.write_srv(dev, PASS2_SLOT + 2, &s.coarse_mv_texture, DXGI_FORMAT_R16G16_FLOAT);
    self.write_uav(
      dev,
      PASS2_SLOT + 3,
      &s.motion_vectors_texture,
      DXGI_FORMAT_R16G16_FLOAT,
    );
    self.write_uav(
      dev,
      PASS2_SLOT + 4,
      &s.depth_buffer_texture,
      DXGI_FORMAT_R32_FLOAT,
    );
  }

  fn write_srv(&self, dev: &ID3D12Device, slot: usize, res: &ID3D12Resource, fmt: DXGI_FORMAT) {
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
      dev.CreateShaderResourceView(res, Some(&desc), self.cpu_handle(slot));
    }
  }

  fn write_uav(&self, dev: &ID3D12Device, slot: usize, res: &ID3D12Resource, fmt: DXGI_FORMAT) {
    let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
      Format: fmt,
      ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
      Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
        Texture2D: D3D12_TEX2D_UAV {
          MipSlice: 0,
          PlaneSlice: 0,
        },
      },
    };
    unsafe {
      dev.CreateUnorderedAccessView(
        res,
        None::<&ID3D12Resource>,
        Some(&desc),
        self.cpu_handle(slot),
      );
    }
  }

  fn cpu_handle(&self, slot: usize) -> D3D12_CPU_DESCRIPTOR_HANDLE {
    D3D12_CPU_DESCRIPTOR_HANDLE {
      ptr: unsafe { self.descriptor_heap.GetCPUDescriptorHandleForHeapStart() }.ptr
        + slot * self.descriptor_size,
    }
  }

  fn gpu_handle(&self, slot: usize) -> D3D12_GPU_DESCRIPTOR_HANDLE {
    D3D12_GPU_DESCRIPTOR_HANDLE {
      ptr: unsafe { self.descriptor_heap.GetGPUDescriptorHandleForHeapStart() }.ptr
        + (slot * self.descriptor_size) as u64,
    }
  }

  fn create_root_signature(
    device: &ID3D12Device,
    logs: &mut LogStore,
  ) -> Result<ID3D12RootSignature> {
    let srv = [D3D12_DESCRIPTOR_RANGE {
      RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
      NumDescriptors: 3,
      BaseShaderRegister: 0,
      RegisterSpace: 0,
      OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    }];
    let uav = [D3D12_DESCRIPTOR_RANGE {
      RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
      NumDescriptors: 2,
      BaseShaderRegister: 0,
      RegisterSpace: 0,
      OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    }];
    let tbl = |r: &[D3D12_DESCRIPTOR_RANGE]| D3D12_ROOT_PARAMETER {
      ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
      Anonymous: D3D12_ROOT_PARAMETER_0 {
        DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
          NumDescriptorRanges: 1,
          pDescriptorRanges: r.as_ptr(),
        },
      },
      ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
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
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
      },
      tbl(&srv),
      tbl(&uav),
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
      ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
      NumParameters: 3,
      pParameters: params.as_ptr(),
      NumStaticSamplers: 1,
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
      logs.error(format!("[GBuffer] RootSignature-Fehler ({err}): {d}"));
      bail!("SyntheticGBuffer RootSignature fehlgeschlagen ({err}): {d}");
    }
    let blob = sig_blob.context("Kein RootSignature-Blob fuer SyntheticGBuffer")?;
    let bytes = unsafe {
      std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    };
    unsafe {
      device
        .CreateRootSignature(0, bytes)
        .context("CreateRootSignature (SyntheticGBuffer) fehlgeschlagen")
    }
  }

  fn create_compute_pso(
    device: &ID3D12Device,
    root_sig: &ID3D12RootSignature,
    entry: &[u8],
    logs: &mut LogStore,
  ) -> Result<ID3D12PipelineState> {
    let cs_blob = Self::compile_cs(entry, logs)?;
    let mut pso_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
      pRootSignature: ManuallyDrop::new(Some(root_sig.clone())),
      CS: D3D12_SHADER_BYTECODE {
        pShaderBytecode: unsafe { cs_blob.GetBufferPointer() },
        BytecodeLength: unsafe { cs_blob.GetBufferSize() },
      },
      NodeMask: 0,
      CachedPSO: Default::default(),
      Flags: D3D12_PIPELINE_STATE_FLAG_NONE,
    };
    let res = unsafe { device.CreateComputePipelineState(&pso_desc) };
    unsafe {
      ManuallyDrop::drop(&mut pso_desc.pRootSignature);
    }
    res.context("CreateComputePipelineState (SyntheticGBuffer) fehlgeschlagen")
  }

  fn compile_cs(entry_nul: &[u8], logs: &mut LogStore) -> Result<ID3DBlob> {
    let (mut code_blob, mut err_blob) = (None, None);
    let res = unsafe {
      D3DCompile(
        GBUFFER_HLSL_SOURCE.as_ptr() as *const c_void,
        GBUFFER_HLSL_SOURCE.len(),
        PCSTR(b"frameforge_gbuffer_cs.hlsl\0".as_ptr()),
        None,
        None::<&ID3DInclude>,
        PCSTR(entry_nul.as_ptr()),
        PCSTR(b"cs_5_0\0".as_ptr()),
        D3DCOMPILE_OPTIMIZATION_LEVEL3,
        0,
        &mut code_blob,
        Some(&mut err_blob),
      )
    };
    if let Err(err) = res {
      let d = Self::blob_str(err_blob.as_ref());
      logs.error(format!(
        "[GBuffer] D3DCompile '{}' fehlgeschlagen ({err}): {d}",
        String::from_utf8_lossy(entry_nul).trim_end_matches('\0')
      ));
      bail!("D3DCompile (SyntheticGBuffer) fehlgeschlagen ({err}): {d}");
    }
    code_blob.context("Kein Compute-Shader-Blob erhalten")
  }

  fn try_create_gpu_timer(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    logs: &mut LogStore,
  ) -> Option<GpuTimestampRing> {
    let freq_hz = unsafe { queue.GetTimestampFrequency().ok()? };
    if freq_hz == 0 {
      return None;
    }
    let qdesc = D3D12_QUERY_HEAP_DESC {
      Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
      Count: (TIMER_SLOTS * 2) as u32,
      NodeMask: 0,
    };
    let mut qheap: Option<ID3D12QueryHeap> = None;
    unsafe {
      device.CreateQueryHeap(&qdesc, &mut qheap).ok()?;
    }

    let rprops = D3D12_HEAP_PROPERTIES {
      Type: D3D12_HEAP_TYPE_READBACK,
      ..Default::default()
    };
    let rdesc = D3D12_RESOURCE_DESC {
      Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
      Alignment: 0,
      Width: (TIMER_SLOTS * size_of::<[u64; 2]>()) as u64,
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
    let mut rbuf: Option<ID3D12Resource> = None;
    unsafe {
      device
        .CreateCommittedResource(
          &rprops,
          D3D12_HEAP_FLAG_NONE,
          &rdesc,
          D3D12_RESOURCE_STATE_COPY_DEST,
          None,
          &mut rbuf,
        )
        .ok()?;
    }
    logs.info(format!(
      "[FrameForge][GBuffer] GPU-Timestamp-QueryHeap aktiv ({} Slots @ {:.2} MHz).",
      TIMER_SLOTS * 2,
      (freq_hz as f64) / 1e6
    ));
    Some(GpuTimestampRing {
      query_heap: qheap?,
      readback_buffer: rbuf?,
      freq_hz,
      slot_written: [false; TIMER_SLOTS],
      smoothed_ms: 0.0_f32,
      has_sample: false,
    })
  }

  fn create_tex2d(
    device: &ID3D12Device,
    w: u32,
    h: u32,
    fmt: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
  ) -> Result<ID3D12Resource> {
    let props = D3D12_HEAP_PROPERTIES {
      Type: D3D12_HEAP_TYPE_DEFAULT,
      ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
      Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
      Alignment: 0,
      Width: w.max(1) as u64,
      Height: h.max(1),
      DepthOrArraySize: 1,
      MipLevels: 1,
      Format: fmt,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
      Flags: flags,
    };
    let mut tex: Option<ID3D12Resource> = None;
    unsafe {
      device
        .CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut tex)
        .context("CreateCommittedResource (GBuffer) fehlgeschlagen")?;
    }
    tex.context("Keine GBuffer-Texture2D erhalten")
  }

  fn transition(
    res: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
  ) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
      Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
      Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
      Anonymous: D3D12_RESOURCE_BARRIER_0 {
        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
          pResource: unsafe { std::mem::transmute_copy(res) },
          Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
          StateBefore: before,
          StateAfter: after,
        }),
      },
    }
  }

  fn uav_barrier(res: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
      Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
      Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
      Anonymous: D3D12_RESOURCE_BARRIER_0 {
        UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
          pResource: unsafe { std::mem::transmute_copy(res) },
        }),
      },
    }
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
