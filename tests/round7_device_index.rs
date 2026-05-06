//! Round 7: `CodecParameters::device_index` plumbing.
//!
//! Phase 1 added an `Option<u32>` device selector to
//! [`oxideav_core::CodecParameters`]. This crate's H.264 decoder
//! factory now reads it and opens the matching DRM render node
//! using the same enumeration order [`crate::engine::engine_info`]
//! reports — so users can correlate the device-block index they
//! see in `oxideav info <codec>` with the value they pass via
//! `with_device_index`.
//!
//! Skip-friendly: tests that need a specific device count (e.g. the
//! "device 1 = Intel iHD" check) skip when `engine_info()` reports
//! fewer than that many working devices.

#![cfg(target_os = "linux")]
#![cfg(feature = "registry")]

use oxideav_core::{CodecId, CodecParameters, MediaType};
use oxideav_vaapi::{device_path_for_index, engine_info};

/// `device_index = None` (the default) → factory opens device 0,
/// which on this box is `/dev/dri/renderD128` (nvidia-vaapi-driver
/// in this developer setup, anything VA-API-capable in CI).
#[test]
fn device_index_none_opens_first_working_device() {
    let devs = engine_info();
    if devs.is_empty() {
        eprintln!("no working VA-API devices — skip");
        return;
    }
    // device_index defaults to None on a fresh CodecParameters.
    let params = CodecParameters::video(CodecId::new("h264"));
    assert!(
        params.device_index.is_none(),
        "device_index defaults to None"
    );
    assert_eq!(params.media_type, MediaType::Video);

    // None → factory must accept (treated as 0). Calling the factory
    // directly avoids needing the codec registry to be wired up.
    let res = oxideav_vaapi::decoder::h264_decoder_factory(&params);
    assert!(
        res.is_ok(),
        "factory should accept default params with device_index=None: {:?}",
        res.err()
    );

    // The path device 0 resolves to must match the path engine_info
    // reports for its first device's `dri_node` extra entry.
    let path0 = device_path_for_index(0).expect("device 0 resolves");
    let reported = devs[0]
        .extra
        .iter()
        .find(|(k, _)| k == "dri_node")
        .map(|(_, v)| v.clone())
        .expect("engine_info() reports a dri_node");
    assert_eq!(
        path0.to_string_lossy(),
        reported,
        "device_path_for_index(0) must match engine_info()[0].extra[\"dri_node\"]",
    );
    eprintln!("device 0 resolves to {} (matches engine_info)", reported);
}

/// `device_index = 1` → factory opens device 1. On the dev box this
/// is `/dev/dri/renderD129` (Intel iHD). Skip when fewer than 2
/// working devices are present (CI / nvidia-only host).
#[test]
fn device_index_one_opens_second_device() {
    let devs = engine_info();
    if devs.len() < 2 {
        eprintln!(
            "only {} working VA-API device(s) — skip (need ≥2 for device_index=1)",
            devs.len()
        );
        return;
    }

    let path1 = device_path_for_index(1).expect("device 1 resolves");
    let reported = devs[1]
        .extra
        .iter()
        .find(|(k, _)| k == "dri_node")
        .map(|(_, v)| v.clone())
        .expect("engine_info() reports a dri_node for device 1");
    assert_eq!(
        path1.to_string_lossy(),
        reported,
        "device_path_for_index(1) must match engine_info()[1].extra[\"dri_node\"]",
    );

    // The factory must build a decoder bound to that device.
    let params = CodecParameters::video(CodecId::new("h264")).with_device_index(1);
    assert_eq!(params.device_index, Some(1));
    let res = oxideav_vaapi::decoder::h264_decoder_factory(&params);
    assert!(
        res.is_ok(),
        "factory should accept device_index=1 when ≥2 working devices: {:?}",
        res.err()
    );
    eprintln!(
        "device 1 resolves to {} — engine_info says: {:?}",
        reported, devs[1].name
    );
}

/// `device_index = 99` → factory must error (out-of-range), not
/// silently fall back to device 0. Always runs, even in environments
/// with no VA-API stack at all (the call returns Err either way).
#[test]
fn device_index_out_of_range_errors() {
    let path = device_path_for_index(99);
    assert!(
        path.is_err(),
        "device_path_for_index(99) must error on every host"
    );
    if let Err(e) = path {
        eprintln!("expected error: {e}");
    }

    let params = CodecParameters::video(CodecId::new("h264")).with_device_index(99);
    let res = oxideav_vaapi::decoder::h264_decoder_factory(&params);
    assert!(
        res.is_err(),
        "factory must reject out-of-range device_index, not fall back to 0"
    );
    if let Err(e) = res {
        eprintln!("factory returned: {e}");
    }
}
