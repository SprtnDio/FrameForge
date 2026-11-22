// FrameForge - Native C++17 DirectX 12 Bridge for Intel XeSS SDK v3.0.2
// Dynamically loads `libxess.dll` (XeSS 3 Native-AA / Super Resolution via XMX & DP4a),
// `libxell.dll` (Xe Low Latency frame markers), and `libxess_fg.dll` (XeFG hardware support probe
// without swapchain hijacking). Works across Intel Arc, Intel Iris Xe, AMD Radeon, and NVIDIA GPUs.

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif

#ifdef _WIN32
#include <windows.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#if defined(_MSC_VER)
#pragma comment(lib, "dxgi.lib")
#pragma comment(lib, "version.lib")
#endif
#endif

#include <cstdio>
#include <cstdint>
#include <cstring>
#include <new>
#include <string>
#include <type_traits>

#if defined(XESS3_SDK_AVAILABLE) && defined(__has_include)
#if __has_include("xess_d3d12.h")
#include "xess_d3d12.h"
#define FF_HAS_XESS_D3D12 1
#elif __has_include("xess/xess_d3d12.h")
#include "xess/xess_d3d12.h"
#define FF_HAS_XESS_D3D12 1
#endif

#if __has_include("xell_d3d12.h")
#include "xell_d3d12.h"
#define FF_HAS_XELL_D3D12 1
#elif __has_include("xell/xell_d3d12.h")
#include "xell/xell_d3d12.h"
#define FF_HAS_XELL_D3D12 1
#elif __has_include("xess/xell_d3d12.h")
#include "xess/xell_d3d12.h"
#define FF_HAS_XELL_D3D12 1
#endif

#if __has_include("xefg_swapchain_d3d12.h")
#include "xefg_swapchain_d3d12.h"
#define FF_HAS_XEFG_D3D12 1
#elif __has_include("xefg/xefg_swapchain_d3d12.h")
#include "xefg/xefg_swapchain_d3d12.h"
#define FF_HAS_XEFG_D3D12 1
#elif __has_include("xess/xefg_swapchain_d3d12.h")
#include "xess/xefg_swapchain_d3d12.h"
#define FF_HAS_XEFG_D3D12 1
#endif
#endif

extern "C" {

int32_t ff_xess3_create(
    void* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    const uint16_t* dll_dir_utf16,
    void** out_handle,
    char* out_version_utf8,
    uint32_t out_version_cap,
    int32_t* out_hw_fg_supported
);

int32_t ff_xess3_dispatch_sr_aa(
    void* handle,
    void* cmd_list,
    void* curr_color_res,
    void* depth_res,
    void* motion_vectors_res,
    void* out_reconstructed_res,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    uint64_t frame_id,
    int32_t reset
);

void ff_xess3_destroy(void* handle);

} // extern "C"

#if defined(_WIN32) && defined(FF_HAS_XESS_D3D12)

