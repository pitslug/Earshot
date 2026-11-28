# Android build (T10.1)

What it takes to build Earshot's portable crates for Android, and what the Android app (T10.2 on) has to carry. Measured on the Windows x64 desktop on 2026-11-28 with NDK 30.0.16248370, cargo-ndk 4.1.2, CMake 4.4.4 and Ninja 1.13.2, and run on a Pixel (Android API 37, `arm64-v8a`).

## The command

```powershell
cargo xtask build --target aarch64-linux-android
```

It builds `earshot-core`, `earshot-asr-whisper` (whisper-rs 0.16.0, whisper.cpp 1.8.3) and `earshot-onnx` (ort 2.0.0-rc.15, `load-dynamic`) in release for `aarch64-linux-android`, into `C:\eb\aarch64-linux-android\release`. It never builds the whole workspace, which holds Windows-only crates (WASAPI capture, the desktop app), and takes no features (CPU only). `cargo xtask build --app` refuses the Android target.

Prerequisites are GATE-05's: the Android SDK and NDK (`ANDROID_NDK_HOME`, or the newest `<ANDROID_HOME>\ndk\<version>`), cargo-ndk, Ninja and CMake on PATH, and the Rust target `aarch64-linux-android`. `cargo xtask doctor` checks them.

## How it is wired (xtask/src/build.rs)

- **cargo-ndk** runs cargo (`cargo ndk -t arm64-v8a -P 26 build ...`): it is the linker and sets the NDK's compilers for cc-built crates. It passes `--target` itself.
- **whisper.cpp's CMake** uses CMake's own Android support, not the NDK's toolchain file. The `cmake` crate sets `CMAKE_SYSTEM_NAME=Android` and `CMAKE_SYSTEM_PROCESSOR=aarch64` for the target. xtask adds `CMAKE_ANDROID_NDK`, `CMAKE_SYSTEM_VERSION=26`, `CMAKE_GENERATOR=Ninja` and `CMAKE_MAKE_PROGRAM` (Ninja by full path). whisper-rs-sys forwards every `CMAKE_*` variable. The NDK's `android.toolchain.cmake` would need `ANDROID_ABI`, which whisper-rs-sys does not forward, and it falls back to `armeabi-v7a` without it.
- **Paths for CMake use forward slashes.** A backslash in `CMAKE_ANDROID_NDK` is written into CMake's generated `CMakeSystem.cmake` as an escape, and the configure fails with a syntax error.
- **`CMAKE_C_FLAGS` / `CMAKE_CXX_FLAGS` are set to `-fPIC`**, so the `cmake` crate adds no flags of its own. Otherwise it passes whisper-rs-sys's `/utf-8`, which whisper-rs-sys adds whenever its build script runs on Windows (`cfg!(target_os = "windows")` is the host's cfg in a build script), and clang reads it as an input file.
- **Release flags:** `.cargo/config.toml`'s MSVC `CMAKE_*_FLAGS_RELEASE` apply to every target, so xtask overrides them with `-O3 -DNDEBUG` (`native_flags::release_flags_env`). `check_optimised` reads the built CMake cache afterwards.
- **bindgen** gets `--sysroot=<ndk sysroot> -isystem <ndk clang>/include` through `BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android`. Without it the host libclang finds no `stdbool.h`, and whisper-rs-sys falls back to its bundled bindings with only a warning.
- **`advapi32`:** whisper-rs-sys 0.15.0 also asks for `-ladvapi32` under the host's cfg, so linking anything Android that holds whisper.cpp fails in lld. `crates/earshot-asr-whisper/build.rs` puts an empty archive of that name on the link path for Android targets built on Windows. It adds no code. Remove it when whisper-rs-sys tests the target.

## Choices

- **Minimum API level 26** (Android 8.0), as `ANDROID_API`. That is above Tauri 2's default `minSdkVersion` 24. The native code builds and links there. T10.2's app manifest takes the same value or higher, never lower. T10.3's microphone foreground service type needs API 29 behaviour at run time, which the app checks then, not the native floor.
- **CPU baseline** `armv8.2-a+dotprod+i8mm` (`GGML_CPU_ARM_ARCH`, the ARM64 Windows value from `.cargo/config.toml`, which cargo applies to every target). The Pixel reports `asimddp` (dot product) and `i8mm` in `/proc/cpuinfo`, so it fits. A phone without int8 matrix multiply (Cortex-A78 and older cores) would fail on the first such instruction. v2 targets the Pixel, so this stays until another phone is in scope.
- **ABI:** `arm64-v8a` only.

## What ran on the Pixel

The library test binaries were linked for Android (`cargo ndk ... test --no-run --lib`), pushed to `/data/local/tmp` with `libc++_shared.so`, and run there:

| Crate | Passed | Failed | Why the failures |
|---|---|---|---|
| earshot-onnx | 63 | 0 | |
| earshot-asr-whisper | 48 | 1 | reads `models/manifest.toml` from the repo |
| earshot-core | 651 | 117 | every one reads a repo file (fixtures, snapshots, scenario files) not on the phone |

So whisper.cpp and the core run on the device. ONNX Runtime itself was not loaded: `load-dynamic` opens `libonnxruntime.so` only at run time, and no library was on the phone.

## What the app must carry (T10.2)

- **`libonnxruntime.so`** from Microsoft's official `com.microsoft.onnxruntime:onnxruntime-android` AAR on Maven Central (MIT). Version 1.31.0 is published and matches the desktop's pinned 1.31.0 and ort's `api-31`. It is a new download source: it goes through a manifest entry with its licence and SHA-256 before anything fetches it, and `earshot_onnx::init` loads it by path with its digest checked, as on Windows. Its own `minSdkVersion` is read from the AAR's manifest then.
- **`libc++_shared.so`** from the NDK (`sysroot/usr/lib/aarch64-linux-android`). whisper-rs-sys links `c++_shared` on Android, so the app's `jniLibs/arm64-v8a` needs it beside the Rust library.
- **Models** come from the models folder as on Windows. Nothing here bundles them.

## Not covered

- llama.cpp (local summaries), the store, the network crate and the runtime were not built for Android. T10.1's scope is the core and the two recogniser engines. Summaries on Android are not in v2's scope.
- Building on an ARM64 Windows host: `android_env` names the NDK's `windows-x86_64` prebuilt folder, which is the only one the NDK ships for Windows. It runs under emulation on the Surface, untested.
