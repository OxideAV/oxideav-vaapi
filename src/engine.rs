//! Round 6: per-device engine probe.
//!
//! Implements [`engine_info`] — the function that walks the host's
//! DRM render-node range, opens each one through libva, queries its
//! profile / entrypoint matrix, and converts the result into the
//! sibling-agnostic shape declared by `oxideav_core::engine`
//! ([`HwDeviceInfo`] + [`HwCodecCaps`]).
//!
//! The probe is **idempotent and side-effect free**: every call walks
//! the render nodes again from scratch and constructs a fresh
//! [`Display`] per node. Failures (driver not loaded, render node
//! absent, profile-list query refused) are skipped silently — the
//! function returns an empty `Vec` on a sandbox / no-stack host
//! rather than propagating an error, matching the contract documented
//! in [`oxideav_core::engine`].
//!
//! # Render-node range
//!
//! The Linux DRM convention numbers render nodes
//! `/dev/dri/renderD128`..`/dev/dri/renderD191` (`128 + minor`, with a
//! cap of 64 nodes). We walk that whole range and skip any node that
//! doesn't exist on disk; this is cheap and avoids hardcoding a single
//! node when a multi-GPU host (this dev box has two) exposes more.
//!
//! # Codec families
//!
//! The `(VAProfile, VAEntrypoint)` matrix is denormalised into one
//! [`HwCodecCaps`] entry per codec family (h264 / hevc / av1 / vp8 /
//! vp9 / mpeg2 / vc1). For each family we collect:
//!
//! * `decode` — true if any family profile advertises `VAEntrypointVLD`.
//! * `encode` — true if any family profile advertises `VAEntrypointEncSlice`
//!   or `VAEntrypointEncSliceLP`.
//! * `max_width` / `max_height` — read via `vaGetConfigAttributes` on
//!   the family's "highest" decode profile (e.g. H.264 High, HEVC
//!   Main); falls back to `None` when the driver returns
//!   `VA_ATTRIB_NOT_SUPPORTED`.
//! * `profiles` — the [`VaProfile::name`] strings of every family
//!   profile present.

use std::path::{Path, PathBuf};

use oxideav_core::engine::{HwCodecCaps, HwDeviceInfo};

use crate::config;
use crate::display::{Display, EntrypointMatrix, VaError, VaProfile};
use crate::profiles::KNOWN_CODECS;
use crate::sys::{attrib, entrypoint};

/// Range of DRM render-node minors we attempt to probe. Linux assigns
/// render nodes starting at minor 128 and the hard upper bound is 191
/// (64 nodes total, see `drivers/gpu/drm/drm_drv.c`).
const RENDER_MINOR_FIRST: u32 = 128;
const RENDER_MINOR_LAST: u32 = 191;

/// Walk every DRM render node, open each one through libva, and
/// report a [`HwDeviceInfo`] entry for every node that initialised
/// successfully.
///
/// Failure-tolerant by construction:
///
/// * Render nodes that don't exist on disk are skipped silently.
/// * Render nodes whose libva driver doesn't load are skipped (this
///   is the no-driver-installed case — the caller gets an empty
///   `Vec` rather than a fatal error).
/// * Per-codec capability queries that the driver refuses (e.g.
///   `VA_ATTRIB_NOT_SUPPORTED` for `MaxPictureWidth`) collapse to
///   `None` in the corresponding [`HwCodecCaps`] field.
///
/// Returned in render-minor order (128, 129, …) so multi-GPU hosts
/// produce a stable enumeration across calls.
pub fn engine_info() -> Vec<HwDeviceInfo> {
    let mut out = Vec::new();
    for minor in RENDER_MINOR_FIRST..=RENDER_MINOR_LAST {
        let path = PathBuf::from(format!("/dev/dri/renderD{minor}"));
        if !path.exists() {
            continue;
        }
        if let Some(info) = probe_node(&path) {
            out.push(info);
        }
    }
    out
}