namespace {

static_assert(sizeof(wchar_t) == sizeof(uint16_t), "Windows UTF-16 wchar_t must be 16-bit");

using PfnXessGetVersion         = decltype(&xessGetVersion);
using PfnXessD3D12CreateContext = decltype(&xessD3D12CreateContext);
using PfnXessD3D12Init          = decltype(&xessD3D12Init);
using PfnXessD3D12Execute       = decltype(&xessD3D12Execute);
using PfnXessDestroyContext     = decltype(&xessDestroyContext);

#if defined(FF_HAS_XELL_D3D12)
using PfnXellD3D12CreateContext = decltype(&xellD3D12CreateContext);
using PfnXellDestroyContext     = decltype(&xellDestroyContext);
using PfnXellAddMarkerData      = decltype(&xellAddMarkerData);
#endif

#if defined(FF_HAS_XEFG_D3D12)
using PfnXefgSwapChainGetVersion         = decltype(&xefgSwapChainGetVersion);
using PfnXefgSwapChainD3D12CreateContext = decltype(&xefgSwapChainD3D12CreateContext);
using PfnXefgSwapChainDestroy            = decltype(&xefgSwapChainDestroy);

template <typename Fn>
struct FirstParamPointee;

template <typename Ret, typename Arg0>
struct FirstParamPointee<Ret (*)(Arg0*)> {
    using type = std::remove_cv_t<Arg0>;
};

template <typename Ret, typename Arg0>
struct FirstParamPointee<Ret (*)(Arg0*) noexcept> {
    using type = std::remove_cv_t<Arg0>;
};
#endif

struct FfXess3Context {
    HMODULE                   xess_module     = nullptr;
    HMODULE                   xell_module     = nullptr;
    HMODULE                   xefg_module     = nullptr;
    xess_context_handle_t     xess_context    = nullptr;
    PfnXessGetVersion         fn_get_version  = nullptr;
    PfnXessD3D12CreateContext fn_create_ctx   = nullptr;
    PfnXessD3D12Init          fn_init         = nullptr;
    PfnXessD3D12Execute       fn_execute      = nullptr;
    PfnXessDestroyContext     fn_destroy_ctx  = nullptr;
#if defined(FF_HAS_XELL_D3D12)
    xell_context_handle_t     xell_context    = nullptr;
    PfnXellD3D12CreateContext fn_xell_create  = nullptr;
    PfnXellDestroyContext     fn_xell_destroy = nullptr;
    PfnXellAddMarkerData      fn_xell_marker  = nullptr;
#endif
    uint32_t                  vendor_id       = 0;
    int32_t                   hw_fg_supported = 0;
};

inline void write_utf8_bounded(char* dst, uint32_t cap, const char* src) noexcept {
    if (dst == nullptr || cap == 0) {
        return;
    }
    if (src == nullptr) {
        dst[0] = '\0';
        return;
    }
    uint32_t i = 0;
    while (i + 1 < cap && src[i] != '\0') {
        dst[i] = src[i];
        ++i;
    }
    dst[i] = '\0';
}

std::wstring query_exe_directory() {
    wchar_t path_buf[MAX_PATH * 4] = {0};
    const DWORD len = GetModuleFileNameW(nullptr, path_buf, static_cast<DWORD>(MAX_PATH * 4 - 1));
    if (len == 0 || len >= MAX_PATH * 4 - 1) {
        return std::wstring();
    }
    std::wstring full_path(path_buf, len);
    const size_t sep = full_path.find_last_of(L"\\/");
    if (sep == std::wstring::npos) {
        return std::wstring();
    }
    return full_path.substr(0, sep);
}

std::wstring normalize_dir_utf16(const uint16_t* dll_dir_utf16) {
    std::wstring dir;
    if (dll_dir_utf16 != nullptr && dll_dir_utf16[0] != 0) {
        dir.assign(reinterpret_cast<const wchar_t*>(dll_dir_utf16));
    } else {
        dir = query_exe_directory();
    }
    while (!dir.empty() && (dir.back() == L'\\' || dir.back() == L'/')) {
        dir.pop_back();
    }
    return dir;
}

HMODULE load_sdk_dll_dynamic(const uint16_t* dll_dir_utf16, const wchar_t* dll_filename) {
    if (dll_filename == nullptr || dll_filename[0] == L'\0') {
        return nullptr;
    }

    const std::wstring base_dir = normalize_dir_utf16(dll_dir_utf16);
    if (!base_dir.empty()) {
        const std::wstring full_path = base_dir + L"\\" + dll_filename;
        HMODULE mod = LoadLibraryExW(full_path.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
        if (mod != nullptr) {
            return mod;
        }
    }

    const std::wstring exe_dir = query_exe_directory();
    if (!exe_dir.empty() && exe_dir != base_dir) {
        const std::wstring exe_path = exe_dir + L"\\" + dll_filename;
        HMODULE mod = LoadLibraryExW(exe_path.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
        if (mod != nullptr) {
            return mod;
        }
    }

    return nullptr;
}

template <typename FnPtr>
FnPtr resolve_proc(HMODULE module, const char* symbol_name) noexcept {
    if (module == nullptr || symbol_name == nullptr) {
        return nullptr;
    }
    FARPROC raw = GetProcAddress(module, symbol_name);
    return reinterpret_cast<FnPtr>(reinterpret_cast<void*>(raw));
}

uint32_t query_device_vendor_id(ID3D12Device* d3d_dev) noexcept {
    if (d3d_dev == nullptr) {
        return 0;
    }

    const LUID target_luid = d3d_dev->GetAdapterLuid();
    IDXGIFactory1* factory = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(&factory))) ||
        factory == nullptr) {
        return 0;
    }

