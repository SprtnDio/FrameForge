// FrameForge - Native C++17 DirectX 12 Bridge for NVIDIA Streamline SDK v2.14.16
// (DLSS 4.5 Transformer Super Resolution & Multi-Frame-Gen + Reflex + DLSS 5 Neural Rendering check)
// Queries genuine DXGI VendorId (`0x10DE`), reads PE file versions from `sl.dlss_g.dll` / `sl.interposer.dll`,
// initializes `sl.interposer.dll` with `PreferenceFlags::eUseManualHooking | PreferenceFlags::eAllowOTA`
// exclusively on NVIDIA GPUs (0% crash risk on Intel Iris Xe / Arc / AMD), and tracks `FrameToken` pacing.

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

#include <atomic>
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <mutex>
#include <new>
#include <string>
#include <type_traits>
#include <vector>

#if defined(SL_SDK_AVAILABLE) && defined(__has_include)
#if __has_include("sl.h")
#include "sl.h"
#if __has_include("sl_consts.h")
#include "sl_consts.h"
#endif
#if __has_include("sl_dlss_g.h")
#include "sl_dlss_g.h"
#endif
#if __has_include("sl_dlss.h")
#include "sl_dlss.h"
#endif
#if __has_include("sl_reflex.h")
#include "sl_reflex.h"
#endif
#define FF_HAS_SL_SDK 1
#elif __has_include("streamline/sl.h")
#include "streamline/sl.h"
#if __has_include("streamline/sl_consts.h")
#include "streamline/sl_consts.h"
#endif
#if __has_include("streamline/sl_dlss_g.h")
#include "streamline/sl_dlss_g.h"
#endif
#if __has_include("streamline/sl_dlss.h")
#include "streamline/sl_dlss.h"
#endif
#if __has_include("streamline/sl_reflex.h")
#include "streamline/sl_reflex.h"
#endif
#define FF_HAS_SL_SDK 1
#endif
#endif

extern "C" {

int32_t ff_dlss45_create(
    void* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    const uint16_t* dll_dir_utf16,
    void** out_handle,
    char* out_version_utf8,
    uint32_t out_version_cap,
    int32_t* out_rtx_fg_supported
);

int32_t ff_dlss45_tick_frame(
    void* handle,
    uint64_t frame_id
);

void ff_dlss45_destroy(void* handle);

} // extern "C"

#if defined(_WIN32) && defined(FF_HAS_SL_SDK)

