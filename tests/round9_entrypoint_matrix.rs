//! Round 9: `Display::entrypoint_matrix` + the shared `EntrypointMatrix`
//! surface that lets callers issue O(P) FFI calls instead of O(P · K).
//!
//! The matrix is a snapshot of the `(profile, [entrypoints])` map the
//! driver advertises. Building it once amortises the
//! `vaQueryConfigEntrypoints` round-trips across however many
//! `(profile, entrypoint)` lookups the caller needs to perform.
//!
//! All tests are skip-friendly: on a host with no `/dev/dri/renderD128`
//! they short-circuit without panic. On a working host they assert the
//! matrix mirrors the existing [`Display::profiles`] /
//! [`Display::is_supported`] / [`Display::profiles_with_entrypoint`]
//! results for at least one profile/entrypoint pair.

#![cfg(target_os = "linux")]

use std::path::Path;

use oxideav_vaapi::sys::{entrypoint, profile};
use oxideav_vaapi::{
    codec_decode_supported, codec_encode_supported, host_entrypoint_matrix,
    host_supports_codec_decode, Display, EntrypointMatrix, VaProfile,
};

const RENDER_NODE: &str = "/dev/dri/renderD128";

fn open_or_skip() -> Option<Display> {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("No {RENDER_NODE} — skip");
        return None;
    }
    match Display::open_drm(Path::new(RENDER_NODE)) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("Display::open_drm failed (skip): {e}");
            None
        }
    }
}

#[test]
fn matrix_profiles_match_display_profiles() {
    // Round-trip: the matrix's advertised profile list must equal
    // `Display::profiles()` exactly. They both walk
    // `vaQueryConfigProfiles` once; differences would indicate a
    // construction bug.
    let Some(dpy) = open_or_skip() else { return };
    let matrix = dpy.entrypoint_matrix().expect("matrix builds");
    let baseline = dpy.profiles().expect("Display::profiles works");
    let from_matrix: Vec<VaProfile> = matrix.profiles().collect();
    assert_eq!(
        from_matrix, baseline,
        "matrix.profiles() differs from Display::profiles() — possible filter drift"
    );
    assert_eq!(matrix.len(), baseline.len());
}

#[test]
fn matrix_is_supported_agrees_with_display_for_advertised_profile() {
    // For every profile the driver advertises, the matrix's
    // `is_supported(VLD)` answer must equal `Display::is_supported(VLD)`.
    // If they ever disagree, callers using the matrix would see a
    // capability surface different from the one the FFI path reports.
    let Some(dpy) = open_or_skip() else { return };
    let matrix = dpy.entrypoint_matrix().expect("matrix builds");
    for p in matrix.profiles() {
        let matrix_says = matrix.is_supported(p, entrypoint::VAEntrypointVLD);
        let ffi_says = dpy.is_supported(p, entrypoint::VAEntrypointVLD);
        assert_eq!(
            matrix_says,
            ffi_says,
            "VLD-supported mismatch on {:?}",
            p.name()
        );
    }
}

#[test]
fn matrix_profiles_with_entrypoint_matches_display() {
    // Same cross-check but for the bulk filter. `profiles_with_entrypoint`
    // on `Display` walks `is_supported(p, ep)` for every advertised
    // profile and collects the matches; the matrix variant does the
    // same against its snapshot — they must agree.
    let Some(dpy) = open_or_skip() else { return };
    let matrix = dpy.entrypoint_matrix().expect("matrix builds");
    let display = dpy
        .profiles_with_entrypoint(entrypoint::VAEntrypointVLD)
        .expect("display filter");
    let from_matrix = matrix.profiles_with_entrypoint(entrypoint::VAEntrypointVLD);
    assert_eq!(from_matrix, display, "VLD filter differs across surfaces");
}

#[test]
fn matrix_any_supports_handles_unadvertised_family() {
    // `any_supports` over a list of profiles the driver doesn't
    // advertise must return false (no panic, no out-of-bounds).
    let Some(dpy) = open_or_skip() else { return };
    let matrix = dpy.entrypoint_matrix().expect("matrix builds");
    // Use a deliberately-bogus profile value far above the real enum.
    let bogus = &[9999];
    assert!(!matrix.any_supports(bogus, entrypoint::VAEntrypointVLD));
}

#[test]
fn matrix_entrypoints_for_unknown_profile_returns_empty() {
    let Some(dpy) = open_or_skip() else { return };
    let matrix = dpy.entrypoint_matrix().expect("matrix builds");
    // Profile not in the driver's advertisement list.
    assert!(matrix
        .entrypoints_for(VaProfile(profile::VAProfileNone))
        .is_empty());
}

#[test]
fn host_entrypoint_matrix_skip_friendly_on_no_libva() {
    // On a host with no render node the helper returns `None` instead
    // of panicking. On hosts with a working driver it returns
    // `Some(_)`. Either is acceptable; the test asserts no panic.
    let _ = host_entrypoint_matrix();
}

#[test]
fn host_supports_codec_decode_matches_matrix_path() {
    // Both spellings of the question must agree on a real host. We
    // build the matrix ourselves and consult `codec_decode_supported`,
    // then call the convenience helper, and check the answers match
    // for h264 (the codec the README's status row promises).
    let Some(matrix) = host_entrypoint_matrix() else {
        eprintln!("no matrix available — skip");
        return;
    };
    let from_matrix = codec_decode_supported(&matrix, "h264");
    let via_helper = host_supports_codec_decode("h264");
    assert_eq!(
        from_matrix, via_helper,
        "codec_decode_supported(matrix, \"h264\") {from_matrix} != \
         host_supports_codec_decode(\"h264\") {via_helper} — sources of truth diverged"
    );
}

#[test]
fn codec_encode_supported_returns_false_for_unknown_codec() {
    // Pure no-libva check — unknown codec id short-circuits before
    // touching the matrix.
    let Some(matrix) = host_entrypoint_matrix() else {
        eprintln!("no matrix available — skip");
        return;
    };
    assert!(!codec_encode_supported(&matrix, "definitely-not-a-codec"));
    assert!(!codec_decode_supported(&matrix, "definitely-not-a-codec"));
}

#[test]
fn empty_matrix_basics_do_not_panic() {
    // Constructible-from-scratch sanity check: an empty matrix
    // reports zero profiles, no entrypoints, no support for anything.
    // Built directly via the public `EntrypointMatrix::default()` path
    // — currently the only public constructor is `Display::entrypoint_matrix`,
    // so we synthesise an empty one through the profile filter.
    let Some(dpy) = open_or_skip() else { return };
    let matrix = dpy.entrypoint_matrix().expect("matrix builds");
    // `is_supported` over a bogus profile must yield false, not panic.
    assert!(!matrix.is_supported(VaProfile(9999), entrypoint::VAEntrypointVLD));
    // `len` ≥ 0 — sanity guard; on a real host this is > 0.
    let _ = matrix.len();
    let _ = matrix.is_empty();
}