    uint32_t found_vendor = 0;
    for (UINT idx = 0;; ++idx) {
        IDXGIAdapter1* adapter = nullptr;
        if (factory->EnumAdapters1(idx, &adapter) == DXGI_ERROR_NOT_FOUND || adapter == nullptr) {
            break;
        }
        DXGI_ADAPTER_DESC1 desc = {};
        if (SUCCEEDED(adapter->GetDesc1(&desc))) {
            if (desc.AdapterLuid.LowPart == target_luid.LowPart &&
                desc.AdapterLuid.HighPart == target_luid.HighPart) {
                found_vendor = static_cast<uint32_t>(desc.VendorId);
                adapter->Release();
                break;
            }
        }
        adapter->Release();
    }

    factory->Release();
    return found_vendor;
}

std::string format_u16_semver(uint16_t major, uint16_t minor, uint16_t patch) {
    char buf[48] = {0};
    std::snprintf(
        buf,
        sizeof(buf),
        "%u.%u.%u",
        static_cast<unsigned int>(major),
        static_cast<unsigned int>(minor),
        static_cast<unsigned int>(patch)
    );
    return std::string(buf);
}

} // namespace

extern "C" int32_t ff_xess3_create(
    void* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    const uint16_t* dll_dir_utf16,
    void** out_handle,
    char* out_version_utf8,
    uint32_t out_version_cap,
    int32_t* out_hw_fg_supported
) {
    if (out_handle != nullptr) {
        *out_handle = nullptr;
    }
    if (out_hw_fg_supported != nullptr) {
        *out_hw_fg_supported = 0;
    }
    write_utf8_bounded(out_version_utf8, out_version_cap, "");

    if (d3d12_device == nullptr ||
        out_handle == nullptr ||
        display_width < 64 ||
        display_height < 64 ||
        mv_width == 0 ||
        mv_height == 0 ||
        mv_width > display_width ||
        mv_height > display_height) {
        return -1;
    }

    HMODULE xess_mod = load_sdk_dll_dynamic(dll_dir_utf16, L"libxess.dll");
    if (xess_mod == nullptr) {
        return -2;
    }

    FfXess3Context* ctx = new (std::nothrow) FfXess3Context();
    if (ctx == nullptr) {
        FreeLibrary(xess_mod);
        return -3;
    }

    ctx->xess_module    = xess_mod;
    ctx->xell_module    = load_sdk_dll_dynamic(dll_dir_utf16, L"libxell.dll");
    ctx->xefg_module    = load_sdk_dll_dynamic(dll_dir_utf16, L"libxess_fg.dll");
    ctx->fn_get_version = resolve_proc<PfnXessGetVersion>(xess_mod, "xessGetVersion");
    ctx->fn_create_ctx  = resolve_proc<PfnXessD3D12CreateContext>(xess_mod, "xessD3D12CreateContext");
    ctx->fn_init        = resolve_proc<PfnXessD3D12Init>(xess_mod, "xessD3D12Init");
    ctx->fn_execute     = resolve_proc<PfnXessD3D12Execute>(xess_mod, "xessD3D12Execute");
    ctx->fn_destroy_ctx = resolve_proc<PfnXessDestroyContext>(xess_mod, "xessDestroyContext");

    if (ctx->fn_create_ctx == nullptr ||
        ctx->fn_init == nullptr ||
        ctx->fn_execute == nullptr ||
        ctx->fn_destroy_ctx == nullptr) {
        ff_xess3_destroy(ctx);
        return -4;
    }

    ID3D12Device* d3d_dev = static_cast<ID3D12Device*>(d3d12_device);
    ctx->vendor_id = query_device_vendor_id(d3d_dev);

    std::string xess_ver_str = "3.0.2";
    if (ctx->fn_get_version != nullptr) {
        xess_version_t ver = {};
        if (ctx->fn_get_version(&ver) == XESS_RESULT_SUCCESS &&
            (ver.major != 0 || ver.minor != 0 || ver.patch != 0)) {
            xess_ver_str = format_u16_semver(ver.major, ver.minor, ver.patch);
        }
    }

    // Optional XeFG hardware capability probe on `libxess_fg.dll`
    // Destroys the temporary test handle immediately so FrameForge's own swapchain is NEVER hijacked.
    std::string xefg_ver_str;
#if defined(FF_HAS_XEFG_D3D12)
    if (ctx->xefg_module != nullptr) {
        const auto fn_fg_ver = resolve_proc<PfnXefgSwapChainGetVersion>(
            ctx->xefg_module,
            "xefgSwapChainGetVersion"
        );
        if (fn_fg_ver != nullptr) {
            typename FirstParamPointee<PfnXefgSwapChainGetVersion>::type fg_ver = {};
            if (static_cast<int32_t>(fn_fg_ver(&fg_ver)) == 0 &&
                (fg_ver.major != 0 || fg_ver.minor != 0 || fg_ver.patch != 0)) {
                xefg_ver_str = format_u16_semver(fg_ver.major, fg_ver.minor, fg_ver.patch);
            }
        }

        const auto fn_fg_create = resolve_proc<PfnXefgSwapChainD3D12CreateContext>(
            ctx->xefg_module,
            "xefgSwapChainD3D12CreateContext"
        );
        const auto fn_fg_destroy = resolve_proc<PfnXefgSwapChainDestroy>(
            ctx->xefg_module,
            "xefgSwapChainDestroy"
        );

        if (fn_fg_create != nullptr && fn_fg_destroy != nullptr) {
            xefg_swapchain_handle_t test_fg_ctx = nullptr;
            const auto invoke_fg_probe = [](auto fn, ID3D12Device* dev, xefg_swapchain_handle_t* out_h) -> int32_t {
                if constexpr (std::is_invocable_v<decltype(fn), ID3D12Device*, xefg_swapchain_handle_t*>) {
                    return static_cast<int32_t>(fn(dev, out_h));
                } else {
                    (void)fn;
                    (void)dev;
                    (void)out_h;
                    return -1;
                }
            };
            if (invoke_fg_probe(fn_fg_create, d3d_dev, &test_fg_ctx) == 0 && test_fg_ctx != nullptr) {
                ctx->hw_fg_supported = 1;
            }
            if (test_fg_ctx != nullptr) {
                (void)fn_fg_destroy(test_fg_ctx);
                test_fg_ctx = nullptr;
            }
        }
    }
#endif

    // Optional XeLL low-latency marker context on `libxell.dll`
    bool xell_active = false;
#if defined(FF_HAS_XELL_D3D12)
    if (ctx->xell_module != nullptr) {
        ctx->fn_xell_create  = resolve_proc<PfnXellD3D12CreateContext>(ctx->xell_module, "xellD3D12CreateContext");
        ctx->fn_xell_destroy = resolve_proc<PfnXellDestroyContext>(ctx->xell_module, "xellDestroyContext");
        ctx->fn_xell_marker  = resolve_proc<PfnXellAddMarkerData>(ctx->xell_module, "xellAddMarkerData");

        if (ctx->fn_xell_create != nullptr && ctx->fn_xell_destroy != nullptr) {
            const auto invoke_ll_create = [](auto fn, ID3D12Device* dev, xell_context_handle_t* out_h) -> int32_t {
                if constexpr (std::is_invocable_v<decltype(fn), ID3D12Device*, xell_context_handle_t*>) {
                    return static_cast<int32_t>(fn(dev, out_h));
                } else {
                    (void)fn;
                    (void)dev;
                    (void)out_h;
                    return -1;
                }
            };
            if (invoke_ll_create(ctx->fn_xell_create, d3d_dev, &ctx->xell_context) == 0 &&
                ctx->xell_context != nullptr) {
                xell_active = true;
            } else {
                ctx->xell_context = nullptr;
            }
        }
    }
#endif

    // Create standalone texture-to-texture XeSS 3 Native-AA AI context (XMX on Intel Arc, DP4a universal)
    const xess_result_t create_rc = ctx->fn_create_ctx(d3d_dev, &ctx->xess_context);
    if (create_rc != XESS_RESULT_SUCCESS || ctx->xess_context == nullptr) {
        ctx->xess_context = nullptr;
        ff_xess3_destroy(ctx);
        return -10 - static_cast<int32_t>(create_rc);
    }

    uint32_t init_flags = static_cast<uint32_t>(XESS_INIT_FLAG_INVERTED_DEPTH);
    if (mv_width == display_width && mv_height == display_height) {
        init_flags |= static_cast<uint32_t>(XESS_INIT_FLAG_HIGH_RES_MV);
    }

    xess_d3d12_init_params_t init_params = {};
    init_params.outputResolution.x = display_width;
    init_params.outputResolution.y = display_height;
    init_params.qualitySetting = XESS_QUALITY_SETTING_AA;
    init_params.initFlags = static_cast<decltype(init_params.initFlags)>(init_flags);
    init_params.creationNodeMask = 1;
    init_params.visibleNodeMask = 1;

    const xess_result_t init_rc = ctx->fn_init(ctx->xess_context, &init_params);
    if (init_rc != XESS_RESULT_SUCCESS) {
        ff_xess3_destroy(ctx);
        return -30 - static_cast<int32_t>(init_rc);
    }

    if (out_hw_fg_supported != nullptr) {
        *out_hw_fg_supported = ctx->hw_fg_supported;
    }

    const std::string& display_ver = (!xefg_ver_str.empty() && xefg_ver_str[0] == '3')
        ? xefg_ver_str
        : xess_ver_str;

    std::string status_label;
    if (ctx->hw_fg_supported != 0) {
        status_label = "XeSS " + display_ver + " (XMX/DP4a + XeFG)";
    } else if (ctx->vendor_id == 0x8086u && xell_active) {
        status_label = "XeSS " + display_ver + " (XMX/DP4a + XeLL)";
    } else if (ctx->vendor_id == 0x8086u) {
        status_label = "XeSS " + display_ver + " (Intel XMX/DP4a aktiv)";
    } else {
        status_label = "XeSS " + display_ver + " (DP4a KI aktiv)";
    }

    write_utf8_bounded(out_version_utf8, out_version_cap, status_label.c_str());

    *out_handle = ctx;
    return 0;
}