/// Resolve a 0-based [`oxideav_core::CodecParameters::device_index`]
/// to the DRM render-node path the decoder factory should open.
///
/// Walks `/dev/dri/renderD128`..`renderD191` in order, attempts
/// [`Display::open_drm`] on each existing path, and returns the
/// `index`-th path that opened cleanly. The walk and filter logic
/// mirrors [`engine_info`] exactly so consumers can correlate the
/// device-block indices printed by `info <codec>` with the value
/// they pass via `with_device_index`.
///
/// Failure modes:
///
/// * `index >= number of working devices` →
///   [`VaError::Init`] with a status of `0` and a descriptive
///   `message` (the closest fit in the existing error taxonomy —
///   the alternative would require a new variant for "no such
///   device" and that buys nothing for callers).
///
/// O(N) per construction where N is the number of working render
/// nodes (typically 1-3 on real hosts). No caching: the renderD*
/// node set is stable across a process lifetime in practice but
/// rebuilding lets the function stay side-effect free, matching
/// [`engine_info`]'s contract.
pub fn device_path_for_index(index: u32) -> Result<PathBuf, VaError> {
    let mut working = 0u32;
    let mut total_existing = 0u32;
    for minor in RENDER_MINOR_FIRST..=RENDER_MINOR_LAST {
        let path = PathBuf::from(format!("/dev/dri/renderD{minor}"));
        if !path.exists() {
            continue;
        }
        total_existing += 1;
        // Try to init libva on this node — if it works, count it.
        // The Display is dropped immediately so we don't hold the
        // VADisplay open across the walk.
        match Display::open_drm(&path) {
            Ok(_dpy) => {
                if working == index {
                    return Ok(path);
                }
                working += 1;
            }
            Err(_) => {
                // Skip nodes whose libva driver doesn't bind — same
                // policy as `engine_info`.
            }
        }
    }
    Err(VaError::Init {
        status: 0,
        message: format!(
            "device_index {index} out of range: only {working} working VA-API \
             device(s) on this host ({total_existing} render node(s) probed)"
        ),
    })
}

/// Open a single render node, build its [`HwDeviceInfo`], and return
/// `None` when libva refuses to talk to the driver behind the node.
fn probe_node(path: &Path) -> Option<HwDeviceInfo> {
    let dpy = Display::open_drm(path).ok()?;

    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("renderD?");

    // Vendor string + node basename so multi-node hosts where every
    // node reports the same vendor (this dev box: two render nodes,
    // same nvidia-vaapi-driver shim) still produce a unique name.
    let vendor = dpy
        .vendor_string()
        .unwrap_or_else(|_| "unknown VA-API driver".to_string());
    let name = format!("{vendor} ({basename})");

    let (major, minor) = dpy.api_version();
    let api_version = Some(format!("VA-API {major}.{minor}"));

    let extra = vec![("dri_node".to_string(), path.display().to_string())];

    // Build the (profile, [entrypoints]) matrix once and let
    // `collect_codecs` consult it without re-issuing
    // `vaQueryConfigEntrypoints` for every `(family, entrypoint)` pair.
    // On a 25-profile driver with 7 codec families this collapses
    // ~50 FFI calls per device down to ~25.
    let matrix = match dpy.entrypoint_matrix() {
        Ok(m) => m,
        Err(_) => return None,
    };
    let codecs = collect_codecs(&dpy, &matrix);

    Some(HwDeviceInfo {
        name,
        driver_version: None,
        api_version,
        total_memory_bytes: None,
        extra,
        codecs,
    })
}

