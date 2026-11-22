// FrameForge - Native C++17 DirectX 12 Bridge for AMD FidelityFX SDK v2.4.0
// (FSR 4.1.2 Upscaler Suite + FSR FG 4.0.0 ML with automatic fallback to universal FSR FG 3.1.6 Compute)
// Dynamically loads `amd_fidelityfx_loader_dx12.dll` (auto-loading `amd_fidelityfx_framegeneration_dx12.dll`
// via LOAD_WITH_ALTERED_SEARCH_PATH) or `amd_fidelityfx_dx12.dll` as fallback, probes FSR FG 4.x ML first
// and seamlessly falls back to universal "3.1." Compute on non-AMD-ML GPUs (Intel Iris Xe / Arc / NVIDIA / RDNA2-3).

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif

#ifdef _WIN32
#include <windows.h>
#include <d3d12.h>
#endif

#include <cstdint>
#include <cstring>
#include <new>
#include <string>
#include <vector>

#ifdef FFX_SDK_AVAILABLE
#include "ffx_api.h"
#include "dx12/ffx_api_dx12.h"
#include "ffx_framegeneration.h"
#if defined(__has_include)
#if __has_include("ffx_upscale.h")
#include "ffx_upscale.h"
#endif
#endif
#endif

extern "C" {

int32_t ff_fsr3_create(
    void* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    const uint16_t* dll_dir_utf16,
    void** out_handle,
    char* out_version_utf8,
    uint32_t out_version_cap
);

int32_t ff_fsr3_dispatch(
    void* handle,
    void* cmd_list,
    void* curr_color_res,
    void* depth_res,
    void* motion_vectors_res,
    void* out_interpolated_res,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    float delta_time_ms,
    uint64_t frame_id,
    int32_t reset
);

void ff_fsr3_destroy(void* handle);

} // extern "C"

#if defined(_WIN32) && defined(FFX_SDK_AVAILABLE)