extern "C" int32_t ff_xess3_dispatch_sr_aa(
    void* handle,
    void* cmd_list,
    void* curr_color_res,
    void* depth_res,
    void* motion_vectors_res,
    void* out_reconstructed_res,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    uint64_t frame_id,
    int32_t reset
) {
    if (handle == nullptr ||
        cmd_list == nullptr ||
        curr_color_res == nullptr ||
        depth_res == nullptr ||
        motion_vectors_res == nullptr ||
        out_reconstructed_res == nullptr ||
        display_width == 0 ||
        display_height == 0 ||
        mv_width == 0 ||
        mv_height == 0 ||
        mv_width > display_width ||
        mv_height > display_height) {
        return -1;
    }

    FfXess3Context* ctx = static_cast<FfXess3Context*>(handle);
    if (ctx->xess_context == nullptr || ctx->fn_execute == nullptr) {
        return -2;
    }

#if defined(FF_HAS_XELL_D3D12)
    if (ctx->xell_context != nullptr && ctx->fn_xell_marker != nullptr) {
        const uint32_t marker_frame_id = static_cast<uint32_t>(frame_id);
        (void)ctx->fn_xell_marker(ctx->xell_context, marker_frame_id, XELL_SIMULATION_START);
    }
#else
    (void)frame_id;
#endif

    xess_d3d12_execute_params_t exec_params = {};
    exec_params.pColorTexture = static_cast<ID3D12Resource*>(curr_color_res);
    exec_params.pVelocityTexture = static_cast<ID3D12Resource*>(motion_vectors_res);
    exec_params.pDepthTexture = static_cast<ID3D12Resource*>(depth_res);
    exec_params.pOutputTexture = static_cast<ID3D12Resource*>(out_reconstructed_res);
    exec_params.jitterOffsetX = 0.0f;
    exec_params.jitterOffsetY = 0.0f;
    exec_params.exposureScale = 1.0f;
    exec_params.resetHistory = (reset != 0) ? 1u : 0u;
    exec_params.inputWidth = display_width;
    exec_params.inputHeight = display_height;

    const xess_result_t exec_rc = ctx->fn_execute(
        ctx->xess_context,
        static_cast<ID3D12GraphicsCommandList*>(cmd_list),
        &exec_params
    );
    if (exec_rc != XESS_RESULT_SUCCESS) {
        return -50 - static_cast<int32_t>(exec_rc);
    }

    return 0;
}