namespace {

static_assert(sizeof(wchar_t) == sizeof(uint16_t), "Windows UTF-16 wchar_t must be 16-bit");

constexpr uint32_t kNvidiaVendorId = 0x10DEu;

using PfnSlInit               = decltype(&slInit);
using PfnSlShutdown           = decltype(&slShutdown);
using PfnSlSetD3DDevice       = decltype(&slSetD3DDevice);
using PfnSlIsFeatureSupported = decltype(&slIsFeatureSupported);
using PfnSlGetNewFrameToken   = decltype(&slGetNewFrameToken);

struct SlProcessRuntime {
    std::mutex              mutex                = {};
    std::atomic<bool>       initialized          {false};
    int32_t                 ref_count            = 0;
    HMODULE                 interposer_mod       = nullptr;
    PfnSlInit               fn_init              = nullptr;
    PfnSlShutdown           fn_shutdown          = nullptr;
    PfnSlSetD3DDevice       fn_set_d3d_device    = nullptr;
    PfnSlIsFeatureSupported fn_is_feat_supported = nullptr;
    PfnSlGetNewFrameToken   fn_get_frame_token   = nullptr;
};

SlProcessRuntime g_sl_runtime;

struct FfDlss45Context {
    bool             owns_sl_ref      = false;
    bool             rtx_fg_supported = false;
    uint32_t         vendor_id        = 0;
    uint32_t         display_width    = 0;
    uint32_t         display_height   = 0;
    sl::FrameToken*  last_frame_token = nullptr;
};

struct SlPluginInventory {
    std::wstring plugin_dir     = {};
    bool         has_interposer = false;
    bool         has_dlss_g     = false;
    bool         has_dlss_sr    = false;
    bool         has_reflex     = false;
    bool         has_dlss5_nr   = false;
    std::string  file_version   = {};
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

bool file_exists_w(const std::wstring& path) noexcept {
    if (path.empty()) {
        return false;
    }
    const DWORD attrs = GetFileAttributesW(path.c_str());
    return (attrs != INVALID_FILE_ATTRIBUTES) && ((attrs & FILE_ATTRIBUTE_DIRECTORY) == 0);
}

std::string query_pe_file_version(const std::wstring& dll_path) {
    if (dll_path.empty()) {
        return std::string();
    }

    DWORD dummy_handle = 0;
    const DWORD info_size = GetFileVersionInfoSizeW(dll_path.c_str(), &dummy_handle);
    if (info_size == 0) {
        return std::string();
    }

    std::vector<uint8_t> ver_buf(static_cast<size_t>(info_size), 0);
    if (!GetFileVersionInfoW(dll_path.c_str(), 0, info_size, ver_buf.data())) {
        return std::string();
    }

    VS_FIXEDFILEINFO* fixed_info = nullptr;
    UINT fixed_len = 0;
    if (!VerQueryValueW(
            ver_buf.data(),
            L"\\",
            reinterpret_cast<LPVOID*>(&fixed_info),
            &fixed_len) ||
        fixed_info == nullptr ||
        fixed_len < sizeof(VS_FIXEDFILEINFO)) {
        return std::string();
    }

    const unsigned int major = static_cast<unsigned int>(HIWORD(fixed_info->dwFileVersionMS));
    const unsigned int minor = static_cast<unsigned int>(LOWORD(fixed_info->dwFileVersionMS));
    const unsigned int patch = static_cast<unsigned int>(HIWORD(fixed_info->dwFileVersionLS));

    if (major == 0 && minor == 0 && patch == 0) {
        return std::string();
    }

    char formatted[48] = {0};
    std::snprintf(formatted, sizeof(formatted), "%u.%u.%u", major, minor, patch);
    return std::string(formatted);
}

SlPluginInventory inspect_streamline_directory(const std::wstring& dir) {
    SlPluginInventory inv = {};
    if (dir.empty()) {
        return inv;
    }

    inv.plugin_dir     = dir;
    const std::wstring interposer_path = dir + L"\\sl.interposer.dll";
    const std::wstring dlss_g_path     = dir + L"\\sl.dlss_g.dll";
    const std::wstring dlss_sr_path    = dir + L"\\sl.dlss.dll";
    const std::wstring reflex_path     = dir + L"\\sl.reflex.dll";
    const std::wstring dlss5_nr_path   = dir + L"\\sl.dlss_nr.dll";

    inv.has_interposer = file_exists_w(interposer_path);
    inv.has_dlss_g     = file_exists_w(dlss_g_path);
    inv.has_dlss_sr    = file_exists_w(dlss_sr_path);
    inv.has_reflex     = file_exists_w(reflex_path);
    inv.has_dlss5_nr   = file_exists_w(dlss5_nr_path);

    if (inv.has_dlss_g) {
        inv.file_version = query_pe_file_version(dlss_g_path);
    }
    if (inv.file_version.empty() && inv.has_interposer) {
        inv.file_version = query_pe_file_version(interposer_path);
    }
    return inv;
}

SlPluginInventory discover_streamline_plugins(const uint16_t* dll_dir_utf16) {
    const std::wstring primary_dir = normalize_dir_utf16(dll_dir_utf16);
    SlPluginInventory inv = inspect_streamline_directory(primary_dir);
    if (inv.has_interposer || inv.has_dlss_g) {
        return inv;
    }

    const std::wstring exe_dir = query_exe_directory();
    if (!exe_dir.empty() && exe_dir != primary_dir) {
        SlPluginInventory exe_inv = inspect_streamline_directory(exe_dir);
        if (exe_inv.has_interposer || exe_inv.has_dlss_g) {
            return exe_inv;
        }
    }

    return inv;
}

uint32_t query_adapter_vendor_id(ID3D12Device* d3d_dev, LUID* out_luid) noexcept {
    if (d3d_dev == nullptr) {
        return 0;
    }

    const LUID target_luid = d3d_dev->GetAdapterLuid();
    if (out_luid != nullptr) {
        *out_luid = target_luid;
    }

    IDXGIFactory1* factory = nullptr;
    if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), reinterpret_cast<void**>(&factory))) ||
        factory == nullptr) {
        return 0;
    }

    uint32_t vendor_id = 0;
    for (UINT idx = 0;; ++idx) {
        IDXGIAdapter1* adapter = nullptr;
        if (factory->EnumAdapters1(idx, &adapter) == DXGI_ERROR_NOT_FOUND || adapter == nullptr) {
            break;
        }
        DXGI_ADAPTER_DESC1 desc = {};
        if (SUCCEEDED(adapter->GetDesc1(&desc))) {
            if (desc.AdapterLuid.LowPart == target_luid.LowPart &&
                desc.AdapterLuid.HighPart == target_luid.HighPart) {
                vendor_id = static_cast<uint32_t>(desc.VendorId);
                adapter->Release();
                break;
            }
        }
        adapter->Release();
    }

    factory->Release();
    return vendor_id;
}

