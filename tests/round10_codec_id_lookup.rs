//! Round 10: reverse lookup — `codec_id_for_profile` /
//! `codec_id_for_va_profile`.
//!
//! Until this round the codec-family table only answered the forward
//! question "which profiles cover this codec id?" (`codec_profiles`).
//! The reverse direction is the natural complement: given an advertised
//! profile value from `vaQueryConfigProfiles` (or an
//! `EntrypointMatrix` row), tell me which codec family it belongs to.
//!
//! These tests cover both the pure (no-FFI) round-trip path through
//! [`KNOWN_CODECS`] and, on a host with a working driver, the bucket-
//! by-codec walk over a real [`EntrypointMatrix`].
//!
//! All driver-touching tests are skip-friendly: on a host with no
//! `/dev/dri/renderD128` they short-circuit without panic.

#![cfg(target_os = "linux")]

use std::path::Path;

use oxideav_vaapi::sys::profile;
use oxideav_vaapi::{
    codec_id_for_profile, codec_id_for_va_profile, codec_profiles, host_entrypoint_matrix, Display,
    VaProfile, KNOWN_CODECS,
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
fn reverse_lookup_covers_every_table_entry() {
    // Strongest pure-table consistency check. Every profile listed in
    // any `KNOWN_CODECS` row must round-trip back through
    // `codec_id_for_profile` to that row's codec id — and the result
    // must be a value `codec_profiles` recognises.
    for fam in KNOWN_CODECS {
        for raw in fam.profiles {
            let id = codec_id_for_profile(*raw)
                .unwrap_or_else(|| panic!("profile {raw} missing reverse mapping"));
            assert_eq!(
                id, fam.codec,
                "profile {raw} mapped to {id:?}, expected {:?}",
                fam.codec
            );
            assert!(
                codec_profiles(id).is_some_and(|ps| ps.contains(raw)),
                "round-trip failure: {raw} → {id} → codec_profiles loses {raw}"
            );
        }
    }
}

#[test]
fn reverse_lookup_returns_none_for_va_profile_none() {
    // `VAProfileNone = -1` is the canonical "no profile selected"
    // sentinel used in scratch buffers; it must NOT match any codec.
    assert!(codec_id_for_profile(profile::VAProfileNone).is_none());
    assert!(codec_id_for_va_profile(VaProfile(profile::VAProfileNone)).is_none());
}

#[test]
fn reverse_lookup_returns_none_for_unknown_value() {
    // Values outside the documented `VAProfile` range must collapse to
    // `None` rather than e.g. panic. The high value (9999) is well
    // above the largest enum entry in libva today; the negative one
    // exercises the same path through a different code arm.
    assert!(codec_id_for_profile(9999).is_none());
    assert!(codec_id_for_profile(-9999).is_none());
    assert!(codec_id_for_va_profile(VaProfile(9999)).is_none());
}

#[test]
fn typed_and_raw_variants_agree_for_every_table_entry() {
    // Behavioural parity guard: callers should be free to use whichever
    // entry point matches their existing types. `VaProfile` is the
    // newtype Display::profiles() returns; raw `i32` is what
    // `KNOWN_CODECS` stores.
    for fam in KNOWN_CODECS {
        for raw in fam.profiles {
            assert_eq!(
                codec_id_for_profile(*raw),
                codec_id_for_va_profile(VaProfile(*raw))
            );
        }
    }
}

#[test]
fn matrix_advertised_profiles_bucket_by_codec_without_panic() {
    // Real-driver path. On a host with a working VA-API driver,
    // every advertised profile that the table knows about must
    // resolve to a codec id; unrecognised values just collapse to
    // `None` (which we tolerate — drivers occasionally advertise
    // experimental/future profile numbers).
    let Some(matrix) = host_entrypoint_matrix() else {
        eprintln!("no matrix available — skip");
        return;
    };
    for p in matrix.profiles() {
        // We don't assert `Some` here because real drivers can list
        // future or vendor-specific profile values. The contract is
        // "doesn't panic, returns either Some(known-codec) or None".
        let opt = codec_id_for_va_profile(p);
        if let Some(id) = opt {
            // Sanity: the returned codec id must be one the table
            // claims to know about.
            assert!(
                codec_profiles(id).is_some(),
                "matrix bucketed profile {p:?} as codec {id:?} but codec_profiles({id:?}) returned None"
            );
        }
    }
}

#[test]
fn matrix_codec_id_lookup_matches_table_for_recognised_profiles() {
    // For every advertised profile that the table knows about, the
    // reverse-lookup answer must be the codec id of the row containing
    // that raw profile value. We re-derive the expected codec id by
    // walking `KNOWN_CODECS` directly, then assert the helper agrees.
    let Some(dpy) = open_or_skip() else { return };
    let matrix = dpy.entrypoint_matrix().expect("matrix builds");
    for p in matrix.profiles() {
        let expected = KNOWN_CODECS
            .iter()
            .find(|f| f.profiles.contains(&p.raw()))
            .map(|f| f.codec);
        let got = codec_id_for_va_profile(p);
        assert_eq!(
            got, expected,
            "matrix profile {p:?}: reverse-lookup {got:?} != table-walk {expected:?}"
        );
    }
}

#[test]
fn reverse_lookup_zero_collision_across_families() {
    // Cross-family safety: no two distinct rows in `KNOWN_CODECS` may
    // claim the same raw profile value. If they did, the reverse
    // lookup would return whichever codec the row iteration hit first
    // — silently ambiguous. The forward / reverse contract requires
    // each profile to belong to exactly one codec.
    for (i, fam_a) in KNOWN_CODECS.iter().enumerate() {
        for raw_a in fam_a.profiles {
            for (j, fam_b) in KNOWN_CODECS.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert!(
                    !fam_b.profiles.contains(raw_a),
                    "profile {raw_a} appears in both {:?} and {:?}",
                    fam_a.codec,
                    fam_b.codec
                );
            }
        }
    }
}
