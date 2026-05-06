//! Round 6: `engine_info()` — DRI render-node enumeration + per-codec
//! capability matrix.
//!
//! Skip-friendly: when no VA-API render node is available (sandboxed
//! CI, no GPU stack installed) the probe returns an empty `Vec` and
//! the headline test exits early. The dev box (RTX 5080 +
//! `nvidia-vaapi-driver 0.0.16`) sees `/dev/dri/renderD128` and
//! `/dev/dri/renderD129`, both reporting the NVDEC vendor string.

#![cfg(target_os = "linux")]
#![cfg(feature = "registry")]

#[test]
fn engine_info_finds_render_node_or_skips() {
    let devs = oxideav_vaapi::engine_info();
    if devs.is_empty() {
        eprintln!("No VA-API render nodes available — skip");
        return;
    }
    eprintln!("found {} VA-API device(s)", devs.len());
    let dev = &devs[0];
    eprintln!("device 0: {:?}", dev);
    assert!(!dev.name.is_empty(), "vendor string non-empty");
    assert!(dev.api_version.is_some(), "API version reported");
    let h264 = dev.codecs.iter().find(|c| c.codec == "h264");
    assert!(h264.is_some(), "h264 entry present");
    assert!(h264.unwrap().decode, "h264 decode advertised");
}

#[test]
fn engine_info_does_not_panic_when_called_twice() {
    let _ = oxideav_vaapi::engine_info();
    let _ = oxideav_vaapi::engine_info();
}

/// On a host with a working Intel iHD render node (the dev box has
/// one at `/dev/dri/renderD129`), the per-device h264 capability row
/// must surface real `max_width`/`max_height` numbers from
/// `vaGetConfigAttributes(VAConfigAttribMaxPictureWidth/Height)`. Skip
/// if no Intel device is present (CI / nvidia-only host).
#[test]
fn engine_info_reports_max_dims_for_intel_h264() {
    let devs = oxideav_vaapi::engine_info();
    let intel = devs.iter().find(|d| {
        // The Intel iHD vendor string starts with "Intel iHD driver"
        // — match on a stable substring rather than the full string
        // (the version suffix changes between releases).
        d.name.contains("Intel iHD")
    });
    let Some(intel) = intel else {
        eprintln!(
            "No Intel iHD VA-API device on this host — skip. \
             Devices found: {:?}",
            devs.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
        return;
    };
    eprintln!("Intel device: {:?}", intel);
    let h264 = intel
        .codecs
        .iter()
        .find(|c| c.codec == "h264")
        .expect("Intel iHD must advertise h264");
    assert!(h264.decode, "Intel iHD must advertise h264 decode");
    assert!(
        h264.max_width.is_some(),
        "Intel iHD must report a max_width via vaGetConfigAttributes (got None)"
    );
    assert!(
        h264.max_height.is_some(),
        "Intel iHD must report a max_height via vaGetConfigAttributes (got None)"
    );
    let w = h264.max_width.unwrap();
    let h = h264.max_height.unwrap();
    eprintln!("Intel iHD h264 max dims: {w}x{h}");
    // Sanity bounds — Intel Gen Graphics typically reports 4096-8192
    // for h264. Reject zero (sentinel for "not implemented") and any
    // absurd value.
    assert!(w >= 1024 && w <= 16384, "max_width={w} outside sane range");
    assert!(h >= 1024 && h <= 16384, "max_height={h} outside sane range");
}