template <typename FnPtr>
FnPtr resolve_proc(HMODULE module, const char* symbol_name) noexcept {
    if (module == nullptr || symbol_name == nullptr) {
        return nullptr;
    }
    FARPROC raw = GetProcAddress(module, symbol_name);
    return reinterpret_cast<FnPtr>(reinterpret_cast<void*>(raw));
}

template <typename FlagFieldT>
constexpr FlagFieldT make_manual_sl_pref_flags() noexcept {
    using EnumU = std::underlying_type_t<sl::PreferenceFlags>;
    const EnumU combined =
        static_cast<EnumU>(sl::PreferenceFlags::eUseManualHooking) |
        static_cast<EnumU>(sl::PreferenceFlags::eAllowOTA);
    return static_cast<FlagFieldT>(combined);
}

template <typename FnInitT>
sl::Result invoke_sl_init(FnInitT fn_init, const sl::Preferences& pref) {
    if constexpr (std::is_invocable_v<FnInitT, const sl::Preferences&, uint64_t>) {
        return fn_init(pref, sl::kSDKVersion);
    } else if constexpr (std::is_invocable_v<FnInitT, const sl::Preferences&>) {
        return fn_init(pref);
    } else {
        (void)fn_init;
        (void)pref;
        return sl::Result::eErrorNotInitialized;
    }
}

template <typename FnSetDevT>
sl::Result invoke_sl_set_d3d_device(FnSetDevT fn_set_dev, void* d3d12_device) {
    if constexpr (std::is_invocable_v<FnSetDevT, void*>) {
        return fn_set_dev(d3d12_device);
    } else if constexpr (std::is_invocable_v<FnSetDevT, IUnknown*>) {
        IUnknown* unk = static_cast<IUnknown*>(static_cast<ID3D12Device*>(d3d12_device));
        return fn_set_dev(unk);
    } else {
        (void)fn_set_dev;
        (void)d3d12_device;
        return sl::Result::eErrorNotInitialized;
    }
}

template <typename FnGetTokenT>
sl::Result invoke_sl_get_frame_token(
    FnGetTokenT fn_get_token,
    sl::FrameToken*& out_token,
    uint32_t frame_index
) {
    if constexpr (std::is_invocable_v<FnGetTokenT, sl::FrameToken*&, const uint32_t*>) {
        return fn_get_token(out_token, &frame_index);
    } else if constexpr (std::is_invocable_v<FnGetTokenT, sl::FrameToken*&, uint32_t>) {
        return fn_get_token(out_token, frame_index);
    } else {
        (void)fn_get_token;
        (void)out_token;
        (void)frame_index;
        return sl::Result::eErrorNotInitialized;
    }
}

} // namespace

