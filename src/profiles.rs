//! Round 8: codec-id → VA-API profile family map.
//!
//! Both [`crate::engine::engine_info`] and [`crate::register`] consult a
//! "which `VAProfile` values does this codec id refer to?" table. Until
//! this round each consumer carried its own private copy — engine.rs
//! held the seven-entry `CODEC_FAMILIES` array, and lib.rs hand-rolled a
//! single-codec `host_supports_h264_decode` predicate. With more codec
//! adapters on the way (HEVC and VP9 are the natural next two on
//! drivers where they're advertised), one shared source of truth pays
//! off — adding a new codec is a one-line edit here, and the engine
//! probe + `register()` pre-flight pick it up automatically.
//!
//! # What lives here
//!
//! * [`codec_profiles`] — given a codec id (`"h264"`, `"hevc"`, …),
//!   return the family's [`VaProfile`] list in ascending capability
//!   order (the last entry is the "headline" profile callers use for
//!   `vaGetConfigAttributes(MaxPictureWidth)` queries).
//! * [`headline_profile`] — convenience accessor for the last entry of
//!   [`codec_profiles`].
//! * [`host_supports_codec_decode`] — true iff a default DRM render
//!   node opens cleanly and at least one of the codec's family profiles
//!   advertises [`crate::sys::entrypoint::VAEntrypointVLD`]. Used by
//!   [`crate::register`] to skip codec registration on hosts where the
//!   GPU's driver shim doesn't accelerate the codec.
//! * [`KNOWN_CODECS`] — the static list of codec ids this table covers,
//!   in registration-order (h264 first because it's the only codec with
//!   a working decoder adapter so far).
//!
//! # Where the table lives
//!
//! Encoded as a `&'static [CodecFamily]` rather than a `match` so the
//! engine probe can iterate it directly without duplication.

use std::path::Path;

use crate::display::{Display, EntrypointMatrix, VaProfile};
use crate::sys::{entrypoint, profile};

/// One row in the codec → VA-API profile table.
///
/// `profiles` is in ascending capability order — the last entry is the
/// "headline" profile callers use for max-dim queries (e.g.
/// `H264High`, `HEVCMain444_12`). Some families have only one
/// profile (`vp8`).
#[derive(Copy, Clone)]
pub struct CodecFamily {
    /// The codec id used by `oxideav_core::CodecId::new`.
    pub codec: &'static str,
    /// Family profiles, ascending capability order.
    pub profiles: &'static [i32],
}

/// The codec-family table — one row per codec id this crate knows
/// about. Adding a new row here makes [`crate::engine::engine_info`]
/// surface the codec's row automatically and lets [`crate::register`]
/// pre-flight the corresponding decode profile.
///
/// Ordering matches the surface presented in
/// [`crate::engine::engine_info`]: h264, hevc, av1, vp8, vp9, mpeg2,
/// vc1. The "headline" profile (last entry of `profiles`) is the one
/// max-dim queries are routed against by default.
pub const KNOWN_CODECS: &[CodecFamily] = &[
    CodecFamily {
        codec: "h264",
        profiles: &[
            profile::VAProfileH264ConstrainedBaseline,
            profile::VAProfileH264Baseline,
            profile::VAProfileH264Main,
            profile::VAProfileH264High,
        ],
    },
    CodecFamily {
        codec: "hevc",
        profiles: &[
            profile::VAProfileHEVCMain,
            profile::VAProfileHEVCMain10,
            profile::VAProfileHEVCMain12,
            profile::VAProfileHEVCMain444,
            profile::VAProfileHEVCMain444_10,
            profile::VAProfileHEVCMain444_12,
        ],
    },
    CodecFamily {
        codec: "av1",
        profiles: &[profile::VAProfileAV1Profile0, profile::VAProfileAV1Profile1],
    },
    CodecFamily {
        codec: "vp8",
        profiles: &[profile::VAProfileVP8Version0_3],
    },
    CodecFamily {
        codec: "vp9",
        profiles: &[profile::VAProfileVP9Profile0, profile::VAProfileVP9Profile2],
    },
    CodecFamily {
        codec: "mpeg2",
        profiles: &[profile::VAProfileMPEG2Simple, profile::VAProfileMPEG2Main],
    },
    CodecFamily {
        codec: "vc1",
        profiles: &[
            profile::VAProfileVC1Simple,
            profile::VAProfileVC1Main,
            profile::VAProfileVC1Advanced,
        ],
    },
    CodecFamily {
        codec: "jpeg",
        profiles: &[profile::VAProfileJPEGBaseline],
    },
];

