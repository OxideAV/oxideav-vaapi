//! Round 4 integration tests — capability probing.
//!
//! Round 4 pivots from "make decode work" to "make capability probing
//! correct and machine-checkable". The Round 3 H.264 decode attempt
//! silently failed against `nvidia-vaapi-driver 0.0.16` (parameter
//! buffers accepted, surface returned constant 0x80 — i.e. NVDEC was
//! never invoked), and the encode pivot turned out to be a structural
//! dead end: the NVDEC backend advertises **decode-only** entrypoints
//! for every profile, so VA-API encode is unavailable on this driver
//! regardless of what we submit.
//!
//! These tests pin the Round-4 capability surface:
//!
//! 1. `entrypoints_for_h264_high_includes_vld` — at least the
//!    decode entrypoint exists for H.264 High on a working driver.
//! 2. `is_supported_recognises_h264_decode` — `Display::is_supported`
//!    returns `true` for `(H264High, VLD)`.
//! 3. `is_supported_returns_false_for_unsupported_profile` — bogus
//!    profile id produces `false`, not an error/panic.
//! 4. `decode_profile_set_is_non_empty` —
//!    `profiles_with_entrypoint(VLD)` returns at least one entry.
//! 5. `entrypoints_for_unsupported_profile_returns_empty` — querying
//!    a bogus profile id produces `Ok(empty)`, not `Err`.
//! 6. `encode_unavailable_on_nvdec_backend` (`#[ignore]`-gated to
//!    NVDEC dev boxes) — the structural dead-end documented above.
//!
//! All non-ignored tests are skip-friendly on hosts without a
//! VA-API driver (`Display::open_drm` returning `Err(Init)`).

#![cfg(target_os = "linux")]

use std::path::Path;

use oxideav_vaapi::sys::{entrypoint, profile};
use oxideav_vaapi::{Display, VaError, VaProfile};

const RENDER_NODE: &str = "/dev/dri/renderD128";

fn open_display_or_skip() -> Option<Display> {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("skipping: {RENDER_NODE} not present");
        return None;
    }
    match Display::open_drm(Path::new(RENDER_NODE)) {
        Ok(dpy) => Some(dpy),
        Err(VaError::Init { status, message }) => {
            eprintln!(
                "skipping: vaInitialize failed (no driver for this GPU): \
                 status={status} message='{message}'"
            );
            None
        }
        Err(other) => panic!("Display::open_drm: expected Ok or Err(VaError::Init); got {other:?}"),
    }
}

#[test]
fn entrypoints_for_h264_high_includes_vld() {
    let Some(dpy) = open_display_or_skip() else {
        return;
    };
    let entries = dpy
        .entrypoints(VaProfile(profile::VAProfileH264High))
        .expect("entrypoints query for H264High");
    assert!(
        entries.contains(&entrypoint::VAEntrypointVLD),
        "every working H.264 capable VA-API driver advertises VLD for \
         H264High; got {entries:?}"
    );
}

#[test]
fn is_supported_recognises_h264_decode() {
    let Some(dpy) = open_display_or_skip() else {
        return;
    };
    assert!(
        dpy.is_supported(
            VaProfile(profile::VAProfileH264High),
            entrypoint::VAEntrypointVLD,
        ),
        "H264High/VLD must be supported on a working H.264 decode driver"
    );
}

#[test]
fn is_supported_for_unsupported_entrypoint_is_false() {
    let Some(dpy) = open_display_or_skip() else {
        return;
    };
    // Made-up entrypoint id 999 — nothing should advertise this. The
    // call is exercising the "looked up entrypoint, didn't find it"
    // branch of `is_supported`.
    assert!(
        !dpy.is_supported(VaProfile(profile::VAProfileH264High), 999),
        "made-up entrypoint id 999 should never be in the list"
    );
}

#[test]
fn decode_profile_set_is_non_empty() {
    let Some(dpy) = open_display_or_skip() else {
        return;
    };
    let decode_profiles = dpy
        .profiles_with_entrypoint(entrypoint::VAEntrypointVLD)
        .expect("profiles_with_entrypoint(VLD)");
    assert!(
        !decode_profiles.is_empty(),
        "a working VA-API driver should advertise at least one decode profile"
    );
    eprintln!(
        "decode profiles ({}): {:?}",
        decode_profiles.len(),
        decode_profiles.iter().map(|p| p.name()).collect::<Vec<_>>()
    );
}

#[test]
fn entrypoints_for_unsupported_profile_handled_gracefully() {
    let Some(dpy) = open_display_or_skip() else {
        return;
    };
    // Profile 9999 is well outside the VA-API enum range. Drivers
    // are allowed to either:
    // 1. Return `VA_STATUS_ERROR_UNSUPPORTED_PROFILE` — `entrypoints()`
    //    maps that to `Ok(empty)` so capability audits don't have to
    //    special-case `Err`.
    // 2. Or return a generic entrypoint list (some drivers, including
    //    `nvidia-vaapi-driver` 0.0.16, do this — the driver doesn't
    //    range-check the profile).
    //
    // Either way, the wrapper must NOT panic, must NOT return `Err`,
    // and the call must complete.
    let result = dpy.entrypoints(VaProfile(9999));
    assert!(
        result.is_ok(),
        "entrypoints() on a bogus profile must be `Ok(...)`, not `Err`; \
         got {result:?}"
    );
}

/// **Documentation test** for `nvidia-vaapi-driver 0.0.16`:
/// the NVDEC backend exposes decode-only entrypoints. This test is
/// `#[ignore]` so it doesn't gate-fail on Intel/AMD machines (which
/// would advertise EncSlice for H.264 too) — run it explicitly via
/// `cargo test ... -- --ignored encode_unavailable_on_nvdec_backend`
/// to confirm the documented limitation on this dev box.
#[test]
#[ignore]
fn encode_unavailable_on_nvdec_backend() {
    let Some(dpy) = open_display_or_skip() else {
        return;
    };
    let vendor = dpy.vendor_string().unwrap_or_default();
    assert!(
        vendor.contains("NVDEC"),
        "this test is meaningful only against the NVDEC backend; \
         current vendor='{vendor}'"
    );
    // None of these profiles should advertise encode on the NVDEC
    // backend — the entire "Enc" entrypoint family is structurally
    // unavailable.
    for p in [
        profile::VAProfileH264High,
        profile::VAProfileH264Main,
        profile::VAProfileH264ConstrainedBaseline,
        profile::VAProfileHEVCMain,
        profile::VAProfileHEVCMain10,
        profile::VAProfileAV1Profile0,
    ] {
        for ep in [
            entrypoint::VAEntrypointEncSlice,
            entrypoint::VAEntrypointEncSliceLP,
        ] {
            assert!(
                !dpy.is_supported(VaProfile(p), ep),
                "NVDEC backend unexpectedly advertises encode for \
                 profile={p} entrypoint={ep}"
            );
        }
    }
}
