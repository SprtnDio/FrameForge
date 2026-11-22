<div align="center">
  <img src="assets/logo.svg" alt="FrameForge Cyber-Anvil Logo" width="190" />

  # FrameForge v1.0.0
  **Created by SprtnDio**

  **EN:** Ultra-Low-Latency DirectX 12 HDMI Console Capture, Synthetic 3D G-Buffer & Multi-Vendor Frame Generation (`AMD FSR 4.1` • `Intel XeSS 3` • `NVIDIA DLSS 4.5` • `2x / 3x / 4x MFG`) for Windows 10/11 x64.  
  **DE:** Ultra-Low-Latency DirectX-12 HDMI-Konsolen-Capture, Synthetic 3D-G-Buffer & herstellerübergreifende Frame Generation (`AMD FSR 4.1` • `Intel XeSS 3` • `NVIDIA DLSS 4.5` • `2x / 3x / 4x MFG`) für Windows 10/11 x64.
</div>

---

## Download (GitHub Releases)

Grab the latest ready-to-run Windows x64 packages directly from the [**GitHub Releases**](../../releases) page:

### English — Choose Your Edition
| Package | Description & Settings Storage |
| :--- | :--- |
| **`FrameForge-v1.0.0-Setup-x64.msi`**<br>*(Windows Installer)* | Installs cleanly into `C:\Program Files\FrameForge` with **Start Menu** and **Desktop** shortcuts plus full Windows *Installed Apps* integration. Stores user settings automatically in `%APPDATA%\FrameForge\settings.json`. |
| **`FrameForge-v1.0.0-Portable-win64.zip`**<br>*(Portable Edition)* | Zero installation required — simply extract to any folder or USB drive and launch `frameforge.exe`. Includes `portable.mode` so all settings are saved right next to `frameforge.exe` in `frameforge_settings.json`. |

### Deutsch — Edition auswählen
| Paket | Beschreibung & Einstellungs-Speicherort |
| :--- | :--- |
| **`FrameForge-v1.0.0-Setup-x64.msi`**<br>*(Windows-Installer)* | Saubere Installation nach `C:\Program Files\FrameForge` inklusive **Startmenü-** und **Desktop-Verknüpfung** sowie Eintrag unter *Installierte Apps*. Speichert alle Nutzereinstellungen automatisch unter `%APPDATA%\FrameForge\settings.json`. |
| **`FrameForge-v1.0.0-Portable-win64.zip`**<br>*(Portable Edition)* | Keine Installation nötig — einfach in einen beliebigen Ordner entpacken und `frameforge.exe` starten. Speichert dank `portable.mode` alle Einstellungen direkt neben `frameforge.exe` in `frameforge_settings.json`. |

> **SHA-256 Checksums:** Every release includes `SHA256SUMS.txt` to verify package integrity.

---

## Key Features / Hauptfunktionen

### English
- **Zero-Backlog DirectX 12 HDMI Video & WASAPI Audio Pipeline:**
  - Prioritizes uncompressed Media Foundation capture formats (`NV12` / `YUY2`) over `MJPEG` for minimal input latency, paired with 3-slot DX12 upload ring buffers.
  - Console **Limited-RGB (`16–235 -> 0–255`)** range expansion, **`60 -> 30 FPS` duplicate frame filter** for 30 FPS console titles inside 60 Hz HDMI containers, and Contrast Adaptive Sharpening (**CAS**).
  - Dedicated low-latency **WASAPI stereo audio passthrough** (~20 ms target ring buffer) with linear resampling, soft-clip volume boost (`0%–200%`), and click-free drift crossfading.
- **Synthetic 3D G-Buffer & Edge Aegis (`cs_5_0` GPU Compute):**
  - Reconstructs **`R16G16_FLOAT` Motion Vectors** (hierarchical block matching + 3x3 Lucas-Kanade subpixel refinement) and **`R32_FLOAT` Edge-Aegis Pseudo-Depth** directly from live 2D HDMI video feeds, including live debug views (`Motion Vectors`, `Pseudo-Depth Heatmap`, `3-Way Split Screen`).
