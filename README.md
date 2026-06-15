# oxideav-vaapi

Linux VA-API hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

[VA-API](https://intel.github.io/libva/) is the dominant hardware acceleration interface on Linux for Intel iGPUs and AMD GPUs (via the radeonsi/Mesa driver), and it is also supported on NVIDIA through the `nvidia-vaapi-driver` shim. For codecs the chip supports natively this is **5–50× faster** than software decoding and orders of magnitude more energy-efficient.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on `libva`. The library is opened via [`libloading`] on first use.

## Fallback behaviour

Two distinct failure paths fall back automatically to the pure-Rust codec:

1. **Load failure** — `libva.so.2` or `libva-drm.so.2` not installed, distro without GPU stack, sandboxed environment without `/dev/dri` access. `register()` logs and returns without registering, so the SW codec is the only candidate at dispatch.
2. **Init failure** — `vaInitialize` / `vaCreateConfig` / `vaCreateContext` return a non-zero `VAStatus` for the requested parameters. Common triggers: stream above the driver's max resolution, profile the GPU doesn't accelerate, no compatible DRI render node. The factory returns `Err`; the registry's `make_decoder_with` / `make_encoder_with` retries the next-priority impl (typically the SW one).

Pipelines that **require** hardware can opt out of the SW fallback by setting `CodecPreferences { require_hardware: true, .. }` — the registry will then surface the `VAStatus` error instead of degrading silently.

## Platform gating

The whole crate is `#![cfg(target_os = "linux")]`. On macOS / Windows it compiles to an empty rlib; the umbrella `oxideav` crate gates the `register` call behind the same cfg.

## Priority

Hardware factories register with `CodecCapabilities::with_priority(10)` — **lower numbers win at resolution time**, so on Linux+VA-API hardware paths are preferred over the pure-Rust impls (which sit at priority 100+).

## Opt-out

Users who want to force the pure-Rust path globally can pass `--no-hwaccel` to the `oxideav` CLI; this sets `CodecPreferences { no_hardware: true }`, which the pipeline forwards to `make_decoder_with` / `make_encoder_with` so HW factories are skipped at dispatch time. The runtime context still registers VA-API — `oxideav list` shows the `*_vaapi` rows regardless of the flag — only resolution is biased.

## Coverage roadmap

| Codec        | Decode | Encode |
|--------------|--------|--------|
| H.264        | planned | depends on host driver (see below) |
| HEVC         | planned | depends on host driver |
| VP9          | planned | depends on host driver |
| AV1          | planned (Intel Tiger Lake+, AMD RDNA3+) | depends on host driver |
| VP8          | planned | — |
| MPEG-2       | planned | depends on host driver |
| JPEG         | planned | depends on host driver |
| VVC (H.266)  | planned (Intel Lunar Lake+) | — |

Encode availability is **host-driver dependent**. VA-API exposes
encode only when the underlying driver shim wraps a hardware encoder.
On Intel iGPUs (`iHD`/`i965`) and AMD GPUs (`mesa-va-gallium`) most
codecs land an `EncSlice` entrypoint; on NVIDIA via
`nvidia-vaapi-driver` (NVDEC-only) **no** encode entrypoint is
exposed — see [`Display::is_supported`](#capability-probing) for
the runtime check.

## Capability probing

This crate's biggest user-facing API is post-init capability probing,
because what VA-API drivers actually do varies dramatically by
vendor / chip / driver version. Three helpers cover the typical
audit:

```rust,ignore
use oxideav_vaapi::{Display, VaProfile};
use oxideav_vaapi::sys::{profile, entrypoint};
use std::path::Path;

let dpy = Display::open_drm(Path::new("/dev/dri/renderD128"))?;

// Single yes/no:
let h264_decode_ok = dpy.is_supported(
    VaProfile(profile::VAProfileH264High),
    entrypoint::VAEntrypointVLD,
);

// Full entrypoint list for a profile:
let h264_entries = dpy.entrypoints(
    VaProfile(profile::VAProfileH264High),
)?;

// All profiles that support a given operation:
let encode_capable = dpy.profiles_with_entrypoint(
    entrypoint::VAEntrypointEncSlice,
)?;
```

The diagnostic `capability_dump` test (`cargo test -p oxideav-vaapi
--test capability_dump -- --ignored --nocapture`) prints the full
`(profile, entrypoint, RTFormat)` matrix and a decode/encode summary
for the loaded driver. On NVIDIA boxes that summary is currently
`encode profile(s): 0`.

## Status

The dlopen bridge, capability probing, the engine probe, and the
codec-id ↔ `VAProfile` family map are in place. H.264 decode is
wired and validated against a black-box decoder binary; it registers
at priority 10 on hosts where the driver advertises VLD for any H.264
family profile.

Capability / family-map surface:

- `codec_profiles(codec_id) -> &[i32]` — forward map from codec id to
  the advertised `VAProfile` family values.
- `codec_id_for_profile(raw: i32) -> Option<&'static str>` /
  `codec_id_for_va_profile(VaProfile)` — reverse lookup answering
  "which codec family does this advertised profile belong to?"
  (`Some("h264")` for any H.264 family value, `Some("hevc")` for
  HEVC Main/Main10/Main12/Main444[_10/_12], …; `None` for
  `VAProfileNone` or unknown vendor profiles).
- `EntrypointMatrix` — a pre-built `(profile, [entrypoints])` snapshot
  (`Display::entrypoint_matrix()`) that callers needing several
  capability checks against the same display can consult without
  re-issuing `vaQueryConfigEntrypoints` per pair. Offers
  `is_supported`, `profiles_with_entrypoint`, `entrypoints_for`,
  `any_supports`, `profiles()` — all O(rows), zero FFI. `engine_info`
  builds it once per device; `profiles::host_entrypoint_matrix()` /
  `codec_decode_supported(&matrix, codec_id)` /
  `codec_encode_supported(&matrix, codec_id)` let multi-codec
  pre-flights share one matrix.

Additional codec adapters (HEVC, VP9, AV1) register once matching
parser crates land, pre-flighting `codec_decode_supported` without
re-walking the profile list.

Integration tests are regime-agnostic and pass both against a working
driver (success path — vendor string, profile list, entrypoint set)
and against a no-driver setup (graceful-failure path — `VaError::Init`
carries the driver-supplied message).

## Workspace policy

Calling a system OS / driver API via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule (no embedding third-party codec library source) does not apply to this crate.

## License

MIT.
