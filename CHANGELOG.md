# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — Round 3 (Tier A: decode pipeline scaffolding)

- New `config` module — safe wrapper around `VAConfigID`.
  - `Config::new(&Display, profile, entrypoint, attribs)` calls
    `vaCreateConfig`. Empty `attribs` slice maps to `(NULL, 0)` so
    the driver picks defaults (e.g. `VA_RT_FORMAT_YUV420` on H.264
    VLD).
  - `Config::supported_attributes()` walks a portable shortlist
    (`RTFormat`, `DecSliceMode`, `MaxPictureWidth`,
    `MaxPictureHeight`, `RateControl`) and returns each value the
    driver advertises (filtered against `VA_ATTRIB_NOT_SUPPORTED`).
  - `Config::get_attribute(type)` reads a single attribute.
  - Standalone `config::supported_attributes` /
    `config::get_attribute` mirror the methods so capability
    probing doesn't require constructing a `Config` first.
  - `Drop` calls `vaDestroyConfig`.
- New `context` module — safe wrapper around `VAContextID` plus
  the render-target surfaces it owns.
  - `Context::new(&Display, &Config, w, h, num_surfaces)` allocates
    `VA_RT_FORMAT_YUV420` surfaces via `vaCreateSurfaces` and binds
    them to a context via `vaCreateContext`. Failures during
    `vaCreateContext` clean up the just-allocated surfaces.
  - `Drop` tears down in reverse order (`vaDestroyContext` →
    `vaDestroySurfaces`).
- `sys.rs` extensions:
  - Resolved into the vtable: `vaQueryConfigEntrypoints`,
    `vaGetConfigAttributes`, `vaDestroyConfig`, `vaDestroyContext`,
    `vaSyncSurface`, `vaDestroyBuffer`, `vaMapBuffer`,
    `vaUnmapBuffer`, `vaDeriveImage`, `vaDestroyImage`, `vaGetImage`,
    `vaPutImage`, `vaCreateImage`, `vaQueryImageFormats`,
    `vaMaxNumImageFormats`.
  - New constants: `VAConfigAttribType` values (`RTFormat`,
    `DecSliceMode`, `MaxPictureWidth`, `MaxPictureHeight`,
    `RateControl`, etc.), `VA_ATTRIB_NOT_SUPPORTED`,
    `VA_RT_FORMAT_YUV420/422/444/420_10`, `VA_FOURCC_NV12 / I420 /
    YV12 / IYUV`, `VABufferType` decode entries
    (`VAPictureParameterBufferType`, `VASliceParameterBufferType`,
    `VASliceDataBufferType`, `VAImageBufferType`),
    `VA_SLICE_DATA_FLAG_*`, `VA_PICTURE_H264_*`,
    `VA_PADDING_LOW/MEDIUM/HIGH`, `VA_INVALID_ID`,
    `VA_INVALID_SURFACE`.
  - New struct types: `VAConfigAttrib`, `VAImageFormat`, `VAImage`,
    `VAPictureH264` (with `::invalid()` helper), and the
    decoder-target buffers `VAPictureParameterBufferH264` and
    `VASliceParameterBufferH264`. All `#[repr(C)]`; sizes verified
    by hand against a C `sizeof()` driver to match
    `/usr/include/va/va.h` exactly (36 / 672 / 3128 / 120 / 48 / 8
    bytes respectively on x86_64).
- New integration tests in `tests/round3_pipeline.rs` (skip-friendly
  on no-driver hosts):
  - `h264_high_decode_config_creates` — `Config::new(&dpy,
    VAProfileH264High, VAEntrypointVLD, &[])` returns Ok with a
    non-zero config id, profile, and entrypoint round-tripping
    through accessors.
  - `h264_high_decode_context_creates` — config + a 1920×1088
    single-surface context create succeeds end-to-end.
  - `h264_high_supports_yuv420_render_target` — query
    `VAConfigAttribRTFormat` for `(H264High, VLD)` and assert the
    returned bitmask includes `VA_RT_FORMAT_YUV420 = 0x01`.

### Status — Round 3 (Tier B: actual decode)

- A first attempt at end-to-end H.264 IDR decode (parse SPS/PPS,
  build `VAPictureParameterBufferH264` + `VASliceParameterBufferH264`,
  submit `vaCreateBuffer × 3` → `vaBeginPicture` → `vaRenderPicture`
  → `vaEndPicture` → `vaSyncSurface` → `vaGetImage`) was implemented
  locally and the submission half works: every libva entry point
  returns `VA_STATUS_SUCCESS` and the surface read-back hits the
  `vaCreateImage` + `vaGetImage` fallback (because
  `nvidia-vaapi-driver` doesn't implement `vaDeriveImage`).
- However on the dev box (RTX 5080 + nvidia-vaapi-driver 0.0.16 +
  libva 1.22) the surface comes back as constant 0x80 luma /
  0x80/0x80 chroma — the driver accepts the parameter buffers but
  the GPU never writes to the surface. The submission flow is
  correct against the libva spec; the issue is most likely a field
  in the H.264-specific parameter buffers that
  `nvidia-vaapi-driver`'s shim handles differently from the way
  `va.h` documents (or a missing field-pic / DPB bookkeeping detail
  for the IDR-only case).
- Per workspace clean-room policy we cannot consult
  `nvidia-vaapi-driver`, ffmpeg's `vaapi.c`, gstreamer-vaapi, or
  third-party Rust bindings to diff our parameter setup against a
  known-good submission. Without a second VA-API driver on the box
  for cross-checking (Intel/AMD), debugging this becomes an
  open-ended bisection exercise across ~60 fields and the
  driver-specific quirks behind the closed-source NVDEC firmware.
- The Tier B scaffolding (NAL splitter, RBSP de-emulation,
  Exp-Golomb `BitReader`, SPS/PPS/slice-header parsers,
  C-bitfield-faithful `seq_fields` / `pic_fields` packing — all
  unit-tested) was removed from the crate before commit so the
  shipped surface only contains code that actually does what its
  docs claim. It will land as part of the H.264 decoder crate
  (`oxideav-h264`) where the parsers belong — at which point this
  crate just needs the pipeline glue to call them.
- Tier C (encode entrypoint) was therefore deliberately not started.

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
