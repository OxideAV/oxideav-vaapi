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
pub mod display;
pub mod sys;

pub use config::Config;
pub use context::Context;
pub use display::{Display, VaError, VaProfile};

/// Confirm the VA-API framework loads, but do not register any codec
/// factories yet.
///
/// Round 2: the [`Display`] wrapper is in place but no codec
/// factories are registered — those need a working `*_drv_video.so`
/// driver `.so` to be meaningful, and we want the integration test
/// suite to demonstrate the graceful no-driver path before wiring
/// pipeline-visible behaviour.
///
/// If `libva.so.2` / `libva-drm.so.2` cannot be loaded (no GPU stack
/// installed, sandboxed environment, etc.) the function logs and
/// returns — the runtime falls back to the pure-Rust impls.
#[cfg(feature = "registry")]
pub fn register(_ctx: &mut oxideav_core::RuntimeContext) {
    match sys::framework() {
        Ok(_) => {
            // Framework loads. Codec factories deferred to Round 3
            // (see crate-level docs).
        }
        Err(e) => {
            eprintln!("oxideav-vaapi: library unavailable, skipping registration: {e}");
        }
    }
}

#[cfg(feature = "registry")]
oxideav_core::register!("vaapi", register);
