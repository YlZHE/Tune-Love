# Key detection native dependencies

Date: 2026-09-29. This document covers only the debug key-detection engine in `src-tauri`.

## Pinned sources and licenses

| Component | Pin | Source archive | SHA-256 | License retained at |
| --- | --- | --- | --- | --- |
| libkeyfinder | v2.2.6 commit `a409c7447e9f440a12627ff4a540a43e41b48a55` | `https://github.com/mixxxdj/libkeyfinder/archive/a409c7447e9f440a12627ff4a540a43e41b48a55.zip` | `3495e048be8a82ee8e946aee2c25cd6e659f46c4e9c487b90db9ac37ac10c39d` | `src-tauri/vendor/key_detection/libkeyfinder-a409c7447e9f440a12627ff4a540a43e41b48a55/LICENSE` (GPL-3.0-or-later) |
| FFTW | 3.3.10 | `https://www.fftw.org/fftw-3.3.10.tar.gz` | `56c932549852cddcfafdab3820b0200c7742675be92179e59e6215b340e26467` | `src-tauri/vendor/key_detection/fftw-3.3.10/COPYING` (GPL-2.0-or-later) |
| CMake build tool | 3.30.5 Windows x86_64 portable | official Kitware GitHub release | `5ab6e1faf20256ee4f04886597e8b6c3b1bd1297b58a68a58511af013710004b` | `src-tauri/vendor/tools/cmake-3.30.5-windows-x86_64/doc/cmake/Copyright.txt` |

The CMake archive hash was checked against Kitware's `cmake-3.30.5-SHA-256.txt`, retained beside the archive. CMake is a workspace-local build tool and is not installed globally or shipped as a runtime dependency.

## Build route and deviations

The published `libkeyfinder-sys` crate was evaluated first. Its build uses `pkg-config` to locate an already-installed libkeyfinder, so it does not provide a self-contained Windows MSVC build and was not added as a dependency. The approved thin route is used instead:

1. `build.rs` requires the retained checksum-verified CMake 3.30.5 executable and fails closed if it is absent; it never falls back to `CMAKE` or `PATH`. Each build-script run configures and incrementally builds the retained FFTW 3.3.10 source as a static, double-precision, non-threaded MSVC library. CMake's build tree is cached in Cargo's `OUT_DIR`; Cargo builds do not fetch from the network.
2. The `cc` build dependency compiles the unmodified libkeyfinder v2.2.6 `.cpp` sources and `native/keyfinder_bridge.cpp`.
3. The bridge keeps `KeyFinder::keyOfAudio` as the batch reference/fallback, and adds an owned reusable context using the upstream `progressiveChromagram`, `finalChromagram`, and `keyOfChromagram` APIs. The live chromagram is trimmed to a bounded rolling horizon; only a separate preview workspace is zero-padded for classification. C++ exceptions become bounded errors and original enum values are returned. No replacement key classifier or vendor-source patches are introduced.
4. Rust validates 48 kHz stereo finite input bounded to 6–8 seconds and explicitly maps all 24 upstream enums. It checks the complete rolling window for RMS/channel choice but normally sends only cursor-new, unscaled mono samples to native code. Uniform gain is applied to preview chroma; windows that would clip under normalization fall back to the original normalized/clamped batch path. Native construction, execution, reset-on-feed, and destruction remain serialized because FFTW planner operations are not assumed thread-safe.

Rebuild with Visual Studio 2022 Community C++ tools available:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml key_detection --lib
```

Cargo watches the libkeyfinder source/header directory, the FFTW source tree, the retained CMake executable/modules/checksum list, the bridge, and the build recipe. A watched change reruns the build script, then CMake checks its dependency graph instead of trusting the mere existence of an old `fftw3.lib`.

The current build recipe is Windows x86_64/MSVC-specific. Adding another target requires a separately verified static FFTW toolchain path; it must not silently fall back to an installed or floating system library.

## Behavioral limits

The normalization floor is RMS `5e-5`, target RMS is `0.10`, and gain is capped at `64x`. This preserves the labeled cadence at a `0.001` amplitude copy while rejecting the deterministic `0.00002` noise fixture. These thresholds are engineering bounds, not calibrated confidence.

Synthetic C-major and A-minor cadences check the native engine path and enum mapping only. They do not establish real-song accuracy. In the recorded negative fixtures, libkeyfinder classified a single A tone as A major and both seeded broadband noise and a click train as A-flat minor. The adapter therefore does not claim reliable rejection of single-tone, noise, or percussion input; downstream stability rules and independently labeled real-audio validation remain necessary.

See [incremental optimization and evaluation](key-detection-optimization.md) for measured compute costs, stream/batch boundary differences, the local-file evaluation manifest, and the limits of the native acceptance evidence.