/// Look up the canonical VA-API profile list for a codec id.
///
/// Returns `None` for codec ids not represented in [`KNOWN_CODECS`] —
/// callers should treat that as "the VA-API bridge can't accelerate
/// this codec at all" (which is distinct from "the codec is known but
/// the driver doesn't advertise any of its profiles").
///
/// The returned slice is in ascending capability order; the last entry
/// is the headline profile used for [`headline_profile`] / max-dim
/// queries.
pub fn codec_profiles(codec_id: &str) -> Option<&'static [i32]> {
    KNOWN_CODECS
        .iter()
        .find(|f| f.codec == codec_id)
        .map(|f| f.profiles)
}

/// Reverse lookup: given a raw `VAProfile` value, return the codec id
/// of the family it belongs to.
///
/// This complements [`codec_profiles`] — that function answers "which
/// profiles does this codec id cover?", and this one answers "which
/// codec does this profile belong to?". Returns `None` for profile
/// values not represented in [`KNOWN_CODECS`] (e.g.
/// `VAProfileNone = -1`, or future profiles the table hasn't been
/// updated for).
///
/// Useful for callers walking the driver's advertised profile list
/// (e.g. an [`EntrypointMatrix`] iteration) that want to bucket each
/// advertised profile by codec without re-scanning the whole table per
/// row — one O(KNOWN_CODECS · max_family_len) sweep per profile is
/// cheap in absolute terms (~30 i32 compares on the current table) and
/// the API turns the "which codec is this driver advertising?" question
/// into a single-line lookup at the call site.
pub fn codec_id_for_profile(profile: i32) -> Option<&'static str> {
    KNOWN_CODECS
        .iter()
        .find(|f| f.profiles.contains(&profile))
        .map(|f| f.codec)
}

/// Variant of [`codec_id_for_profile`] that takes a [`VaProfile`]
/// newtype rather than a raw `i32`. Identical semantics; saves callers
/// a `.raw()` at the call site.
pub fn codec_id_for_va_profile(profile: VaProfile) -> Option<&'static str> {
    codec_id_for_profile(profile.raw())
}

/// The "headline" profile for a codec id — the last (highest)
/// [`VaProfile`] in the family's ascending list. This is the profile
/// max-dim and rate-control queries are routed against when callers
/// don't specify one explicitly.
///
/// Returns `None` for unknown codec ids (same behaviour as
/// [`codec_profiles`]).
pub fn headline_profile(codec_id: &str) -> Option<VaProfile> {
    codec_profiles(codec_id)
        .and_then(|p| p.last())
        .copied()
        .map(VaProfile)
}

/// True iff a working VA-API driver on this host advertises
/// `VAEntrypointVLD` for at least one profile in the codec's family.
///
/// Walks the canonical render node `/dev/dri/renderD128` only — same
/// host-probe shape [`crate::register`] historically used for H.264. A
/// future round can promote this to walk every render node (matching
/// [`crate::engine::engine_info`]) once a per-codec adapter wants to
/// register against multiple devices; today every adapter binds to a
/// single device via [`crate::engine::device_path_for_index`].
///
/// Returns `false` for:
///
/// * unknown codec ids (not in [`KNOWN_CODECS`]),
/// * hosts with no `/dev/dri/renderD128`,
/// * hosts whose `vaInitialize` on that node fails (no driver shim),
/// * hosts whose driver advertises the codec family but no profile
///   in the family carries the VLD (decode) entrypoint.
pub fn host_supports_codec_decode(codec_id: &str) -> bool {
    let Some(profiles) = codec_profiles(codec_id) else {
        return false;
    };
    const RENDER_NODE: &str = "/dev/dri/renderD128";
    if !Path::new(RENDER_NODE).exists() {
        return false;
    }
    let Ok(dpy) = Display::open_drm(Path::new(RENDER_NODE)) else {
        return false;
    };
    // Round 9: build the (profile, entrypoints) matrix once and answer
    // every membership check from it. Without the matrix this loop
    // issued one `vaQueryConfigEntrypoints` per family profile (4 for
    // h264, 6 for hevc, etc.); with the matrix it issues one per
    // *driver-advertised* profile up front and then does pure
    // in-memory lookups. For the H.264 register() pre-flight, that's
    // roughly a 4× → ~1× FFI reduction on multi-profile families.
    let Ok(matrix) = dpy.entrypoint_matrix() else {
        return false;
    };
    matrix.any_supports(profiles, entrypoint::VAEntrypointVLD)
}

