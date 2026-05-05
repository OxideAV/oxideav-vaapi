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
//! Round 1 (this commit): scaffolding only. The framework load is
//! verified via `sys::framework()`; no codec factories are wired up
//! yet. Round 2 will add H.264 + HEVC decode via `vaCreateConfig`,
//! `vaCreateContext`, `vaBeginPicture`, `vaRenderPicture`,
//! `vaEndPicture`.
//!
//! # Workspace policy
//!
//! Calling a system OS / driver API via FFI is the same shape as
//! calling `libc::malloc` — it's the platform, not a copied
//! algorithm. The workspace's clean-room rule (no embedding source
//! from libvpx, libwebp, libjxl, etc.) doesn't apply here.

pub mod sys;

/// Confirm the VA-API framework loads, but do not register any codec
/// factories yet (Round 1 scaffolding).
///
/// If `libva.so.2` / `libva-drm.so.2` cannot be loaded (no GPU stack
/// installed, sandboxed environment, etc.) the function logs and
/// returns — the runtime falls back to the pure-Rust impls.
#[cfg(feature = "registry")]
pub fn register(_ctx: &mut oxideav_core::RuntimeContext) {
    match sys::framework() {
        Ok(_) => {
            // Round 1: framework loads. No factories wired up yet.
        }
        Err(e) => {
            eprintln!("oxideav-vaapi: library unavailable, skipping registration: {e}");
        }
    }
}

#[cfg(feature = "registry")]
oxideav_core::register!("vaapi", register);
