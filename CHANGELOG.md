# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — Round 2

- New `display` module — safe wrapper around the DRM render-node
  libva backend.
  - `Display::open_drm(path)` opens `/dev/dri/renderD*` via
    `libc::open`, calls `vaGetDisplayDRM`, then `vaInitialize`.
    Returns a fully populated `Display` on success or a precise
    error variant on each step's failure.
  - `Display::api_version()`, `Display::vendor_string()`,
    `Display::profiles()`, `Display::raw()`, `Display::fd()`.
  - `Drop` calls `vaTerminate` (only if init succeeded) then
    closes the fd. Init-failure path leaks nothing.
- New `VaError` enum: `OpenDrm(io::Error)`, `GetDisplayNull`,
  `Sys(String)`, `Init { status, message }`, `Va { status, message }`.
  The `Init` variant carries the driver-supplied string from
  `vaErrorStr`, so the no-driver-installed case bubbles up a useful
  reason instead of an opaque code.
- New `VaProfile(i32)` newtype + `VaProfile::name()` for the
  H.264 / HEVC / AV1 profiles oxideav cares about.
- `sys.rs` extensions:
  - `vaQueryVendorString`, `vaMaxNumProfiles` resolved into the
    vtable.
  - `VAProfile*` and `VAEntrypoint*` constants in
    `sys::profile::*` and `sys::entrypoint::*`.
  - Additional `VA_STATUS_ERROR_*` named constants matching the
    cases we route to specific error variants.
  - `sys::error_str(vt, status)` helper that calls `vaErrorStr` and
    returns an owned `String`.
- `libc = "0.2"` added under `target.'cfg(target_os = "linux")'`
  for `open` / `close` of the render node.
- Integration test `tests/round2_init.rs`:
  - `dlopen_succeeds`, `drm_fd_opens`,
    `va_get_display_drm_returns_non_null`,
    `va_initialize_propagates_error_when_no_driver_installed`.
  - The headline test accepts both the success path (real driver
    present, asserts `api_version > (0,0)`, non-empty vendor,
    non-empty profile list) AND the graceful-failure path
    (`Err(VaError::Init { status, message })` with non-zero status
    and non-empty driver message). Same source on both kinds of
    box.

### Added — Round 1 (previously released as 0.0.1)

- Initial scaffolding: `#![cfg(target_os = "linux")]` crate that
  dlopens `libva.so.2` + `libva-drm.so.2` via `libloading` on first
  use.
- `sys.rs` exposes opaque type aliases (`VADisplay`, `VAConfigID`,
  `VAContextID`, `VASurfaceID`, `VABufferID`, `VAStatus`) and a
  resolved `Vtable` covering the bootstrap symbol set:
  `vaInitialize`, `vaTerminate`, `vaErrorStr`, `vaQueryConfigProfiles`,
  `vaCreateConfig`, `vaCreateContext`, `vaCreateSurfaces2`,
  `vaBeginPicture`, `vaRenderPicture`, `vaEndPicture`,
  `vaCreateBuffer`, plus `vaGetDisplayDRM` from the DRM backend.
- Process-wide `OnceLock<Result<Vtable, String>>` cache so the
  dlopen + dlsym round-trip happens at most once per process.
- Unified `register(&mut RuntimeContext)` entry point. Round 1: the
  function confirms the libraries load and returns; no codec
  factories are wired up yet. If load fails (libva not installed,
  sandbox without `/dev/dri`, etc.) the function logs and returns —
  the pure-Rust codec path remains the only resolution candidate.
- Standalone-friendly `registry` feature (default-on) gates the
  `oxideav-core` + `linkme` deps.
- README coverage roadmap and priority explanation.
- Smoke tests: `frameworks_load` and `vtable_resolves` confirm
  symbol resolution on Linux machines that have the VA-API stack
  installed.
