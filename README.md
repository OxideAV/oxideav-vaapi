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
| H.264        | planned | planned |
| HEVC         | planned | planned |
| VP9          | planned | planned |
| AV1          | planned (Intel Tiger Lake+, AMD RDNA3+) | planned (Intel/AMD where supported) |
| VP8          | planned | — |
| MPEG-2       | planned | planned |
| JPEG         | planned | planned |
| VVC (H.266)  | planned (Intel Lunar Lake+) | — |

Round 1 (this commit): scaffolding only. The crate dlopens `libva.so.2` + `libva-drm.so.2`, resolves a small bootstrap symbol set, and exposes a `register(&mut RuntimeContext)` entry point that confirms the framework loads without registering any codec factories yet. Round 2: H.264 + HEVC decode via `vaCreateConfig` / `vaCreateContext` / `vaBeginPicture` / `vaRenderPicture` / `vaEndPicture`.

## Workspace policy

Calling a system OS / driver API via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule (no embedding source from libvpx, libwebp, libjxl, etc.) does not apply to this crate.

## License

MIT.