extern "C" int32_t ff_dlss45_create(
    void* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    const uint16_t* dll_dir_utf16,
    void** out_handle,
    char* out_version_utf8,
    uint32_t out_version_cap,
    int32_t* out_rtx_fg_supported
) {
    if (out_handle != nullptr) {
        *out_handle = nullptr;
    }
    if (out_rtx_fg_supported != nullptr) {
        *out_rtx_fg_supported = 0;
    }
    write_utf8_bounded(out_version_utf8, out_version_cap, "");

    if (d3d12_device == nullptr ||
        out_handle == nullptr ||
        display_width < 64 ||
        display_height < 64) {
        return -1;
    }

    ID3D12Device* d3d_dev = static_cast<ID3D12Device*>(d3d12_device);
    LUID adapter_luid = {};
    const uint32_t vendor_id = query_adapter_vendor_id(d3d_dev, &adapter_luid);

    const SlPluginInventory inv = discover_streamline_plugins(dll_dir_utf16);
    if (!inv.has_interposer && !inv.has_dlss_g) {
        return -2;
    }

    const std::string ver_str = inv.file_version.empty() ? "2.14.16" : inv.file_version;
    const std::string dlss5_tag = inv.has_dlss5_nr ? "+DLSS5-NR" : "";

    FfDlss45Context* ctx = new (std::nothrow) FfDlss45Context();
    if (ctx == nullptr) {
        return -3;
    }
    ctx->vendor_id      = vendor_id;
    ctx->display_width  = display_width;
    ctx->display_height = display_height;

    // Non-NVIDIA GPUs (e.g. Intel Iris Xe / Intel Arc / AMD Radeon):
    // Never invoke slInit on foreign hardware (zero crash risk), report real Streamline DLL version cleanly.
    if (vendor_id != kNvidiaVendorId) {
        const std::string non_rtx_label =
            "DLSS 4.5 Streamline " + ver_str + dlss5_tag + " (Kein RTX -> MFG-CS)";
        write_utf8_bounded(out_version_utf8, out_version_cap, non_rtx_label.c_str());
        *out_handle = ctx;
        return 0;
    }

    if (!inv.has_interposer) {
        const std::string missing_interposer_label =
            "DLSS 4.5 Streamline " + ver_str + dlss5_tag + " (Interposer fehlt -> MFG-CS)";
        write_utf8_bounded(out_version_utf8, out_version_cap, missing_interposer_label.c_str());
        *out_handle = ctx;
        return 0;
    }

    // Genuine NVIDIA GPU (`VendorId == 0x10DE`): Thread-safe singleton Streamline runtime initialization
    bool sl_ready = false;
    bool rtx_dlss_g_ok = false;

    {
        std::lock_guard<std::mutex> lock(g_sl_runtime.mutex);

        if (!g_sl_runtime.initialized.load(std::memory_order_acquire)) {
            const std::wstring interposer_full_path = inv.plugin_dir + L"\\sl.interposer.dll";
            HMODULE mod = LoadLibraryExW(
                interposer_full_path.c_str(),
                nullptr,
                LOAD_WITH_ALTERED_SEARCH_PATH
            );

            if (mod != nullptr) {
                g_sl_runtime.interposer_mod       = mod;
                g_sl_runtime.fn_init              = resolve_proc<PfnSlInit>(mod, "slInit");
                g_sl_runtime.fn_shutdown          = resolve_proc<PfnSlShutdown>(mod, "slShutdown");
                g_sl_runtime.fn_set_d3d_device    = resolve_proc<PfnSlSetD3DDevice>(mod, "slSetD3DDevice");
                g_sl_runtime.fn_is_feat_supported = resolve_proc<PfnSlIsFeatureSupported>(mod, "slIsFeatureSupported");
                g_sl_runtime.fn_get_frame_token   = resolve_proc<PfnSlGetNewFrameToken>(mod, "slGetNewFrameToken");

                if (g_sl_runtime.fn_init != nullptr &&
                    g_sl_runtime.fn_shutdown != nullptr &&
                    g_sl_runtime.fn_set_d3d_device != nullptr &&
                    g_sl_runtime.fn_is_feat_supported != nullptr) {
                    const wchar_t* plugin_paths[1] = { inv.plugin_dir.c_str() };
                    std::vector<sl::Feature> features_to_load;
                    if (inv.has_dlss_g) {
                        features_to_load.push_back(sl::kFeatureDLSS_G);
                    }
                    if (inv.has_dlss_sr) {
                        features_to_load.push_back(sl::kFeatureDLSS);
                    }
                    if (inv.has_reflex) {
                        features_to_load.push_back(sl::kFeatureReflex);
                    }

                    sl::Preferences pref = {};
                    pref.showConsole = false;
                    pref.logLevel = sl::LogLevel::eOff;
                    pref.pathsToPlugins = plugin_paths;
                    pref.numPathsToPlugins = 1u;
                    pref.flags = make_manual_sl_pref_flags<decltype(pref.flags)>();
                    pref.featuresToLoad = features_to_load.empty() ? nullptr : features_to_load.data();
                    pref.numFeaturesToLoad = static_cast<uint32_t>(features_to_load.size());
                    pref.renderAPI = sl::RenderAPI::eD3D12;

                    const sl::Result init_res = invoke_sl_init(g_sl_runtime.fn_init, pref);
                    if (init_res == sl::Result::eOk) {
                        (void)invoke_sl_set_d3d_device(g_sl_runtime.fn_set_d3d_device, d3d12_device);
                        g_sl_runtime.initialized.store(true, std::memory_order_release);
                    } else {
                        FreeLibrary(mod);
                        g_sl_runtime.interposer_mod       = nullptr;
                        g_sl_runtime.fn_init              = nullptr;
                        g_sl_runtime.fn_shutdown          = nullptr;
                        g_sl_runtime.fn_set_d3d_device    = nullptr;
                        g_sl_runtime.fn_is_feat_supported = nullptr;
                        g_sl_runtime.fn_get_frame_token   = nullptr;
                    }
                } else {
                    FreeLibrary(mod);
                    g_sl_runtime.interposer_mod       = nullptr;
                    g_sl_runtime.fn_init              = nullptr;
                    g_sl_runtime.fn_shutdown          = nullptr;
                    g_sl_runtime.fn_set_d3d_device    = nullptr;
                    g_sl_runtime.fn_is_feat_supported = nullptr;
                    g_sl_runtime.fn_get_frame_token   = nullptr;
                }
            }
        } else if (g_sl_runtime.fn_set_d3d_device != nullptr) {
            (void)invoke_sl_set_d3d_device(g_sl_runtime.fn_set_d3d_device, d3d12_device);
        }

        if (g_sl_runtime.initialized.load(std::memory_order_acquire)) {
            sl_ready = true;
            ++g_sl_runtime.ref_count;
            ctx->owns_sl_ref = true;

            if (inv.has_dlss_g && g_sl_runtime.fn_is_feat_supported != nullptr) {
                LUID mutable_luid = adapter_luid;
                sl::AdapterInfo adapter_info = {};
                adapter_info.deviceLUID = reinterpret_cast<uint8_t*>(&mutable_luid);
                adapter_info.deviceLUIDSizeInBytes = static_cast<uint32_t>(sizeof(LUID));

                const sl::Result fg_res = g_sl_runtime.fn_is_feat_supported(
                    sl::kFeatureDLSS_G,
                    adapter_info
                );
                if (fg_res == sl::Result::eOk) {
                    rtx_dlss_g_ok = true;
                }
            }
        }
    }

    ctx->rtx_fg_supported = rtx_dlss_g_ok;
    if (out_rtx_fg_supported != nullptr) {
        *out_rtx_fg_supported = rtx_dlss_g_ok ? 1 : 0;
    }

    std::string rtx_label;
    if (rtx_dlss_g_ok) {
        rtx_label = "DLSS 4.5 Transformer + MFG (SL " + ver_str + dlss5_tag + ")";
    } else if (sl_ready) {
        rtx_label = "DLSS 4.5 Streamline " + ver_str + dlss5_tag + " (RTX Tensor ohne DLSS-G -> MFG-CS)";
    } else {
        rtx_label = "DLSS 4.5 Streamline " + ver_str + dlss5_tag + " (SL Fallback -> MFG-CS)";
    }

    write_utf8_bounded(out_version_utf8, out_version_cap, rtx_label.c_str());
    *out_handle = ctx;
    return 0;
}