/// Build the per-codec capability matrix for one device.
///
/// Round 9: takes a pre-built [`EntrypointMatrix`] so the `(profile,
/// entrypoint)` membership checks below issue zero FFI calls. The
/// caller (`probe_node`) constructs the matrix once per device; this
/// function then iterates [`KNOWN_CODECS`], intersects each family
/// with the matrix's advertised profiles, and builds the
/// [`HwCodecCaps`] entries.
///
/// The codec-family table lives in [`crate::profiles::KNOWN_CODECS`]
/// — see that module for the codec id ↔ `VAProfile` mapping.
fn collect_codecs(dpy: &Display, matrix: &EntrypointMatrix) -> Vec<HwCodecCaps> {
    let mut out = Vec::new();
    for fam in KNOWN_CODECS {
        let present: Vec<VaProfile> = fam
            .profiles
            .iter()
            .copied()
            .filter(|p| !matrix.entrypoints_for(VaProfile(*p)).is_empty())
            .map(VaProfile)
            .collect();
        if present.is_empty() {
            continue;
        }

        // any-of-family flags — all served from the matrix, no FFI.
        let decode = matrix.any_supports(fam.profiles, entrypoint::VAEntrypointVLD);
        let encode = matrix.any_supports(fam.profiles, entrypoint::VAEntrypointEncSlice)
            || matrix.any_supports(fam.profiles, entrypoint::VAEntrypointEncSliceLP);

        // Query MaxPictureWidth / MaxPictureHeight across every
        // (profile, entrypoint) pair the driver advertises in this
        // family, and surface the maximum reported value. Walking the
        // whole matrix (rather than just the headline profile) is
        // important because some drivers report 0 / `VA_ATTRIB_NOT_SUPPORTED`
        // for one entrypoint but real numbers for the other — taking
        // the max across both is the conservative choice.
        let (max_width, max_height) = max_dims_across(dpy, matrix, &present);

        let profiles = present.iter().map(|p| p.name()).collect::<Vec<_>>();

        out.push(HwCodecCaps {
            codec: fam.codec.to_string(),
            decode,
            encode,
            max_width,
            max_height,
            max_bit_depth: None,
            profiles,
            extra: Vec::new(),
        });
    }
    out
}

/// Walk every `(profile, entrypoint)` pair advertised for this codec
/// family and return the maximum reported `MaxPictureWidth` /
/// `MaxPictureHeight` across all of them.
///
/// Round 9: uses the pre-built [`EntrypointMatrix`] for the membership
/// check so the only FFI traffic per pair is the
/// [`vaGetConfigAttributes`] call itself — no
/// [`vaQueryConfigEntrypoints`] retries.
///
/// The libva spec says `vaGetConfigAttributes` returns
/// `VA_ATTRIB_NOT_SUPPORTED = 0x80000000` for attributes the driver
/// doesn't implement; in practice some drivers (notably the NVDEC
/// libva shim at 0.0.16) write `0` instead. We treat both as
/// "unknown" — the value is only contributed to the returned max if
/// it's strictly positive.
///
/// Returns `(None, None)` if no entrypoint reports a real number for
/// either dimension.
fn max_dims_across(
    dpy: &Display,
    matrix: &EntrypointMatrix,
    profiles: &[VaProfile],
) -> (Option<u32>, Option<u32>) {
    const ENTRYPOINTS: &[i32] = &[
        entrypoint::VAEntrypointVLD,
        entrypoint::VAEntrypointEncSlice,
        entrypoint::VAEntrypointEncSliceLP,
    ];

    let mut max_w: Option<u32> = None;
    let mut max_h: Option<u32> = None;

    for p in profiles {
        for ep in ENTRYPOINTS {
            if !matrix.is_supported(*p, *ep) {
                continue;
            }
            // Driver advertises this `(profile, entrypoint)` pair —
            // ask for both width and height. Errors are silenced
            // because libva can return UNSUPPORTED on a profile that
            // is technically advertised but won't compile a config;
            // the caller wants the best available answer, not an error.
            if let Ok(Some(v)) = config::get_attribute(
                dpy.raw(),
                p.raw(),
                *ep,
                attrib::VAConfigAttribMaxPictureWidth,
            ) {
                if v != 0 {
                    max_w = Some(max_w.map_or(v, |cur| cur.max(v)));
                }
            }
            if let Ok(Some(v)) = config::get_attribute(
                dpy.raw(),
                p.raw(),
                *ep,
                attrib::VAConfigAttribMaxPictureHeight,
            ) {
                if v != 0 {
                    max_h = Some(max_h.map_or(v, |cur| cur.max(v)));
                }
            }
        }
    }

    (max_w, max_h)
}