/// Build the [`EntrypointMatrix`] for the canonical render node
/// (`/dev/dri/renderD128`).
///
/// Convenience wrapper for callers that want one shared snapshot of
/// the host's VA-API capability surface — e.g. registering multiple
/// codec adapters and pre-flighting each one without paying the FFI
/// cost N times.
///
/// Returns `None` on hosts with no render node, no working driver, or
/// any error walking the profile list.
pub fn host_entrypoint_matrix() -> Option<EntrypointMatrix> {
    const RENDER_NODE: &str = "/dev/dri/renderD128";
    if !Path::new(RENDER_NODE).exists() {
        return None;
    }
    let dpy = Display::open_drm(Path::new(RENDER_NODE)).ok()?;
    dpy.entrypoint_matrix().ok()
}

/// Variant of [`host_supports_codec_decode`] that reuses an existing
/// [`EntrypointMatrix`]. Use this when pre-flighting several codecs
/// against the same host so the matrix is built only once.
pub fn codec_decode_supported(matrix: &EntrypointMatrix, codec_id: &str) -> bool {
    let Some(profiles) = codec_profiles(codec_id) else {
        return false;
    };
    matrix.any_supports(profiles, entrypoint::VAEntrypointVLD)
}

/// Variant of [`codec_decode_supported`] for encode capability —
/// matches the engine probe's "either EncSlice or EncSliceLP counts"
/// rule.
pub fn codec_encode_supported(matrix: &EntrypointMatrix, codec_id: &str) -> bool {
    let Some(profiles) = codec_profiles(codec_id) else {
        return false;
    };
    matrix.any_supports(profiles, entrypoint::VAEntrypointEncSlice)
        || matrix.any_supports(profiles, entrypoint::VAEntrypointEncSliceLP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codecs_covers_h264_hevc_av1() {
        // Sanity: every codec id we promise in the README's coverage
        // roadmap is represented in the table.
        for codec in ["h264", "hevc", "av1", "vp8", "vp9", "mpeg2", "vc1", "jpeg"] {
            assert!(
                codec_profiles(codec).is_some(),
                "codec id {codec:?} missing from KNOWN_CODECS"
            );
        }
    }

    #[test]
    fn unknown_codec_returns_none() {
        assert!(codec_profiles("definitely-not-a-codec").is_none());
        assert!(headline_profile("definitely-not-a-codec").is_none());
    }

    #[test]
    fn headline_is_last_entry_for_h264_high() {
        // H.264 headline profile is High (last in the ascending list).
        let h = headline_profile("h264").expect("h264 known");
        assert_eq!(h.raw(), profile::VAProfileH264High);
    }

    #[test]
    fn headline_for_vp8_is_the_single_entry() {
        // Single-profile families: the headline is the only entry.
        let h = headline_profile("vp8").expect("vp8 known");
        assert_eq!(h.raw(), profile::VAProfileVP8Version0_3);
    }

    #[test]
    fn headline_for_hevc_is_main444_12() {
        // HEVC headline profile is Main444_12 (last in the ascending
        // list — encodes the broadest capability set: 4:4:4 chroma +
        // 12-bit pixel depth).
        let h = headline_profile("hevc").expect("hevc known");
        assert_eq!(h.raw(), profile::VAProfileHEVCMain444_12);
    }

    #[test]
    fn known_codecs_table_has_no_empty_family() {
        // Empty `profiles` would crash `headline_profile`'s `.last()`
        // path. Guard at the table level rather than per-call.
        for fam in KNOWN_CODECS {
            assert!(
                !fam.profiles.is_empty(),
                "codec family {:?} has empty profile list",
                fam.codec
            );
        }
    }

    #[test]
    fn host_supports_unknown_codec_is_false() {
        // Skip-friendly: doesn't touch libva — the codec-id check
        // short-circuits before we even reach the dlopen path.
        assert!(!host_supports_codec_decode("definitely-not-a-codec"));
    }

    #[test]
    fn host_supports_h264_or_no_libva() {
        // Either we have a working libva + driver that accelerates
        // H.264 decode, or we don't (sandbox / CI / no GPU stack). Both
        // are valid; the call must not panic.
        let _ = host_supports_codec_decode("h264");
    }

    #[test]
    fn codec_id_for_profile_resolves_h264_family() {
        // Every H.264 family profile value must map back to "h264".
        for raw in [
            profile::VAProfileH264ConstrainedBaseline,
            profile::VAProfileH264Baseline,
            profile::VAProfileH264Main,
            profile::VAProfileH264High,
        ] {
            assert_eq!(
                codec_id_for_profile(raw),
                Some("h264"),
                "raw profile {raw} did not map back to h264"
            );
        }
    }

    #[test]
    fn codec_id_for_profile_resolves_hevc_family() {
        // HEVC has the broadest family (6 profiles); make sure each
        // one round-trips. This is the most-likely-to-drift family
        // because it's still growing — HEVC Main12 was added after
        // Main10, and Main444 variants are still optional on many
        // drivers.
        for raw in [
            profile::VAProfileHEVCMain,
            profile::VAProfileHEVCMain10,
            profile::VAProfileHEVCMain12,
            profile::VAProfileHEVCMain444,
            profile::VAProfileHEVCMain444_10,
            profile::VAProfileHEVCMain444_12,
        ] {
            assert_eq!(codec_id_for_profile(raw), Some("hevc"));
        }
    }

    #[test]
    fn codec_id_for_profile_resolves_vp9_av1() {
        // The two next-up planned codecs per the README roadmap.
        for raw in [profile::VAProfileVP9Profile0, profile::VAProfileVP9Profile2] {
            assert_eq!(codec_id_for_profile(raw), Some("vp9"));
        }
        for raw in [profile::VAProfileAV1Profile0, profile::VAProfileAV1Profile1] {
            assert_eq!(codec_id_for_profile(raw), Some("av1"));
        }
    }

    #[test]
    fn codec_id_for_profile_returns_none_for_unknown() {
        // VAProfileNone (-1) is the canonical "no profile" sentinel and
        // is intentionally NOT in any family. Numbers above the highest
        // table entry are equally unmapped.
        assert!(codec_id_for_profile(profile::VAProfileNone).is_none());
        assert!(codec_id_for_profile(9999).is_none());
        assert!(codec_id_for_profile(-9999).is_none());
    }

    #[test]
    fn codec_id_for_va_profile_matches_raw_variant() {
        // The typed `VaProfile` wrapper must answer the same question
        // as the raw `i32` variant — anything else would mean callers
        // got different results depending on which entry point they
        // used.
        for fam in KNOWN_CODECS {
            for raw in fam.profiles {
                assert_eq!(
                    codec_id_for_va_profile(VaProfile(*raw)),
                    codec_id_for_profile(*raw),
                    "VaProfile / raw disagreement on profile {raw}"
                );
            }
        }
    }

    #[test]
    fn codec_id_round_trips_through_codec_profiles() {
        // Strongest consistency check: every profile in the table must
        // map back to a codec id whose `codec_profiles` slice contains
        // the original profile. If this fails, the two helpers are
        // looking at different views of the same table.
        for fam in KNOWN_CODECS {
            for raw in fam.profiles {
                let codec = codec_id_for_profile(*raw).expect("table entry maps back");
                let profiles = codec_profiles(codec).expect("returned codec id is in the table");
                assert!(
                    profiles.contains(raw),
                    "round-trip lost: profile {raw} → {codec} → profile list {profiles:?}"
                );
            }
        }
    }
}