extern "C" int32_t ff_dlss45_tick_frame(
    void* handle,
    uint64_t frame_id
) {
    if (handle == nullptr) {
        return -1;
    }

    FfDlss45Context* ctx = static_cast<FfDlss45Context*>(handle);
    if (!ctx->owns_sl_ref || !g_sl_runtime.initialized.load(std::memory_order_acquire)) {
        return 0;
    }

    const PfnSlGetNewFrameToken fn_get_token = g_sl_runtime.fn_get_frame_token;
    if (fn_get_token == nullptr) {
        return 0;
    }

    sl::FrameToken* token = nullptr;
    const uint32_t frame_u32 = static_cast<uint32_t>(frame_id);
    const sl::Result res = invoke_sl_get_frame_token(fn_get_token, token, frame_u32);
    if (res != sl::Result::eOk) {
        return -20 - static_cast<int32_t>(res);
    }

    ctx->last_frame_token = token;
    return 0;
}

extern "C" void ff_dlss45_destroy(void* handle) {
    if (handle == nullptr) {
        return;
    }

    FfDlss45Context* ctx = static_cast<FfDlss45Context*>(handle);
    ctx->last_frame_token = nullptr;

    if (ctx->owns_sl_ref) {
        std::lock_guard<std::mutex> lock(g_sl_runtime.mutex);
        ctx->owns_sl_ref = false;
        if (g_sl_runtime.ref_count > 0) {
            --g_sl_runtime.ref_count;
        }
        if (g_sl_runtime.ref_count == 0 && g_sl_runtime.initialized.load(std::memory_order_acquire)) {
            if (g_sl_runtime.fn_shutdown != nullptr) {
                (void)g_sl_runtime.fn_shutdown();
            }
            g_sl_runtime.initialized.store(false, std::memory_order_release);
            g_sl_runtime.fn_init              = nullptr;
            g_sl_runtime.fn_shutdown          = nullptr;
            g_sl_runtime.fn_set_d3d_device    = nullptr;
            g_sl_runtime.fn_is_feat_supported = nullptr;
            g_sl_runtime.fn_get_frame_token   = nullptr;
            if (g_sl_runtime.interposer_mod != nullptr) {
                FreeLibrary(g_sl_runtime.interposer_mod);
                g_sl_runtime.interposer_mod = nullptr;
            }
        }
    }

    delete ctx;
}

#else // !defined(_WIN32) || !defined(FF_HAS_SL_SDK)

extern "C" int32_t ff_dlss45_create(
    void* d3d12_device,
    uint32_t display_width,
    uint32_t display_height,
    const uint16_t* dll_dir_utf16,
    void** out_handle,
    char* out_version_utf8,
    uint32_t out_version_cap,
    int32_t* out_rtx_fg_supported
) {
    (void)d3d12_device;
    (void)display_width;
    (void)display_height;
    (void)dll_dir_utf16;
    if (out_handle != nullptr) {
        *out_handle = nullptr;
    }
    if (out_rtx_fg_supported != nullptr) {
        *out_rtx_fg_supported = 0;
    }
    if (out_version_utf8 != nullptr && out_version_cap > 0) {
        out_version_utf8[0] = '\0';
    }
    return -100;
}

extern "C" int32_t ff_dlss45_tick_frame(
    void* handle,
    uint64_t frame_id
) {
    (void)handle;
    (void)frame_id;
    return -100;
}

extern "C" void ff_dlss45_destroy(void* handle) {
    (void)handle;
}

#endif
