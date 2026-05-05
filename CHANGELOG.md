# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
