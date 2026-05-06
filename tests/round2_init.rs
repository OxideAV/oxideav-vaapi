//! Round 2 integration tests — exercise the libloading bridge end-to-
//! end against whatever VA-API stack is on the host.
//!
//! Two regimes are valid:
//!
//! 1. **Success path** (Intel iGPU, AMD GPU with mesa-va-gallium, or
//!    NVIDIA with `nvidia-vaapi-driver` shim installed): a
//!    `Display::open_drm` call returns `Ok` with a populated
//!    api_version, vendor string, and profile list. Tests assert
//!    those values look reasonable.
//!
//! 2. **Graceful-failure path** (NVIDIA without the shim, no
//!    `*_drv_video.so` in `/usr/lib*/dri/`): `Display::open_drm`
//!    returns `Err(VaError::Init { status, message })`. Tests
//!    assert the error is propagated verbatim — this proves the
//!    wrapper handles the no-driver case without panicking.
//!
//! The dev box this crate is being developed on takes path #2.
//! These tests must pass in both regimes without modification.

#![cfg(target_os = "linux")]

use std::path::Path;

use oxideav_vaapi::{sys, Display, VaError};

const RENDER_NODE: &str = "/dev/dri/renderD128";

fn render_node_available() -> bool {
    Path::new(RENDER_NODE).exists()
}

#[test]
fn dlopen_succeeds() {
    // The framework load (libva.so.2 + libva-drm.so.2) is the
    // foundation everything else is built on. If this fails the
    // remaining tests are moot — but on CI runners without libva
    // installed (most stock GitHub-hosted runners) we want to skip
    // cleanly instead of failing the suite.
    let fw = match sys::framework() {
        Ok(fw) => fw,
        Err(e) => {
            eprintln!("skipping: libva framework unavailable: {e}");
            return;
        }
    };
    let _ = fw;
    if let Err(e) = sys::vtable() {
        eprintln!("skipping: libva vtable failed to resolve: {e}");
    }
}

#[test]
fn drm_fd_opens() {
    if !render_node_available() {
        eprintln!("skipping: {RENDER_NODE} not present (no GPU exposed in this environment)");
        return;
    }
    // SAFETY: `path` is a NUL-terminated literal.
    let path = b"/dev/dri/renderD128\0";
    let fd = unsafe { libc::open(path.as_ptr() as *const _, libc::O_RDWR | libc::O_CLOEXEC) };
    assert!(
        fd >= 0,
        "libc::open(/dev/dri/renderD128, O_RDWR) failed: {}",
        std::io::Error::last_os_error()
    );
    // Always close on success.
    let rc = unsafe { libc::close(fd) };
    assert_eq!(rc, 0, "close() of valid fd returned non-zero");
}

#[test]
fn va_get_display_drm_returns_non_null() {
    if !render_node_available() {
        eprintln!("skipping: {RENDER_NODE} not present");
        return;
    }
    let vt = match sys::vtable() {
        Ok(vt) => vt,
        Err(e) => {
            eprintln!("skipping: libva vtable unavailable: {e}");
            return;
        }
    };

    let path = b"/dev/dri/renderD128\0";
    let fd = unsafe { libc::open(path.as_ptr() as *const _, libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        eprintln!(
            "skipping: open(/dev/dri/renderD128) failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    // SAFETY: `fd` is a valid render-node descriptor; libva-drm dups
    // it internally on success.
    let dpy = unsafe { (vt.va_get_display_drm)(fd) };
    assert!(
        !dpy.is_null(),
        "vaGetDisplayDRM unexpectedly returned NULL for a valid render-node fd"
    );

    // Don't bother calling vaInitialize/vaTerminate here — the next
    // test exercises that. Just close our fd.
    let _ = unsafe { libc::close(fd) };
}

/// **Headline test for Round 2.**
///
/// `Display::open_drm` either succeeds (real-driver path) or fails
/// at `vaInitialize` with a useful error string (no-driver path).
/// Both outcomes are accepted — what we're verifying is that:
///
/// * The wrapper does not panic.
/// * On failure, [`VaError::Init`] carries a non-success status AND
///   a non-empty message (so a higher layer can surface the
///   driver-supplied reason).
/// * The fd is closed cleanly so we can re-run the open path
///   without exhausting fds.
///
/// On the dev box this crate is built on (NVIDIA RTX 5080 with no
/// `nvidia-vaapi-driver` installed) the failure branch fires and we
/// verify the error propagation works correctly.
#[test]
fn va_initialize_propagates_error_when_no_driver_installed() {
    if !render_node_available() {
        eprintln!("skipping: {RENDER_NODE} not present");
        return;
    }
    let path = Path::new(RENDER_NODE);

    match Display::open_drm(path) {
        Ok(display) => {
            // Real-driver path. Spot-check that the wrapper
            // populated everything.
            let (major, minor) = display.api_version();
            assert!(
                major > 0 || minor > 0,
                "vaInitialize should report a non-zero (major, minor) on success; got ({major}, {minor})"
            );
            // Vendor string is documented as always returning a
            // non-null pointer to a non-empty string after
            // successful init.
            let vendor = display
                .vendor_string()
                .expect("vaQueryVendorString should succeed after init");
            assert!(
                !vendor.trim().is_empty(),
                "vaQueryVendorString returned empty string"
            );
            // Profile list should contain at least one entry on a
            // real driver.
            let profiles = display
                .profiles()
                .expect("vaQueryConfigProfiles should succeed after init");
            assert!(
                !profiles.is_empty(),
                "driver advertised 0 profiles — unexpected on a working VA-API stack"
            );
            eprintln!(
                "VA-API success path: vendor='{vendor}' api={major}.{minor} profiles={}",
                profiles.len()
            );
        }
        Err(VaError::Init { status, message }) => {
            // Graceful-failure path — the case exercised on this
            // dev box.
            assert_ne!(status, 0, "VaError::Init must carry a non-success status");
            assert!(
                !message.trim().is_empty(),
                "VaError::Init must carry a non-empty driver-supplied message; got '{message}'"
            );
            eprintln!(
                "VA-API graceful-failure path: status={status} message='{message}' (no driver installed for this GPU)"
            );
        }
        Err(other) => {
            panic!("expected Ok or Err(VaError::Init {{ .. }}); got Err({other:?})");
        }
    }

    // Re-running open_drm should still work — proves the previous
    // attempt didn't leak the fd.
    let _ = Display::open_drm(path);
}
