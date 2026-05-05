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
//! Round 2 (this commit): a safe [`Display`] wrapper around the DRM
//! render-node libva backend — opens `/dev/dri/renderD128`, calls
//! `vaGetDisplayDRM`, runs `vaInitialize`, and surfaces the
//! driver-supplied error string via [`VaError::Init`] when no
//! `*_drv_video.so` is installed for the GPU. `vaQueryVendorString`
//! and `vaQueryConfigProfiles` are wired up for the success path
//! (used on boxes with an Intel/AMD/installed-NVIDIA-shim driver).
//! No codec factories yet — those come in Round 3 once we have a
//! tested-against-driver path.
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
