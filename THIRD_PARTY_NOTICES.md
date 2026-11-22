# Third-Party Notices & License Information

**FrameForge v1.0.0** — Created by **SprtnDio**

FrameForge's own source code, HLSL shaders, icons, and documentation are licensed under the **MIT License** (`LICENSE`). Binary distributions (`.msi` installer and portable `.zip` archive) may bundle unmodified runtime libraries (`.dll`) from third-party GPU SDKs as well as open-source Rust crates listed below. Full original SDK license texts are included inside the `licenses/` folder of every binary release package.

---

## 1. AMD FidelityFX SDK (v2.4.0)

- **Bundled Runtime Files:** `amd_fidelityfx_loader_dx12.dll`, `amd_fidelityfx_framegeneration_dx12.dll`, `amd_fidelityfx_upscaler_dx12.dll`
- **Copyright:** Copyright (c) Advanced Micro Devices, Inc. All rights reserved.
- **License:** MIT License (`licenses/AMD_FidelityFX_LICENSE.md`)

```text
MIT License

Copyright (c) 2023-2025 Advanced Micro Devices, Inc. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

---

## 2. Intel XeSS SDK (v3.0.2)

- **Bundled Runtime Files:** `libxess.dll`, `libxell.dll`, `libxess_fg.dll`
- **Copyright:** Copyright (c) Intel Corporation. All rights reserved.
- **License:**
  - **C/C++ Headers (`xess*.h`, `xell*.h`, `xess_fg*.h`):** MIT License.
  - **Precompiled Runtime Binaries (`libxess*.dll`, `libxell.dll`):** Redistributed as unmodified binaries under the **Intel Simplified Software License (ISSL)** and accompanying third-party notices included in `licenses/Intel_*`.

---

## 3. NVIDIA Streamline SDK (v2.14.16) & NVIDIA RTX / DLSS Plugins

- **Bundled Runtime Files:** `sl.interposer.dll`, `sl.common.dll`, `sl.dlss_g.dll`, `sl.dlss.dll`, `sl.reflex.dll`, `sl.pcl.dll`, `nvngx_dlss.dll`, `nvngx_dlssg.dll`
- **Copyright:** Copyright (c) NVIDIA Corporation. All rights reserved.
- **License:**
  - **NVIDIA Streamline Framework & Headers:** MIT License (`licenses/NVIDIA_*`).
  - **NVIDIA DLSS / NGX Precompiled Binaries (`nvngx_dlss*.dll` and associated RTX SDK plugins):** Proprietary property of NVIDIA Corporation, redistributed unmodified subject to the **NVIDIA RTX SDKs License Agreement** included in `licenses/NVIDIA_*`. Reverse engineering, decompilation, or modification of proprietary NVIDIA binaries is prohibited under their applicable license terms.

---

## 4. Open-Source Rust Crates & Build Tooling

FrameForge is built in Rust (2021 Edition) using the following open-source libraries:

| Crate | Version | License | Purpose |
| :--- | :--- | :--- | :--- |
| `windows` | `0.58` | MIT OR Apache-2.0 | DirectX 12, DXGI, Fxc, COM & Win32 system bindings |
| `winit` | `0.30` | Apache-2.0 | Native Win32 windowing & non-blocking event loop |
| `raw-window-handle` | `0.6` | MIT OR Apache-2.0 OR Zlib | Interoperable Win32 `HWND` extraction |
| `egui` | `0.29` | MIT OR Apache-2.0 | Immediate-mode graphical user interface & telemetry OSD |
| `egui-winit` | `0.29` | MIT OR Apache-2.0 | `winit` platform integration for `egui` |
| `nokhwa` | `0.10` | Apache-2.0 | Microsoft Media Foundation (`input-msmf`) HDMI video capture |
| `cpal` | `0.15` | Apache-2.0 | Low-latency WASAPI audio capture & playback |
| `crossbeam-channel` | `0.5` | MIT OR Apache-2.0 | Lock-free multi-threaded frame & audio event channels |
| `serde` & `serde_json` | `1.0` | MIT OR Apache-2.0 | Persistent JSON configuration serialization |
| `anyhow` | `1.0` | MIT OR Apache-2.0 | Structured error context propagation |
| `cc` | `1.x` | MIT OR Apache-2.0 | MSVC C++17 compilation for native SDK bridges |
| `winresource` | `0.1` | MIT | Windows PE icon & version resource embedding |

Windows installer packages (`.msi`) are built using the open-source **WiX Toolset v5.0.2** (OSI-approved MS-RL).

---

## 5. Trademark & Non-Affiliation Disclaimer (English / Deutsch)

### English
**FrameForge** (created and maintained by **SprtnDio**) is an independent open-source software project. It is **not** affiliated with, endorsed by, sponsored by, or officially connected to **Advanced Micro Devices, Inc. (AMD)**, **Intel Corporation**, **NVIDIA Corporation**, **Microsoft Corporation**, or any of their subsidiaries or affiliates.

*AMD, FidelityFX, Radeon, and FSR* are trademarks or registered trademarks of Advanced Micro Devices, Inc. *Intel, Arc, Iris, and XeSS* are trademarks of Intel Corporation or its subsidiaries. *NVIDIA, GeForce, RTX, DLSS, and Reflex* are trademarks and/or registered trademarks of NVIDIA Corporation. *Microsoft, Windows, DirectX, and Direct3D* are registered trademarks of Microsoft Corporation. All product and company names are used strictly for identification and hardware/SDK compatibility description purposes.

### Deutsch
**FrameForge** (entwickelt und gepflegt von **SprtnDio**) ist ein unabhängiges Open-Source-Softwareprojekt. Es besteht **keinerlei** wirtschaftliche, organisatorische oder offizielle Verbindung zu **Advanced Micro Devices, Inc. (AMD)**, der **Intel Corporation**, der **NVIDIA Corporation**, der **Microsoft Corporation** oder deren Tochtergesellschaften; das Projekt wird von diesen Unternehmen weder unterstützt noch gesponsert.

*AMD, FidelityFX, Radeon und FSR* sind Marken oder eingetragene Marken von Advanced Micro Devices, Inc. *Intel, Arc, Iris und XeSS* sind Marken der Intel Corporation oder ihrer Tochtergesellschaften. *NVIDIA, GeForce, RTX, DLSS und Reflex* sind Marken bzw. eingetragene Marken der NVIDIA Corporation. *Microsoft, Windows, DirectX und Direct3D* sind eingetragene Marken der Microsoft Corporation. Alle genannten Produkt- und Markennamen dienen ausschließlich der technischen Kompatibilitätsbeschreibung.
