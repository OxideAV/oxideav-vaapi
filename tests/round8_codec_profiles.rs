//! Round 8: codec-id → VA-API profile family map.
//!
//! Exercises the new public [`oxideav_vaapi::profiles`] surface:
//!
//! 1. [`codec_profiles`] returns the canonical ascending profile list
//!    for every codec id the README's coverage roadmap names, and
//!    `None` for an unknown codec id.
//! 2. [`headline_profile`] returns the last entry of the list (the
//!    "headline" profile drivers use for max-dim queries).
//! 3. [`host_supports_codec_decode`] is skip-friendly: on hosts with
//!    no `/dev/dri/renderD128` (CI / sandbox) it returns `false` and
//!    doesn't panic; on hosts with a working H.264 decoder the call
//!    for `"h264"` returns `true`.
//! 4. The codec ids surfaced by [`engine_info`] for the headline
//!    device are all present in [`KNOWN_CODECS`] — proving the
//!    refactor (engine.rs ↔ profiles.rs) hasn't drifted.

#![cfg(target_os = "linux")]
#![cfg(feature = "registry")]

use std::path::Path;

use oxideav_vaapi::sys::profile;
use oxideav_vaapi::{
    codec_profiles, engine_info, headline_profile, host_supports_codec_decode, KNOWN_CODECS,
};

#[test]
fn codec_profiles_covers_readme_roadmap() {
    // Every codec id the README's "Coverage roadmap" table lists must
    // resolve to a non-empty family. Lower-case-only ids; the table
    // in the docs uses display-friendly names ("HEVC", "AV1") that
    // resolve to "hevc"/"av1" inside oxideav.
    for codec in ["h264", "hevc", "av1", "vp8", "vp9", "mpeg2", "vc1", "jpeg"] {
        let profiles = codec_profiles(codec)
            .unwrap_or_else(|| panic!("codec_profiles({codec:?}) returned None"));
        assert!(
            !profiles.is_empty(),
            "codec {codec:?} has empty profile list"
        );
    }
}

#[test]
fn unknown_codec_id_returns_none() {
    assert!(codec_profiles("not-a-codec").is_none());
    assert!(headline_profile("not-a-codec").is_none());
    // Skip-friendly: doesn't touch libva — short-circuits inside the
    // helper before reaching the dlopen path.
    assert!(!host_supports_codec_decode("not-a-codec"));
}

#[test]
fn headline_profile_for_h264_is_high() {
    let h = headline_profile("h264").expect("h264 in KNOWN_CODECS");
    assert_eq!(h.raw(), profile::VAProfileH264High);
}

#[test]
fn headline_profile_for_hevc_is_main444_12() {
    let h = headline_profile("hevc").expect("hevc in KNOWN_CODECS");
    assert_eq!(h.raw(), profile::VAProfileHEVCMain444_12);
}

#[test]
fn known_codecs_table_is_consistent() {
    // Every row in the table must have a non-empty profile list (the
    // `.last().unwrap()` in `headline_profile` relies on this).
    for fam in KNOWN_CODECS {
        assert!(
            !fam.profiles.is_empty(),
            "codec family {:?} has empty profile list",
            fam.codec
        );
        // `codec_profiles` must round-trip the table entry.
        assert_eq!(codec_profiles(fam.codec), Some(fam.profiles));
    }
}

#[test]
fn host_supports_codec_decode_skip_friendly_on_no_libva() {
    // On hosts with no `/dev/dri/renderD128` the call must return
    // false (not panic, not error).
    if !Path::new("/dev/dri/renderD128").exists() {
        assert!(
            !host_supports_codec_decode("h264"),
            "expected false with no /dev/dri/renderD128"
        );
    }
}

#[test]
fn engine_info_reports_only_known_codecs() {
    // The refactor moved the codec-family table from `engine.rs` to
    // `profiles.rs`. Every codec id in `engine_info()` output must
    // round-trip through `codec_profiles` — otherwise the two tables
    // have drifted.
    let devs = engine_info();
    if devs.is_empty() {
        eprintln!("No VA-API render nodes — skip");
        return;
    }
    for dev in &devs {
        for caps in &dev.codecs {
            assert!(
                codec_profiles(&caps.codec).is_some(),
                "engine_info() surfaced codec {:?} that profiles::KNOWN_CODECS doesn't know",
                caps.codec
            );
        }
    }
}