- **3 Next-Gen Frame Generation Backends (`2x / 3x / 4x` Multi-FG):**
  - **AMD FSR 4.1 Suite (FidelityFX SDK v2.4.0):** Native signed DX12 integration supporting FSR FG 4.0 ML on compatible Radeon GPUs with automatic fallback to universal FSR 3.1.6 compute (`2x`).
  - **Intel XeSS 3 (XeSS SDK v3.0.2):** XMX & DP4a AI reconstruction, XeLL low-latency pacing, and **`2x / 3x / 4x` Multi-Frame Generation**.
  - **NVIDIA DLSS 4.5 (Streamline SDK v2.14.16):** Transformer architecture, Reflex low-latency markers, **`2x / 3x / 4x` Multi-Frame Generation**, and forward-compatible DLSS 5 plugin detection.
- **Individually Selectable iGPU Performance Turbos & Anti-Flicker Sync:**
  - Four independent toggles tailored for integrated GPUs (such as Intel Iris Xe): **50% Half-Res Flow Scale**, **Fast 5-Tap Diamond Search**, **Lightweight Compute Interpolator**, and **Automatic HUD / Minimap / Subtitle Protection**.
  - Non-blocking **D3D12 Output FPS Limiter** (`Auto`, `60`, `120`, `144`, `240`, `Uncapped`) via `ControlFlow::WaitUntil` plus clean VSync default to prevent VRR brightness flicker and 600+ idle FPS.
- **Live Bilingual UI (`DE | EN`), Persistent Autosave & Draggable Telemetry OSD:**
  - Switch the entire application and OSD live between **English** and **Deutsch** (`DE | EN` in the top bar), drag & drop the fixed-width, flicker-free **Performance OSD** anywhere on screen, and rely on automatic atomic JSON persistence.

### Deutsch
- **Verzögerungsfreie DirectX-12 HDMI-Video- & WASAPI-Audio-Pipeline:**
  - Bevorzugt unkomprimierte Media-Foundation-Formate (`NV12` / `YUY2`) vor `MJPEG` für minimale Eingabelatenz, kombiniert mit einem 3-Slot-DX12-Upload-Ringpuffer.
  - **HDMI Limited-RGB-Spreizung (`16–235 -> 0–255`)** für Spielkonsolen, **`60 -> 30 FPS` Duplikat-Filter** für 30-FPS-Spiele im 60-Hz-Container und Kontrast-Nachschärfung (**CAS**).
  - Dedizierte **WASAPI-Stereo-Audio-Bridge** (~20 ms Ziel-Pufferlatenz) mit Linear-Resampler, Lautstärke-Boost (`0 %–200 %`) und knackfreiem Drift-Crossfade.
- **Synthetic 3D-G-Buffer & Edge Aegis (`cs_5_0` GPU-Compute):**
  - Rekonstruiert **`R16G16_FLOAT` Motion Vectors** (Block-Matching + 3x3-Lucas-Kanade-Subpixel-Verfeinerung) sowie **`R32_FLOAT` Edge-Aegis Pseudo-Depth** in Echtzeit aus dem 2D-HDMI-Signal – inklusive Live-Debug-Ansichten (`F4`).
- **3 Next-Gen Frame-Generation-Backends (`2x / 3x / 4x` Multi-FG):**
  - **AMD FSR 4.1 Suite (FidelityFX SDK v2.4.0):** Signierte DX12-Anbindung mit FSR FG 4.0 ML (kompatible Radeon-GPUs) und Auto-Fallback auf universelles FSR 3.1.6 Compute (`2x`).
  - **Intel XeSS 3 (XeSS SDK v3.0.2):** XMX- & DP4a-KI-Rekonstruktion, XeLL und **`2x / 3x / 4x` Multi-Frame Generation**.
  - **NVIDIA DLSS 4.5 (Streamline SDK v2.14.16):** Transformer-Architektur, Reflex, **`2x / 3x / 4x` Multi-Frame Generation** und zukunftssichere DLSS-5-Plugin-Erkennung.
