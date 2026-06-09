#![cfg(target_os = "linux")]
//! Linux VA-API hardware decode/encode bridge.
//!
//! This crate is a **runtime-loaded** bridge to the
//! [VA-API](https://intel.github.io/libva/) library family. It uses
//! [`libloading`] to `dlopen` `libva.so.2` and `libva-drm.so.2` on
//! first use, so:
//!
//! * Linux builds have **no compile-time link dependency** on libva;
//!   if the libraries can't be loaded, the registered factories return
//!   `Error::Unsupported` and the framework registry falls back to the
//!   pure-Rust codec implementation.
//! * No bindgen, no `*-sys` crate, no autoconf gymnastics. Symbol
//!   resolution and `VAStatus` propagation is all done by hand.
//!
//! The crate is gated to `cfg(target_os = "linux")` at the source
//! level: on macOS / Windows the entire crate compiles to an empty
//! rlib, and consumers (umbrella `oxideav`) gate the `register` call
//! behind the same cfg.
//!
//! # Status
//!
//! Round 4 (this commit): capability-probing API plus the driver-
//! reality findings that came out of investigating Round 3's H.264
//! decode silent-fail and the planned encode pivot.
//!
//! * [`Display::entrypoints`], [`Display::is_supported`], and
//!   [`Display::profiles_with_entrypoint`] are the public capability
//!   surface for asking "does this host's VA-API stack accelerate
//!   codec X for operation Y?" without having to call
//!   `vaCreateConfig` and pattern-match on its error.
//! * The diagnostic `capability_dump` integration test prints the
//!   full `(profile, entrypoint, RTFormat)` matrix and a decode /
//!   encode summary; canonical for fingerprinting a host's VA-API
//!   stack.
//! * On `nvidia-vaapi-driver 0.0.16` (the RTX 5080 dev box this
//!   crate is developed on) the encode summary is **0 profiles** —
//!   NVDEC-only by design, NVENC reached separately via
//!   `oxideav-nvidia`.
//! * No codec factories register yet. The bridge dlopens libva,
//!   exposes config/context/capability wrappers, and falls back
//!   gracefully when the driver lacks a `*_drv_video.so` for the
//!   GPU.
//!
//! # Workspace policy
//!
//! Calling a system OS / driver API via FFI is the same shape as
//! calling `libc::malloc` — it's the platform, not a copied
//! algorithm. The workspace's clean-room rule (no embedding source
//! from libvpx, libwebp, libjxl, etc.) doesn't apply here.

pub mod config;
pub mod context;
pub mod decoder;
pub mod display;
#[cfg(feature = "registry")]
pub mod engine;
pub mod profiles;
pub mod sys;

pub use config::Config;
pub use context::Context;
#[cfg(feature = "registry")]
pub use decoder::H264VaCodecDecoder;
pub use decoder::{DecodedFrame, H264VaDecoder};
pub use display::{Display, EntrypointMatrix, VaError, VaProfile};
#[cfg(feature = "registry")]
pub use engine::{device_path_for_index, engine_info};
pub use profiles::{
    codec_decode_supported, codec_encode_supported, codec_id_for_profile, codec_id_for_va_profile,
    codec_profiles, headline_profile, host_entrypoint_matrix, host_supports_codec_decode,
    KNOWN_CODECS,
};

/// Confirm the VA-API framework loads and (Round 5) register the
/// hardware H.264 decoder factory at priority 10.
///
/// Round 5: the Round 3 H.264 decode silent-fail wall is RESOLVED.
/// With the shared [`oxideav_bitstream`] parser populating the
/// parameter buffers, decode produces a pixel-perfect match against
/// ffmpeg's reference (mean abs diff = 0/255 on the 320×240 fixture).
/// The factory is registered with `hardware_accelerated = true` and
/// priority 10, ahead of the pure-Rust default.
///
/// If `libva.so.2` / `libva-drm.so.2` cannot be loaded (no GPU stack
/// installed, sandboxed environment, etc.) the function logs and
/// returns — the runtime falls back to the pure-Rust impls.
#[cfg(feature = "registry")]
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    match sys::framework() {
        Ok(_) => {
            // Probe whether the live driver actually advertises an
            // H.264 decode profile (`VLD`). On hosts where the
            // libraries load but the driver `.so` for the GPU is
            // unavailable, this returns false and we skip registration
            // so the pure-Rust path stays the only candidate. The
            // codec-family resolution is shared with engine.rs via
            // [`profiles::host_supports_codec_decode`] so any new
            // codec adapter that pre-flights the same way gets the
            // same render-node walk + family fallback semantics.
            if !host_supports_codec_decode("h264") {
                eprintln!(
                    "oxideav-vaapi: driver loaded but no H.264 decode profile advertises VLD; \
                     skipping registration"
                );
                return;
            }
            let info = oxideav_core::CodecInfo::new(oxideav_core::CodecId::new("h264"))
                .capabilities(
                    oxideav_core::CodecCapabilities::video("vaapi-h264")
                        .with_decode()
                        .with_hardware(true)
                        .with_priority(10)
                        .with_max_size(4096, 4096),
                )
                .decoder(decoder::h264_decoder_factory)
                .with_engine_id("vaapi")
                .with_engine_probe(engine::engine_info);
            ctx.codecs.register(info);
        }
        Err(e) => {
            eprintln!("oxideav-vaapi: library unavailable, skipping registration: {e}");
        }
    }
}

#[cfg(feature = "registry")]
oxideav_core::register!("vaapi", register);