namespace {

static_assert(sizeof(wchar_t) == sizeof(uint16_t), "Windows UTF-16 wchar_t must be 16-bit");

struct FfFsr3Context {
    HMODULE              dll_module      = nullptr;
    ffxContext           fg_context      = nullptr;
    PfnFfxCreateContext  create_context  = nullptr;
    PfnFfxDestroyContext destroy_context = nullptr;
    PfnFfxConfigure      configure       = nullptr;
    PfnFfxQuery          query           = nullptr;
    PfnFfxDispatch       dispatch        = nullptr;
};

struct FfxFgProviderSelection {
    bool        has_v4_ml   = false;
    uint64_t    v4_id       = 0;
    std::string v4_semver   = {};
    bool        has_v31     = false;
    uint64_t    v31_id      = 0;
    std::string v31_semver  = {};
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

inline bool is_ascii_digit(char c) noexcept {
    return c >= '0' && c <= '9';
}

// Extracts the leading semantic version token (e.g. "4.0.0" or "3.1.6") from a provider name
// without confusing patch digits like "3.1.4" with major version "4.".
std::string extract_semver_token(const char* provider_name) {
    if (provider_name == nullptr) {
        return std::string();
    }
    for (size_t i = 0; provider_name[i] != '\0'; ++i) {
        if (!is_ascii_digit(provider_name[i])) {
            continue;
        }
        size_t j = i;
        bool saw_dot = false;
        while (provider_name[j] != '\0' &&
               (is_ascii_digit(provider_name[j]) || provider_name[j] == '.')) {
            if (provider_name[j] == '.') {
                saw_dot = true;
            }
            ++j;
        }
        if (saw_dot && j > i + 2) {
            return std::string(provider_name + i, j - i);
        }
        i = (j > 0) ? (j - 1) : i;
    }
    return std::string();
}

inline bool starts_with_prefix(const std::string& text, const char* prefix) noexcept {
    const size_t prefix_len = std::strlen(prefix);
    return text.size() >= prefix_len && std::strncmp(text.c_str(), prefix, prefix_len) == 0;
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

HMODULE load_ffx_runtime_dll(const uint16_t* dll_dir_utf16) {
    const std::wstring base_dir = normalize_dir_utf16(dll_dir_utf16);

    if (!base_dir.empty()) {
        const std::wstring loader_path = base_dir + L"\\amd_fidelityfx_loader_dx12.dll";
        HMODULE mod = LoadLibraryExW(loader_path.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
        if (mod != nullptr) {
            return mod;
        }

        const std::wstring monolithic_path = base_dir + L"\\amd_fidelityfx_dx12.dll";
        mod = LoadLibraryExW(monolithic_path.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
        if (mod != nullptr) {
            return mod;
        }
    }

    const std::wstring exe_dir = query_exe_directory();
    if (!exe_dir.empty() && exe_dir != base_dir) {
        const std::wstring exe_loader = exe_dir + L"\\amd_fidelityfx_loader_dx12.dll";
        HMODULE mod = LoadLibraryExW(exe_loader.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
        if (mod != nullptr) {
            return mod;
        }

        const std::wstring exe_monolithic = exe_dir + L"\\amd_fidelityfx_dx12.dll";
        mod = LoadLibraryExW(exe_monolithic.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
        if (mod != nullptr) {
            return mod;
        }
    }

    HMODULE mod = LoadLibraryW(L"amd_fidelityfx_loader_dx12.dll");
    if (mod != nullptr) {
        return mod;
    }
    return LoadLibraryW(L"amd_fidelityfx_dx12.dll");
}

bool probe_fsr41_upscaler_dll(const uint16_t* dll_dir_utf16) {
    const std::wstring base_dir = normalize_dir_utf16(dll_dir_utf16);
    if (!base_dir.empty()) {
        const std::wstring upscaler_path = base_dir + L"\\amd_fidelityfx_upscaler_dx12.dll";
        HMODULE mod = LoadLibraryExW(upscaler_path.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
        if (mod != nullptr) {
            FreeLibrary(mod);
            return true;
        }
    }

    const std::wstring exe_dir = query_exe_directory();
    if (!exe_dir.empty() && exe_dir != base_dir) {
        const std::wstring exe_upscaler = exe_dir + L"\\amd_fidelityfx_upscaler_dx12.dll";
        HMODULE mod = LoadLibraryExW(exe_upscaler.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
        if (mod != nullptr) {
            FreeLibrary(mod);
            return true;
        }
    }

    HMODULE mod = LoadLibraryW(L"amd_fidelityfx_upscaler_dx12.dll");
    if (mod != nullptr) {
        FreeLibrary(mod);
        return true;
    }
    return false;
}

template <typename FnPtr>
FnPtr resolve_proc(HMODULE module, const char* symbol_name) noexcept {
    FARPROC raw = GetProcAddress(module, symbol_name);
    return reinterpret_cast<FnPtr>(reinterpret_cast<void*>(raw));
}

FfxFgProviderSelection query_fg_providers(PfnFfxQuery fn_query, void* d3d12_device) {
    FfxFgProviderSelection sel = {};
    if (fn_query == nullptr) {
        return sel;
    }

    uint64_t version_count = 0;
    ffxQueryDescGetVersions count_query = {};
    count_query.header.type = FFX_API_QUERY_DESC_TYPE_GET_VERSIONS;
    count_query.header.pNext = nullptr;
    count_query.createDescType = FFX_API_CREATE_CONTEXT_DESC_TYPE_FRAMEGENERATION;
    count_query.device = d3d12_device;
    count_query.outputCount = &version_count;
    count_query.versionIds = nullptr;
    count_query.versionNames = nullptr;

    if (fn_query(nullptr, &count_query.header) != FFX_API_RETURN_OK || version_count == 0) {
        return sel;
    }

    const size_t cap = static_cast<size_t>(version_count);
    std::vector<uint64_t> version_ids(cap, 0);
    std::vector<const char*> version_names(cap, nullptr);

    ffxQueryDescGetVersions list_query = {};
    list_query.header.type = FFX_API_QUERY_DESC_TYPE_GET_VERSIONS;
    list_query.header.pNext = nullptr;
    list_query.createDescType = FFX_API_CREATE_CONTEXT_DESC_TYPE_FRAMEGENERATION;
    list_query.device = d3d12_device;
    list_query.outputCount = &version_count;
    list_query.versionIds = version_ids.data();
    list_query.versionNames = version_names.data();

    if (fn_query(nullptr, &list_query.header) != FFX_API_RETURN_OK || version_count == 0) {
        return sel;
    }

    const size_t filled = (static_cast<size_t>(version_count) < cap)
        ? static_cast<size_t>(version_count)
        : cap;

    for (size_t i = 0; i < filled; ++i) {
        if (version_names[i] == nullptr) {
            continue;
        }
        const std::string semver = extract_semver_token(version_names[i]);
        if (!sel.has_v4_ml && starts_with_prefix(semver, "4.")) {
            sel.has_v4_ml = true;
            sel.v4_id = version_ids[i];
            sel.v4_semver = semver;
        } else if (!sel.has_v31 && starts_with_prefix(semver, "3.1.")) {
            sel.has_v31 = true;
            sel.v31_id = version_ids[i];
            sel.v31_semver = semver;
        }
    }

    return sel;
}

ffxReturnCode_t try_create_fg_context(
    FfFsr3Context* ctx,
    ID3D12Device* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t fg_create_flags,
    bool use_override,
    uint64_t override_version_id
) {
    ctx->fg_context = nullptr;

    ffxOverrideVersion override_desc = {};
    override_desc.header.type = FFX_API_DESC_TYPE_OVERRIDE_VERSION;
    override_desc.header.pNext = nullptr;
    override_desc.versionId = override_version_id;

    ffxCreateBackendDX12Desc dx12_backend_desc = {};
    dx12_backend_desc.header.type = FFX_API_CREATE_CONTEXT_DESC_TYPE_BACKEND_DX12;
    dx12_backend_desc.header.pNext = use_override ? &override_desc.header : nullptr;
    dx12_backend_desc.device = d3d12_device;

    ffxCreateContextDescFrameGenerationVersion fg_version_desc = {};
    fg_version_desc.header.type = FFX_API_CREATE_CONTEXT_DESC_TYPE_FRAMEGENERATION_VERSION;
    fg_version_desc.header.pNext = &dx12_backend_desc.header;
    fg_version_desc.version = FFX_FRAMEGENERATION_VERSION;

    ffxCreateContextDescFrameGeneration fg_create_desc = {};
    fg_create_desc.header.type = FFX_API_CREATE_CONTEXT_DESC_TYPE_FRAMEGENERATION;
    fg_create_desc.header.pNext = &fg_version_desc.header;
    fg_create_desc.flags = fg_create_flags;
    fg_create_desc.displaySize.width = display_width;
    fg_create_desc.displaySize.height = display_height;
    fg_create_desc.maxRenderSize.width = display_width;
    fg_create_desc.maxRenderSize.height = display_height;
    fg_create_desc.backBufferFormat = ffxApiGetSurfaceFormatDX12(DXGI_FORMAT_R8G8B8A8_UNORM);

    return ctx->create_context(&ctx->fg_context, &fg_create_desc.header, nullptr);
}

ffxReturnCode_t dispatch_prepare_legacy_fallback(
    PfnFfxDispatch fn_dispatch,
    ffxContext* fg_context,
    ID3D12GraphicsCommandList* d3d_cmd_list,
    const FfxApiResource& depth_api_res,
    const FfxApiResource& mv_api_res,
    uint32_t mv_width,
    uint32_t mv_height,
    float clamped_dt_ms,
    uint64_t frame_id,
    bool is_reset
) {
#if defined(_MSC_VER)
#pragma warning(push)
#pragma warning(disable : 4996)
#elif defined(__clang__) || defined(__GNUC__)
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wdeprecated-declarations"
#endif

    ffxDispatchDescFrameGenerationPrepareCameraInfo cam_info = {};
    cam_info.header.type = FFX_API_DISPATCH_DESC_TYPE_FRAMEGENERATION_PREPARE_CAMERAINFO;
    cam_info.header.pNext = nullptr;
    cam_info.cameraPosition[0] = 0.0f;
    cam_info.cameraPosition[1] = 0.0f;
    cam_info.cameraPosition[2] = 0.0f;
    cam_info.cameraUp[0] = 0.0f;
    cam_info.cameraUp[1] = 1.0f;
    cam_info.cameraUp[2] = 0.0f;
    cam_info.cameraRight[0] = 1.0f;
    cam_info.cameraRight[1] = 0.0f;
    cam_info.cameraRight[2] = 0.0f;
    cam_info.cameraForward[0] = 0.0f;
    cam_info.cameraForward[1] = 0.0f;
    cam_info.cameraForward[2] = 1.0f;

    ffxDispatchDescFrameGenerationPrepare prep_v1 = {};
    prep_v1.header.type = FFX_API_DISPATCH_DESC_TYPE_FRAMEGENERATION_PREPARE;
    prep_v1.header.pNext = &cam_info.header;
    prep_v1.frameID = frame_id;
    prep_v1.flags = FFX_FRAMEGENERATION_FLAG_NO_SWAPCHAIN_CONTEXT_NOTIFY;
    prep_v1.commandList = d3d_cmd_list;
    prep_v1.renderSize.width = mv_width;
    prep_v1.renderSize.height = mv_height;
    prep_v1.jitterOffset.x = 0.0f;
    prep_v1.jitterOffset.y = 0.0f;
    prep_v1.motionVectorScale.x = 1.0f;
    prep_v1.motionVectorScale.y = 1.0f;
    prep_v1.frameTimeDelta = clamped_dt_ms;
    prep_v1.unused_reset = is_reset;
    prep_v1.cameraNear = 3.402823466e+38F;
    prep_v1.cameraFar = 0.1f;
    prep_v1.cameraFovAngleVertical = 1.04719755f;
    prep_v1.viewSpaceToMetersFactor = 1.0f;
    prep_v1.depth = depth_api_res;
    prep_v1.motionVectors = mv_api_res;

    const ffxReturnCode_t rc = fn_dispatch(fg_context, &prep_v1.header);

#if defined(_MSC_VER)
#pragma warning(pop)
#elif defined(__clang__) || defined(__GNUC__)
#pragma GCC diagnostic pop
#endif

    return rc;
}

} // namespace

extern "C" int32_t ff_fsr3_create(
    void* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    const uint16_t* dll_dir_utf16,
    void** out_handle,
    char* out_version_utf8,
    uint32_t out_version_cap
) {
    if (out_handle != nullptr) {
        *out_handle = nullptr;
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

    HMODULE dll_mod = load_ffx_runtime_dll(dll_dir_utf16);
    if (dll_mod == nullptr) {
        return -2;
    }

    FfFsr3Context* ctx = new (std::nothrow) FfFsr3Context();
    if (ctx == nullptr) {
        FreeLibrary(dll_mod);
        return -3;
    }

    ctx->dll_module      = dll_mod;
    ctx->create_context  = resolve_proc<PfnFfxCreateContext>(dll_mod, "ffxCreateContext");
    ctx->destroy_context = resolve_proc<PfnFfxDestroyContext>(dll_mod, "ffxDestroyContext");
    ctx->configure       = resolve_proc<PfnFfxConfigure>(dll_mod, "ffxConfigure");
    ctx->query           = resolve_proc<PfnFfxQuery>(dll_mod, "ffxQuery");
    ctx->dispatch        = resolve_proc<PfnFfxDispatch>(dll_mod, "ffxDispatch");

    if (ctx->create_context == nullptr ||
        ctx->destroy_context == nullptr ||
        ctx->configure == nullptr ||
        ctx->query == nullptr ||
        ctx->dispatch == nullptr) {
        ff_fsr3_destroy(ctx);
        return -4;
    }

    uint32_t fg_create_flags =
        FFX_FRAMEGENERATION_ENABLE_DEPTH_INVERTED |
        FFX_FRAMEGENERATION_ENABLE_DEPTH_INFINITE;
    if (mv_width == display_width && mv_height == display_height) {
        fg_create_flags |= FFX_FRAMEGENERATION_ENABLE_DISPLAY_RESOLUTION_MOTION_VECTORS;
    }

    ID3D12Device* d3d_dev = static_cast<ID3D12Device*>(d3d12_device);
    const FfxFgProviderSelection providers = query_fg_providers(ctx->query, d3d12_device);

    ffxReturnCode_t create_rc = FFX_API_RETURN_ERROR;
    std::string fallback_semver;
    bool active_v4_ml = false;

    // 1. Prioritaet: FSR FG 4.x ML-Provider (RDNA4 / kompatible AMD-ML-Hardware in SDK 2.4.0)
    if (providers.has_v4_ml) {
        create_rc = try_create_fg_context(
            ctx,
            d3d_dev,
            display_width,
            display_height,
            fg_create_flags,
            true,
            providers.v4_id
        );
        if (create_rc == FFX_API_RETURN_OK && ctx->fg_context != nullptr) {
            active_v4_ml = true;
            fallback_semver = providers.v4_semver;
        } else {
            ctx->fg_context = nullptr;
        }
    }

    // 2. Auto-Fallback: Universeller FSR FG 3.1.x Compute-Provider (Intel Iris Xe / Arc / NVIDIA / RDNA2-3)
    if (ctx->fg_context == nullptr && providers.has_v31) {
        create_rc = try_create_fg_context(
            ctx,
            d3d_dev,
            display_width,
            display_height,
            fg_create_flags,
            true,
            providers.v31_id
        );
        if (create_rc == FFX_API_RETURN_OK && ctx->fg_context != nullptr) {
            fallback_semver = providers.v31_semver;
        } else {
            ctx->fg_context = nullptr;
        }
    }

    // 3. Sicherheitsnetz: Ungefilterter Default-Provider des Loaders
    if (ctx->fg_context == nullptr) {
        create_rc = try_create_fg_context(
            ctx,
            d3d_dev,
            display_width,
            display_height,
            fg_create_flags,
            false,
            0
        );
    }

    if (create_rc != FFX_API_RETURN_OK || ctx->fg_context == nullptr) {
        ctx->fg_context = nullptr;
        ff_fsr3_destroy(ctx);
        return -10 - static_cast<int32_t>(create_rc);
    }

    const bool has_fsr41_upscaler_suite = probe_fsr41_upscaler_dll(dll_dir_utf16);

    ffxQueryGetProviderVersion prov_ver = {};
    prov_ver.header.type = FFX_API_QUERY_DESC_TYPE_GET_PROVIDER_VERSION;
    prov_ver.header.pNext = nullptr;
    prov_ver.versionId = 0;
    prov_ver.versionName = nullptr;

    std::string active_semver = fallback_semver;
    if (ctx->query(&ctx->fg_context, &prov_ver.header) == FFX_API_RETURN_OK &&
        prov_ver.versionName != nullptr &&
        prov_ver.versionName[0] != '\0') {
        const std::string queried_semver = extract_semver_token(prov_ver.versionName);
        if (!queried_semver.empty()) {
            active_semver = queried_semver;
        } else if (active_semver.empty()) {
            active_semver = prov_ver.versionName;
        }
    }

    if (starts_with_prefix(active_semver, "4.")) {
        active_v4_ml = true;
    }
    if (active_semver.empty()) {
        active_semver = active_v4_ml ? "4.0.0" : "3.1.6";
    }

    std::string formatted_label;
    if (active_v4_ml) {
        formatted_label = "FSR FG " + active_semver + " ML (SDK 2.4)";
    } else if (has_fsr41_upscaler_suite) {
        formatted_label = "FSR FG " + active_semver + " (SDK 2.4 / FSR 4.1 Suite)";
    } else {
        formatted_label = "FSR FG " + active_semver + " (SDK 2.4)";
    }

    write_utf8_bounded(out_version_utf8, out_version_cap, formatted_label.c_str());

    *out_handle = ctx;
    return 0;
}

extern "C" int32_t ff_fsr3_dispatch(
    void* handle,
    void* cmd_list,
    void* curr_color_res,
    void* depth_res,
    void* motion_vectors_res,
    void* out_interpolated_res,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    float delta_time_ms,
    uint64_t frame_id,
    int32_t reset
) {
    if (handle == nullptr ||
        cmd_list == nullptr ||
        curr_color_res == nullptr ||
        depth_res == nullptr ||
        motion_vectors_res == nullptr ||
        out_interpolated_res == nullptr ||
        display_width == 0 ||
        display_height == 0 ||
        mv_width == 0 ||
        mv_height == 0 ||
        mv_width > display_width ||
        mv_height > display_height) {
        return -1;
    }

    FfFsr3Context* ctx = static_cast<FfFsr3Context*>(handle);
    if (ctx->fg_context == nullptr || ctx->configure == nullptr || ctx->dispatch == nullptr) {
        return -2;
    }

    ID3D12GraphicsCommandList* d3d_cmd_list = static_cast<ID3D12GraphicsCommandList*>(cmd_list);
    const bool is_reset = (reset != 0);
    const float clamped_dt_ms = (delta_time_ms < 1.0f) ? 1.0f : delta_time_ms;

    // Step A: Standalone FrameGeneration configuration (no swapchain hijacking)
    ffxConfigureDescFrameGeneration cfg_desc = {};
    cfg_desc.header.type = FFX_API_CONFIGURE_DESC_TYPE_FRAMEGENERATION;
    cfg_desc.header.pNext = nullptr;
    cfg_desc.swapChain = nullptr;
    cfg_desc.presentCallback = nullptr;
    cfg_desc.presentCallbackUserContext = nullptr;
    cfg_desc.frameGenerationCallback = nullptr;
    cfg_desc.frameGenerationCallbackUserContext = nullptr;
    cfg_desc.frameGenerationEnabled = true;
    cfg_desc.allowAsyncWorkloads = false;
    cfg_desc.HUDLessColor = FfxApiResource{};
    cfg_desc.flags = FFX_FRAMEGENERATION_FLAG_NO_SWAPCHAIN_CONTEXT_NOTIFY;
    cfg_desc.onlyPresentGenerated = false;
    cfg_desc.generationRect.left = 0;
    cfg_desc.generationRect.top = 0;
    cfg_desc.generationRect.width = static_cast<int32_t>(display_width);
    cfg_desc.generationRect.height = static_cast<int32_t>(display_height);
    cfg_desc.frameID = frame_id;

    const ffxReturnCode_t cfg_rc = ctx->configure(&ctx->fg_context, &cfg_desc.header);
    if (cfg_rc != FFX_API_RETURN_OK) {
        return -20 - static_cast<int32_t>(cfg_rc);
    }

    // Step B: FrameGeneration PrepareV2 (renderSize = mv_width x mv_height, motionVectorScale = 1.0)
    const FfxApiResource depth_api_res = ffxApiGetResourceDX12(
        static_cast<ID3D12Resource*>(depth_res),
        FFX_API_RESOURCE_STATE_PIXEL_COMPUTE_READ
    );
    const FfxApiResource mv_api_res = ffxApiGetResourceDX12(
        static_cast<ID3D12Resource*>(motion_vectors_res),
        FFX_API_RESOURCE_STATE_PIXEL_COMPUTE_READ
    );

    ffxDispatchDescFrameGenerationPrepareV2 prep_v2 = {};
    prep_v2.header.type = FFX_API_DISPATCH_DESC_TYPE_FRAMEGENERATION_PREPARE_V2;
    prep_v2.header.pNext = nullptr;
    prep_v2.frameID = frame_id;
    prep_v2.flags = FFX_FRAMEGENERATION_FLAG_NO_SWAPCHAIN_CONTEXT_NOTIFY;
    prep_v2.commandList = d3d_cmd_list;
    prep_v2.renderSize.width = mv_width;
    prep_v2.renderSize.height = mv_height;
    prep_v2.jitterOffset.x = 0.0f;
    prep_v2.jitterOffset.y = 0.0f;
    prep_v2.motionVectorScale.x = 1.0f;
    prep_v2.motionVectorScale.y = 1.0f;
    prep_v2.frameTimeDelta = clamped_dt_ms;
    prep_v2.reset = is_reset;
    prep_v2.cameraNear = 3.402823466e+38F;
    prep_v2.cameraFar = 0.1f;
    prep_v2.cameraFovAngleVertical = 1.04719755f;
    prep_v2.viewSpaceToMetersFactor = 1.0f;
    prep_v2.depth = depth_api_res;
    prep_v2.motionVectors = mv_api_res;
    prep_v2.cameraPosition[0] = 0.0f;
    prep_v2.cameraPosition[1] = 0.0f;
    prep_v2.cameraPosition[2] = 0.0f;
    prep_v2.cameraUp[0] = 0.0f;
    prep_v2.cameraUp[1] = 1.0f;
    prep_v2.cameraUp[2] = 0.0f;
    prep_v2.cameraRight[0] = 1.0f;
    prep_v2.cameraRight[1] = 0.0f;
    prep_v2.cameraRight[2] = 0.0f;
    prep_v2.cameraForward[0] = 0.0f;
    prep_v2.cameraForward[1] = 0.0f;
    prep_v2.cameraForward[2] = 1.0f;

    ffxReturnCode_t prep_rc = ctx->dispatch(&ctx->fg_context, &prep_v2.header);
    if (prep_rc != FFX_API_RETURN_OK) {
        prep_rc = dispatch_prepare_legacy_fallback(
            ctx->dispatch,
            &ctx->fg_context,
            d3d_cmd_list,
            depth_api_res,
            mv_api_res,
            mv_width,
            mv_height,
            clamped_dt_ms,
            frame_id,
            is_reset
        );
        if (prep_rc != FFX_API_RETURN_OK) {
            return -40 - static_cast<int32_t>(prep_rc);
        }
    }

    // Step C: Standalone Optical Flow + Frame Interpolation Dispatch at full display resolution
    ffxDispatchDescFrameGeneration fg_disp = {};
    fg_disp.header.type = FFX_API_DISPATCH_DESC_TYPE_FRAMEGENERATION;
    fg_disp.header.pNext = nullptr;
    fg_disp.commandList = d3d_cmd_list;
    fg_disp.presentColor = ffxApiGetResourceDX12(
        static_cast<ID3D12Resource*>(curr_color_res),
        FFX_API_RESOURCE_STATE_PIXEL_COMPUTE_READ
    );
    fg_disp.outputs[0] = ffxApiGetResourceDX12(
        static_cast<ID3D12Resource*>(out_interpolated_res),
        FFX_API_RESOURCE_STATE_UNORDERED_ACCESS
    );
    fg_disp.outputs[1] = FfxApiResource{};
    fg_disp.outputs[2] = FfxApiResource{};
    fg_disp.outputs[3] = FfxApiResource{};
    fg_disp.numGeneratedFrames = 1;
    fg_disp.reset = is_reset;
    fg_disp.backbufferTransferFunction = FFX_API_BACKBUFFER_TRANSFER_FUNCTION_SRGB;
    fg_disp.minMaxLuminance[0] = 0.0f;
    fg_disp.minMaxLuminance[1] = 1.0f;
    fg_disp.generationRect.left = 0;
    fg_disp.generationRect.top = 0;
    fg_disp.generationRect.width = static_cast<int32_t>(display_width);
    fg_disp.generationRect.height = static_cast<int32_t>(display_height);
    fg_disp.frameID = frame_id;

    const ffxReturnCode_t disp_rc = ctx->dispatch(&ctx->fg_context, &fg_disp.header);
    if (disp_rc != FFX_API_RETURN_OK) {
        return -60 - static_cast<int32_t>(disp_rc);
    }

    return 0;
}

extern "C" void ff_fsr3_destroy(void* handle) {
    if (handle == nullptr) {
        return;
    }

    FfFsr3Context* ctx = static_cast<FfFsr3Context*>(handle);
    if (ctx->fg_context != nullptr && ctx->destroy_context != nullptr) {
        ctx->destroy_context(&ctx->fg_context, nullptr);
        ctx->fg_context = nullptr;
    }

    ctx->create_context  = nullptr;
    ctx->destroy_context = nullptr;
    ctx->configure       = nullptr;
    ctx->query           = nullptr;
    ctx->dispatch        = nullptr;

    if (ctx->dll_module != nullptr) {
        FreeLibrary(ctx->dll_module);
        ctx->dll_module = nullptr;
    }

    delete ctx;
}

#else // !defined(_WIN32) || !defined(FFX_SDK_AVAILABLE)

extern "C" int32_t ff_fsr3_create(
    void* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    const uint16_t* dll_dir_utf16,
    void** out_handle,
    char* out_version_utf8,
    uint32_t out_version_cap
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
    if (out_version_utf8 != nullptr && out_version_cap > 0) {
        out_version_utf8[0] = '\0';
    }
    return -100;
}

extern "C" int32_t ff_fsr3_dispatch(
    void* handle,
    void* cmd_list,
    void* curr_color_res,
    void* depth_res,
    void* motion_vectors_res,
    void* out_interpolated_res,
    uint32_t display_width,
    uint32_t display_height,
    uint32_t mv_width,
    uint32_t mv_height,
    float delta_time_ms,
    uint64_t frame_id,
    int32_t reset
) {
    (void)handle;
    (void)cmd_list;
    (void)curr_color_res;
    (void)depth_res;
    (void)motion_vectors_res;
    (void)out_interpolated_res;
    (void)display_width;
    (void)display_height;
    (void)mv_width;
    (void)mv_height;
    (void)delta_time_ms;
    (void)frame_id;
    (void)reset;
    return -100;
}

extern "C" void ff_fsr3_destroy(void* handle) {
    (void)handle;
}

#endif
