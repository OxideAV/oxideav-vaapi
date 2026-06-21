//! Round 3 integration tests — exercise the decode pipeline plumbing
//! (config + context + RT-format query) against a real driver.
//!
//! Each test is **skip-friendly**: if `Display::open_drm` returns
//! `Err(VaError::Init { .. })` (no `*_drv_video.so` for the GPU on
//! this box, or no `/dev/dri/renderD128` at all) the test logs and
//! returns. On the dev box this crate is developed on
//! (RTX 5080 + nvidia-vaapi-driver shim) the success path runs and
//! all assertions fire.
//!
//! Tier A goals (all three of these must land before Tier B):
//!
//! 1. `h264_high_decode_config_creates` — `Config::new` works for
//!    `(VAProfileH264High, VAEntrypointVLD)`.
//! 2. `h264_high_decode_context_creates` — config + 1 surface +
//!    context create succeeds at coded size 1920x1088.
//! 3. `h264_high_supports_yuv420_render_target` — query
//!    `VAConfigAttribRTFormat` and assert `VA_RT_FORMAT_YUV420`
//!    (bit 0x01) is set.

#![cfg(target_os = "linux")]

use std::path::Path;

use oxideav_vaapi::sys::{attrib, entrypoint, profile, VA_INVALID_ID, VA_RT_FORMAT_YUV420};
use oxideav_vaapi::{config, Config, Context, Display, VaError};

const RENDER_NODE: &str = "/dev/dri/renderD128";

fn open_display_or_skip() -> Option<Display> {
    if !Path::new(RENDER_NODE).exists() {
        eprintln!("skipping: {RENDER_NODE} not present");
        return None;
    }
    match Display::open_drm(Path::new(RENDER_NODE)) {
        Ok(dpy) => Some(dpy),
        Err(VaError::Init { status, message }) => {
            eprintln!(
                "skipping: vaInitialize failed (no driver for this GPU): \
                 status={status} message='{message}'"
            );
            None
        }
        Err(other) => panic!("Display::open_drm: expected Ok or Err(VaError::Init); got {other:?}"),
    }
}

#[test]
fn h264_high_decode_config_creates() {
    let Some(dpy) = open_display_or_skip() else {
        return;
    };

    let cfg = Config::new(
        &dpy,
        profile::VAProfileH264High,
        entrypoint::VAEntrypointVLD,
        &[],
    )
    .expect("Config::new for (H264High, VLD) on a working VA-API driver");

    // The config id is opaque, but a real VAConfigID is non-zero on
    // every driver we've seen. Spot-check that we got something
    // non-trivially populated.
    assert_ne!(cfg.id(), 0, "vaCreateConfig returned id 0");
    assert_eq!(cfg.profile(), profile::VAProfileH264High);
    assert_eq!(cfg.entrypoint(), entrypoint::VAEntrypointVLD);
}

#[test]
fn h264_high_decode_context_creates() {
    let Some(dpy) = open_display_or_skip() else {
        return;
    };

    let cfg = Config::new(
        &dpy,
        profile::VAProfileH264High,
        entrypoint::VAEntrypointVLD,
        &[],
    )
    .expect("Config::new");

    // 1080-line content is decoded into a 1088-line surface
    // (16-pixel MB alignment). One render target is enough to prove
    // the create path works end-to-end.
    let ctx = Context::new(&dpy, &cfg, 1920, 1088, 1)
        .expect("Context::new for 1920x1088 single-surface H.264 decode context");

    assert_ne!(ctx.id(), 0, "vaCreateContext returned id 0");
    assert_eq!(ctx.dimensions(), (1920, 1088));
    assert_eq!(
        ctx.surfaces().len(),
        1,
        "context should own exactly the one surface we requested"
    );
    assert_ne!(ctx.surfaces()[0], VA_INVALID_ID, "surface id is invalid");
}

#[test]
fn h264_high_supports_yuv420_render_target() {
    let Some(dpy) = open_display_or_skip() else {
        return;
    };

    let value = config::get_attribute(
        dpy.raw(),
        profile::VAProfileH264High,
        entrypoint::VAEntrypointVLD,
        attrib::VAConfigAttribRTFormat,
    )
    .expect("vaGetConfigAttributes(RTFormat) on H264High/VLD")
    .expect("driver should advertise an RTFormat for H264High/VLD");

    assert!(
        value & VA_RT_FORMAT_YUV420 != 0,
        "VAConfigAttribRTFormat = 0x{value:08x} should include \
         VA_RT_FORMAT_YUV420 (0x01) for H.264 High decode"
    );
}
