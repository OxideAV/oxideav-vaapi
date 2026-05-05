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
use crate::display::{Display, VaProfile};
use crate::sys::{attrib, entrypoint, profile};

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

    let codecs = collect_codecs(&dpy);

    Some(HwDeviceInfo {
        name,
        driver_version: None,
        api_version,
        total_memory_bytes: None,
        extra,
        codecs,
    })
}

/// One row in the codec-family table. Each entry maps a codec id (the
/// string `oxideav_core::CodecId` uses) to the set of
/// [`VaProfile`]-encoded profiles that belong to the family. The
/// "headline" profile — the one used to query `MaxPictureWidth` /
/// `MaxPictureHeight` — is the last entry in `profiles` (highest
/// profile in the family); see [`collect_codecs`] for how that's used.
struct CodecFamily {
    codec: &'static str,
    /// Family profiles in ascending capability order. The last one
    /// is treated as the headline profile for max-dim queries.
    profiles: &'static [i32],
}

const CODEC_FAMILIES: &[CodecFamily] = &[
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
        profiles: &[
            profile::VAProfileAV1Profile0,
            profile::VAProfileAV1Profile1,
        ],
    },
    CodecFamily {
        codec: "vp8",
        profiles: &[profile::VAProfileVP8Version0_3],
    },
    CodecFamily {
        codec: "vp9",
        profiles: &[
            profile::VAProfileVP9Profile0,
            profile::VAProfileVP9Profile2,
        ],
    },
    CodecFamily {
        codec: "mpeg2",
        profiles: &[
            profile::VAProfileMPEG2Simple,
            profile::VAProfileMPEG2Main,
        ],
    },
    CodecFamily {
        codec: "vc1",
        profiles: &[
            profile::VAProfileVC1Simple,
            profile::VAProfileVC1Main,
            profile::VAProfileVC1Advanced,
        ],
    },
];

/// Build the per-codec capability matrix for one device.
///
/// We pull the full advertised profile list once via
/// [`Display::profiles`] and intersect it with each family table —
/// any family with no advertised profiles is omitted from the result
/// entirely. For families that do have at least one advertised
/// profile, the resulting [`HwCodecCaps`] reports decode/encode flags
/// (any-of-family) and max dims (queried on the headline profile that
/// is actually advertised).
fn collect_codecs(dpy: &Display) -> Vec<HwCodecCaps> {
    let advertised = match dpy.profiles() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for fam in CODEC_FAMILIES {
        let present: Vec<VaProfile> = fam
            .profiles
            .iter()
            .copied()
            .filter(|p| advertised.iter().any(|a| a.raw() == *p))
            .map(VaProfile)
            .collect();
        if present.is_empty() {
            continue;
        }

        // any-of-family flags
        let mut decode = false;
        let mut encode = false;
        for p in &present {
            if dpy.is_supported(*p, entrypoint::VAEntrypointVLD) {
                decode = true;
            }
            if dpy.is_supported(*p, entrypoint::VAEntrypointEncSlice)
                || dpy.is_supported(*p, entrypoint::VAEntrypointEncSliceLP)
            {
                encode = true;
            }
        }

        // Query MaxPictureWidth / MaxPictureHeight on the highest
        // present profile that the driver actually advertises VLD
        // for; if the family is encode-only (rare), fall back to
        // EncSlice. If neither is advertised we drop the dims
        // entirely — a profile with no entrypoint isn't queryable.
        let (max_width, max_height) = present
            .iter()
            .rev()
            .find_map(|p| pick_dims(dpy, *p, decode, encode))
            .unwrap_or((None, None));

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

/// Try to read `MaxPictureWidth` / `MaxPictureHeight` for `profile`
/// on the most informative entrypoint advertised for it. Returns
/// `Some((width, height))` on a successful query (either field may
/// itself be `None` if the driver returns `VA_ATTRIB_NOT_SUPPORTED`),
/// or `None` if no entrypoint is available.
fn pick_dims(
    dpy: &Display,
    profile: VaProfile,
    family_decode: bool,
    family_encode: bool,
) -> Option<(Option<u32>, Option<u32>)> {
    // Pick the entrypoint we can actually ask about for this profile.
    let ep = if family_decode && dpy.is_supported(profile, entrypoint::VAEntrypointVLD) {
        entrypoint::VAEntrypointVLD
    } else if family_encode && dpy.is_supported(profile, entrypoint::VAEntrypointEncSlice) {
        entrypoint::VAEntrypointEncSlice
    } else if family_encode && dpy.is_supported(profile, entrypoint::VAEntrypointEncSliceLP) {
        entrypoint::VAEntrypointEncSliceLP
    } else {
        return None;
    };

    // A driver that doesn't honour MaxPictureWidth often answers
    // `0` rather than `VA_ATTRIB_NOT_SUPPORTED` (this is observed on
    // `nvidia-vaapi-driver 0.0.16`, which advertises every profile
    // but doesn't surface a max-dim figure). Treat both as "unknown"
    // so the consumer gets `None` instead of a misleading `Some(0)`.
    let w = config::get_attribute(
        dpy.raw(),
        profile.raw(),
        ep,
        attrib::VAConfigAttribMaxPictureWidth,
    )
    .ok()
    .flatten()
    .filter(|v| *v != 0);
    let h = config::get_attribute(
        dpy.raw(),
        profile.raw(),
        ep,
        attrib::VAConfigAttribMaxPictureHeight,
    )
    .ok()
    .flatten()
    .filter(|v| *v != 0);
    Some((w, h))
}

