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
