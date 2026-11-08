# Checking the ARM64 installer on the Surface

The ARM64 installer is built and signed on the desktop, where the release key stays, and checked on the Surface, because `bundle-check` installs the app and runs a meeting through it, which needs an ARM64 machine (T8.4h). On the desktop, `cargo xtask bundle-check --target aarch64-pc-windows-msvc` refuses and installs nothing.

## On the desktop: build

```powershell
cargo xtask build --target aarch64-pc-windows-msvc --bundle --target-dir C:\eb
```

The installer is `C:\eb\aarch64-pc-windows-msvc\release\bundle\nsis\Earshot_<version>_arm64-setup.exe`. Copy that file to the Surface (any folder, for example `D:\release`). Do not rename it: the check reads the architecture and version from its name.

## On the Surface: check

1. Check out the commit the installer was built from (the check refuses an installer whose version differs from the repo's), with Earshot closed.
2. Fetch the models the replay uses into a folder your own shell can see, and point `EARSHOT_MODELS` at it:

```powershell
$env:EARSHOT_MODELS = "$env:LOCALAPPDATA\Earshot\models"
```

```powershell
cargo xtask fetch-models --set desktop,arm64
```

`desktop` alone holds only the x64 ONNX Runtime; `arm64` adds the ARM64 one.

3. Run the check from the repo root, in your own PowerShell (an agent shell sees a virtualised registry), with the ARM64 Rust toolchain (the check takes the host's architecture from the xtask it runs as):

```powershell
cargo xtask bundle-check --installer D:\release\Earshot_<version>_arm64-setup.exe
```

The models and ONNX Runtime for the replay come from `EARSHOT_MODELS`, never from the install, so a pass says nothing about a machine without them.

### What PASS looks like

- The first line is `bundle-check: installer <path> sha256 <hex>`.
- Then one `bundle-check: no build: ...` line per comparison with the build it cannot make, because there is no build folder: the QNN files are checked against their manifest pins instead, `NOTICE.md` for the names of every QNN file, the exe and DLLs by machine type and imports, and `earshot.pdb` is skipped (no build to compare with and no pin).
- Every verdict line reads `ok`, including `the installed exe is for the installer's architecture`, each `matches its manifest pin`, `NOTICE.md names every QNN file`, the replay checks and `registry restored`.
- The last line is `bundle-check: PASS installer <path> sha256 <hex> (...)` and the command exits 0. Any `FAIL` line fails the run and keeps the work folder under `target\bundle-check`.

Write down the SHA-256 from the last line.

## On the desktop: sign

Check that the installer still has the SHA-256 the Surface printed:

```powershell
Get-FileHash C:\eb\aarch64-pc-windows-msvc\release\bundle\nsis\Earshot_<version>_arm64-setup.exe -Algorithm SHA256
```

Then sign it, from a shell that can read the key:

```powershell
cargo xtask sign-update --key "$env:USERPROFILE\.earshot-private\update-signing.key" --target aarch64-pc-windows-msvc --target-dir C:\eb
```

It writes `earshot-update-arm64.json` beside the installer and prints both paths: publish both in the release, as in [updates.md](updates.md).
