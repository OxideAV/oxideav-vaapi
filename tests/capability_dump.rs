//! Diagnostic capability dump — `--ignored` so it only runs on demand.
//!
//! Run with:
//!
//! ```text
//! cargo test -p oxideav-vaapi --test capability_dump \
//!     -- --ignored --nocapture
//! ```
//!
//! Prints, for the VA-API driver loaded for `/dev/dri/renderD128`:
//!
//! * vendor string, negotiated API version,
//! * each `(profile, entrypoint)` pair the driver advertises,
//! * the supported render-target formats for `(profile, VLD)` where
//!   applicable,
//! * a summary of which profiles support **encode** vs **decode**.
//!
//! On `nvidia-vaapi-driver 0.0.16` (RTX 5080 dev box) the encode summary
//! always reports zero, because that driver only wraps NVDEC. This test
//! is the canonical way to confirm that fact on a given host.

#![cfg(target_os = "linux")]

use std::path::Path;

use oxideav_vaapi::sys::{attrib, entrypoint, profile};
use oxideav_vaapi::{config, Display, VaError, VaProfile};

const RENDER_NODE: &str = "/dev/dri/renderD128";

fn entry_name(e: i32) -> &'static str {
    match e {
        1 => "VLD",
        2 => "IZZ",
        3 => "IDCT",
        4 => "MoComp",
        5 => "Deblocking",
        6 => "EncSlice",
        7 => "EncPicture",
        8 => "EncSliceLP",
        10 => "VideoProc",
        11 => "FEI",
        12 => "Stats",
        _ => "?",
    }
}

#[test]
#[ignore]
fn capability_dump() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("skipping: {RENDER_NODE} not present");
        return;
    }
    let dpy = match Display::open_drm(Path::new(RENDER_NODE)) {
        Ok(d) => d,
        Err(VaError::Init { status, message }) => {
            eprintln!("skipping (no driver): status={status} '{message}'");
            return;
        }
        Err(e) => panic!("unexpected: {e:?}"),
    };

    let vendor = dpy.vendor_string().unwrap_or_default();
    let (major, minor) = dpy.api_version();
    eprintln!("\n══ VA-API capability dump ══");
    eprintln!("vendor : {vendor}");
    eprintln!("api    : {major}.{minor}");

    let profiles = dpy.profiles().expect("profiles");
    eprintln!("profile count: {}", profiles.len());

    let mut decode_count = 0;
    let mut encode_count = 0;

    for p in &profiles {
        let entries = match dpy.entrypoints(*p) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("  {} ({}): query failed: {e}", p.name(), p.raw());
                continue;
            }
        };
        let names: Vec<String> = entries
            .iter()
            .map(|&e| format!("{}({})", entry_name(e), e))
            .collect();
        let has_decode = entries.contains(&entrypoint::VAEntrypointVLD);
        let has_encode = entries.contains(&entrypoint::VAEntrypointEncSlice)
            || entries.contains(&entrypoint::VAEntrypointEncSliceLP);
        if has_decode {
            decode_count += 1;
        }
        if has_encode {
            encode_count += 1;
        }

        // Probe RT-format for VLD profiles where applicable.
        let rt_info = if has_decode {
            match config::get_attribute(
                dpy.raw(),
                p.raw(),
                entrypoint::VAEntrypointVLD,
                attrib::VAConfigAttribRTFormat,
            ) {
                Ok(Some(v)) => format!(", RTFormat=0x{v:08x}"),
                Ok(None) => ", RTFormat=N/A".into(),
                Err(_) => "".into(),
            }
        } else {
            "".into()
        };

        eprintln!(
            "  {:<36} ({:>3}): {:?}{}",
            p.name(),
            p.raw(),
            names,
            rt_info
        );
    }

    eprintln!(
        "\nsummary: {} decode profile(s), {} encode profile(s)",
        decode_count, encode_count
    );
    if encode_count == 0 && vendor.contains("NVDEC") {
        eprintln!(
            "(NVDEC backend confirmed: VA-API encode is structurally \
             unavailable on this driver — for NVENC encode use the \
             oxideav-nvidia crate via NVENC-direct, not VA-API.)"
        );
    }
}

/// Negative-config probe: every Enc* entrypoint should be rejected by
/// `vaCreateConfig` on the NVDEC backend. This is the
/// runtime-truth test for the capability summary above. `#[ignore]`'d
/// because it makes assumptions about the host driver.
#[test]
#[ignore]
fn nvdec_rejects_encode_config() {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("skipping: {RENDER_NODE} not present");
        return;
    }
    let dpy = match Display::open_drm(Path::new(RENDER_NODE)) {
        Ok(d) => d,
        Err(VaError::Init { status, message }) => {
            eprintln!("skipping (no driver): status={status} '{message}'");
            return;
        }
        Err(e) => panic!("unexpected: {e:?}"),
    };
    if !dpy.vendor_string().unwrap_or_default().contains("NVDEC") {
        eprintln!("skipping: not running against the NVDEC backend");
        return;
    }
    for p in [
        profile::VAProfileH264High,
        profile::VAProfileH264Main,
        profile::VAProfileHEVCMain,
    ] {
        for ep in [
            entrypoint::VAEntrypointEncSlice,
            entrypoint::VAEntrypointEncSliceLP,
        ] {
            let r = oxideav_vaapi::Config::new(&dpy, p, ep, &[]);
            match r {
                Ok(_) => panic!(
                    "vaCreateConfig unexpectedly succeeded for \
                     ({p}, {ep}) on NVDEC backend"
                ),
                Err(e) => eprintln!("  ({}, {}) → {e}", VaProfile(p).name(), entry_name(ep)),
            }
        }
    }
}