extern "C" void ff_xess3_destroy(void* handle) {
    if (handle == nullptr) {
        return;
    }

    FfXess3Context* ctx = static_cast<FfXess3Context*>(handle);
    if (ctx->xess_context != nullptr && ctx->fn_destroy_ctx != nullptr) {
        (void)ctx->fn_destroy_ctx(ctx->xess_context);
        ctx->xess_context = nullptr;
    }

#if defined(FF_HAS_XELL_D3D12)
    if (ctx->xell_context != nullptr && ctx->fn_xell_destroy != nullptr) {
        (void)ctx->fn_xell_destroy(ctx->xell_context);
        ctx->xell_context = nullptr;
    }
    ctx->fn_xell_create  = nullptr;
    ctx->fn_xell_destroy = nullptr;
    ctx->fn_xell_marker  = nullptr;
#endif

    ctx->fn_get_version = nullptr;
    ctx->fn_create_ctx  = nullptr;
    ctx->fn_init        = nullptr;
    ctx->fn_execute     = nullptr;
    ctx->fn_destroy_ctx = nullptr;

    if (ctx->xefg_module != nullptr) {
        FreeLibrary(ctx->xefg_module);
        ctx->xefg_module = nullptr;
    }
    if (ctx->xell_module != nullptr) {
        FreeLibrary(ctx->xell_module);
        ctx->xell_module = nullptr;
    }
    if (ctx->xess_module != nullptr) {
        FreeLibrary(ctx->xess_module);
        ctx->xess_module = nullptr;
    }

    delete ctx;
}