- **Einzeln wählbare iGPU-Performance-Turbos & Anti-Flacker-Limiter:**
  - Vier einzeln schaltbare Optionen speziell für integrierte GPUs (z. B. Intel Iris Xe): **50 % Flow-Scale**, **5-Tap-Diamond-Schnellsuche**, **Leichtgewicht-Compute-Interpolator** sowie **HUD-, Minimap- & Untertitel-Schutz**.
  - Nicht-blockierender **D3D12-Ausgabe-FPS-Limiter** (`Auto`, `60`, `120`, `144`, `240`, `Unbegrenzt`) und flackerfreier Standard-VSync gegen VRR-Helligkeitsflackern.
- **Live-Sprachumschaltung (`DE | EN`), Autosave & frei ziehbares Performance-OSD:**
  - Komplette Oberfläche jederzeit live zwischen **Deutsch** und **English** umschaltbar, ruhiges zitterfreies Telemetrie-OSD per Maus frei positionierbar und vollautomatische Speicherung aller Einstellungen.

---

## Global Hotkeys / Globale Tastenkürzel

| Key / Taste | Action (English) | Funktion (Deutsch) |
| :---: | :--- | :--- |
| **`ESC`** | Toggle Settings Menu / Game Mode | Einstellungsmenü / Spielmodus umschalten |
| **`F2`** | Show / hide Performance Overlay (OSD) | Performance-Overlay (OSD) ein- / ausblenden |
| **`F3`** | Cycle OSD telemetry preset | OSD-Telemetrie-Profil zyklisch durchschalten |
| **`F4`** | Cycle 3D G-Buffer live debug view | 3D-G-Buffer Live-Debug-Ansicht wechseln |
| **`F11`** | Toggle Borderless Fullscreen / Windowed mode | Randloses Vollbild / Fenstermodus umschalten |
| **`M`** | Mute / unmute HDMI audio immediately | HDMI-Audio sofort stummschalten / freigeben |

---

## Building from Source / Selbst kompilieren

### Prerequisites / Voraussetzungen
- **OS:** Windows 10 / 11 (x64) with DirectX 12 capable GPU
- **Toolchain:** Rust Stable (`x86_64-pc-windows-msvc`) + Visual Studio 2022 C++ Build Tools + Windows 10/11 SDK

```powershell
# Build standalone release binary (compiles cleanly even without optional vendor SDKs via built-in DX12 compute bridges)
cargo build --release
```

To build with full native vendor SDK headers & signed DLLs (`third_party/FidelityFX-SDK`, `third_party/xess`, `third_party/streamline`) as well as the Windows `.msi` installer (via WiX Toolset `v5.0.2`), see the automated workflow in [`.github/workflows/build-windows.yml`](.github/workflows/build-windows.yml).

---

## License & Trademark Disclaimer / Lizenz & Markenhinweis

- **Source Code & Original Assets:** Copyright (c) 2026 **SprtnDio** — licensed under the [**MIT License**](LICENSE).
- **Third-Party Runtime SDKs:** Bundled vendor SDK runtime libraries (`.dll`) are governed by their respective original licenses documented in [**THIRD_PARTY_NOTICES.md**](THIRD_PARTY_NOTICES.md) and inside the `licenses/` folder of binary releases.
- **Non-Affiliation:** FrameForge is an independent open-source project by **SprtnDio** and is not affiliated with, endorsed by, or sponsored by AMD, Intel, NVIDIA, or Microsoft. All product and company names are trademarks or registered trademarks of their respective holders and are used solely to describe compatibility.