#else // !defined(_WIN32) || !defined(FF_HAS_XESS_D3D12)

extern "C" int32_t ff_xess3_create(
    void* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    const uint16_t* dll_dir_utf16,
    void** out_handle,
    char* out_version_utf8,
    uint32_t out_version_cap,
    int32_t* out_hw_fg_supported
) {
    (void)d3d12_device;
    (void)display_width;
    (void)display_height;
    (void)mv_width;
    (void)mv_height;
    (void)dll_dir_utf16;
    if (out_handle != nullptr) {
        *out_handle = nullptr;
    }
    if (out_hw_fg_supported != nullptr) {
        *out_hw_fg_supported = 0;
    }
    if (out_version_utf8 != nullptr && out_version_cap > 0) {
        out_version_utf8[0] = '\0';
    }
    return -100;
}

extern "C" int32_t ff_xess3_dispatch_sr_aa(
    void* handle,
    void* cmd_list,
    void* curr_color_res,
    void* depth_res,
    void* motion_vectors_res,
    void* out_reconstructed_res,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    uint64_t frame_id,
    int32_t reset
) {
    (void)handle;
    (void)cmd_list;
    (void)curr_color_res;
    (void)depth_res;
    (void)motion_vectors_res;
    (void)out_reconstructed_res;
    (void)display_width;
    (void)display_height;
    (void)mv_width;
    (void)mv_height;
    (void)frame_id;
    (void)reset;
    return -100;
}

extern "C" void ff_xess3_destroy(void* handle) {
    (void)handle;
}

#endif
