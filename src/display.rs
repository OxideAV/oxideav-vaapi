//! Safe wrapper around a VA-API DRM display.
//!
//! The lifecycle is:
//!
//! 1. `libc::open("/dev/dri/renderD128", O_RDWR)` — gets a file
//!    descriptor for the GPU's DRM render node.
//! 2. `vaGetDisplayDRM(fd)` — produces a `VADisplay` handle from the
//!    fd. This is the libva DRM backend's display constructor; it
//!    does NOT touch the driver yet.
//! 3. `vaInitialize(display, &mut major, &mut minor)` — this is where
//!    libva loads the actual `*_drv_video.so` for the chip. If no
//!    driver is installed (the case on this dev box's NVIDIA card
//!    without `nvidia-vaapi-driver`), this step fails with a non-zero
//!    `VAStatus` and a useful error string from `vaErrorStr` —
//!    typically "no driver loaded" or similar.
//!
//! [`Display::open_drm`] performs all three steps and returns a
//! [`VaError::Init`] on the third one if it fails, allowing callers
//! to surface a precise failure. `Drop` always closes the fd; it
//! only calls `vaTerminate` if the init was successful.
//!
//! Thread safety: a `Display` owns the fd + the `VADisplay`; the
//! libva spec says a `VADisplay` is **not** thread-safe and callers
//! must serialize access externally. We don't `impl Send/Sync`.

use std::ffi::CString;
use std::io;
use std::os::raw::c_char;
use std::path::Path;

use crate::sys::{self, error_str, profile, VADisplay, VAStatus, VA_STATUS_SUCCESS};

/// VA-API profile identifier — the wire value the driver speaks.
///
/// The set of supported profiles is enumerated by
/// [`Display::profiles`]; named constants (`VAProfileH264Main` etc.)
/// live in [`crate::sys::profile`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VaProfile(pub i32);

impl VaProfile {
    /// Convenience accessor for the underlying enum value.
    pub fn raw(self) -> i32 {
        self.0
    }

    /// Best-effort static name lookup for the subset of profiles
    /// oxideav cares about. Unknown profiles report `"VAProfile(N)"`.
    pub fn name(self) -> String {
        match self.0 {
            profile::VAProfileNone => "VAProfileNone".into(),
            profile::VAProfileMPEG2Simple => "VAProfileMPEG2Simple".into(),
            profile::VAProfileMPEG2Main => "VAProfileMPEG2Main".into(),
            profile::VAProfileH264Baseline => "VAProfileH264Baseline".into(),
            profile::VAProfileH264Main => "VAProfileH264Main".into(),
            profile::VAProfileH264High => "VAProfileH264High".into(),
            profile::VAProfileVC1Simple => "VAProfileVC1Simple".into(),
            profile::VAProfileVC1Main => "VAProfileVC1Main".into(),
            profile::VAProfileVC1Advanced => "VAProfileVC1Advanced".into(),
            profile::VAProfileJPEGBaseline => "VAProfileJPEGBaseline".into(),
            profile::VAProfileH264ConstrainedBaseline => "VAProfileH264ConstrainedBaseline".into(),
            profile::VAProfileVP8Version0_3 => "VAProfileVP8Version0_3".into(),
            profile::VAProfileHEVCMain => "VAProfileHEVCMain".into(),
            profile::VAProfileHEVCMain10 => "VAProfileHEVCMain10".into(),
            profile::VAProfileVP9Profile0 => "VAProfileVP9Profile0".into(),
            profile::VAProfileVP9Profile2 => "VAProfileVP9Profile2".into(),
            profile::VAProfileHEVCMain12 => "VAProfileHEVCMain12".into(),
            profile::VAProfileHEVCMain444 => "VAProfileHEVCMain444".into(),
            profile::VAProfileHEVCMain444_10 => "VAProfileHEVCMain444_10".into(),
            profile::VAProfileHEVCMain444_12 => "VAProfileHEVCMain444_12".into(),
            profile::VAProfileAV1Profile0 => "VAProfileAV1Profile0".into(),
            profile::VAProfileAV1Profile1 => "VAProfileAV1Profile1".into(),
            n => format!("VAProfile({n})"),
        }
    }
}

/// Errors surfaced by the `Display` wrapper.
#[derive(Debug)]
pub enum VaError {
    /// `libc::open` on the DRM render node failed (no `/dev/dri`,
    /// permission denied, fd table exhausted, …).
    OpenDrm(io::Error),
    /// `vaGetDisplayDRM` returned a null pointer. This shouldn't
    /// happen with a valid render-node fd — it indicates libva-drm
    /// rejected the descriptor outright.
    GetDisplayNull,
    /// dlopen / dlsym of `libva.so.2` / `libva-drm.so.2` or the
    /// vtable resolution failed.
    Sys(String),
    /// `vaInitialize` returned a non-success status.
    ///
    /// **This is the path exercised when no VA-API driver `.so` is
    /// installed for the GPU on the box.** `message` comes verbatim
    /// from `vaErrorStr` so the caller sees the driver-supplied
    /// reason (e.g. `"no driver loaded"`).
    Init { status: VAStatus, message: String },
    /// Generic post-init libva failure (vendor string query, profile
    /// enumeration, etc.) with the resolved error string.
    Va { status: VAStatus, message: String },
}

impl std::fmt::Display for VaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaError::OpenDrm(e) => write!(f, "open DRM render node: {e}"),
            VaError::GetDisplayNull => write!(f, "vaGetDisplayDRM returned null"),
            VaError::Sys(s) => write!(f, "libva runtime: {s}"),
            VaError::Init { status, message } => {
                write!(f, "vaInitialize failed (status {status}): {message}")
            }
            VaError::Va { status, message } => {
                write!(f, "VA-API call failed (status {status}): {message}")
            }
        }
    }
}

impl std::error::Error for VaError {}

/// Owned VA-API DRM display handle.
///
/// `Drop` releases everything in reverse order:
/// 1. `vaTerminate(display)` — only if `vaInitialize` succeeded
/// 2. `libc::close(fd)`
///
/// A half-initialized display (open succeeded, init failed) is
/// dropped immediately by [`Display::open_drm`] — only the fd is
/// closed in that path, so callers never see one.
pub struct Display {
    fd: i32,
    dpy: VADisplay,
    /// `Some((major, minor))` only after a successful `vaInitialize`.
    /// `None` would only be visible during partial drop in
    /// `open_drm`; once we hand a `Display` back to the caller it's
    /// always populated.
    api_version: Option<(u32, u32)>,
}

// SAFETY note: VADisplay is documented as not thread-safe. We do not
// impl Send/Sync for `Display`.

impl Display {
    /// Open a DRM render node, request a `VADisplay` for it, and run
    /// `vaInitialize`.
    ///
    /// Steps:
    /// 1. `libc::open(path, O_RDWR | O_CLOEXEC)` — `OpenDrm` on failure.
    /// 2. `vaGetDisplayDRM(fd)` — `GetDisplayNull` on null result.
    /// 3. `vaInitialize` — `Init { status, message }` on failure,
    ///    with `message` from `vaErrorStr`.
    ///
    /// Cleanup on failure: the fd is always closed; `vaTerminate` is
    /// **not** called in the init-failure path because libva docs
    /// only require it after a successful init.
    pub fn open_drm(path: &Path) -> Result<Self, VaError> {
        let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;

        let cpath = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| VaError::OpenDrm(io::Error::new(io::ErrorKind::InvalidInput, e)))?;

        // SAFETY: `cpath` is a valid NUL-terminated C string.
        // `O_RDWR | O_CLOEXEC` is the standard render-node access
        // pattern. No mode arg needed because we are not creating
        // the file.
        let fd = unsafe {
            libc::open(
                cpath.as_ptr() as *const c_char,
                libc::O_RDWR | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(VaError::OpenDrm(io::Error::last_os_error()));
        }

        // SAFETY: `fd` is a valid open file descriptor; libva-drm's
        // contract is to dup it internally so we still own and must
        // close ours.
        let dpy = unsafe { (vt.va_get_display_drm)(fd) };
        if dpy.is_null() {
            // SAFETY: `fd` is a value we just received from `open`; close it.
            unsafe { libc::close(fd) };
            return Err(VaError::GetDisplayNull);
        }

        let mut major: i32 = 0;
        let mut minor: i32 = 0;
        // SAFETY: `dpy` non-null, `&mut` outlives the call. libva
        // writes the negotiated API version on success.
        let status = unsafe { (vt.va_initialize)(dpy, &mut major, &mut minor) };
        if status != VA_STATUS_SUCCESS {
            let message = error_str(vt, status);
            // Don't `vaTerminate` — init failed. Just close the fd.
            // SAFETY: same as above.
            unsafe { libc::close(fd) };
            return Err(VaError::Init { status, message });
        }

        Ok(Self {
            fd,
            dpy,
            api_version: Some((major as u32, minor as u32)),
        })
    }

    /// Negotiated API version reported by `vaInitialize`.
    pub fn api_version(&self) -> (u32, u32) {
        self.api_version.unwrap_or((0, 0))
    }

    /// Raw `VADisplay` pointer for FFI use. The display remains
    /// valid for the lifetime of `&self`; do not store the pointer
    /// past it.
    pub fn raw(&self) -> VADisplay {
        self.dpy
    }

    /// Underlying DRM render-node file descriptor. Treated as
    /// immutable — closing or duping it would invalidate the libva
    /// connection.
    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// Driver vendor string from `vaQueryVendorString`. Only meaningful
    /// after a successful init (this method requires `&self` from a
    /// constructed `Display`, which by definition is post-init).
    pub fn vendor_string(&self) -> Result<String, VaError> {
        let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;
        // SAFETY: `dpy` is valid (we hold it). `vaQueryVendorString`
        // returns a pointer to a static string owned by the driver;
        // valid until the display is terminated.
        let p = unsafe { (vt.va_query_vendor_string)(self.dpy) };
        if p.is_null() {
            return Err(VaError::Va {
                status: VA_STATUS_SUCCESS,
                message: "vaQueryVendorString returned null".into(),
            });
        }
        let s = unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned();
        Ok(s)
    }

    /// Profile list advertised by the driver. Sized via
    /// `vaMaxNumProfiles` and filled by `vaQueryConfigProfiles`.
    pub fn profiles(&self) -> Result<Vec<VaProfile>, VaError> {
        let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;
        // SAFETY: `dpy` is valid.
        let max = unsafe { (vt.va_max_num_profiles)(self.dpy) };
        if max <= 0 {
            return Ok(Vec::new());
        }
        let mut buf: Vec<i32> = vec![profile::VAProfileNone; max as usize];
        let mut num: i32 = 0;
        // SAFETY: buffer is large enough (max entries). `&mut num`
        // is written by libva to the actual count returned.
        let status = unsafe { (vt.va_query_config_profiles)(self.dpy, buf.as_mut_ptr(), &mut num) };
        if status != VA_STATUS_SUCCESS {
            return Err(VaError::Va {
                status,
                message: error_str(vt, status),
            });
        }
        if num < 0 {
            return Ok(Vec::new());
        }
        Ok(buf.into_iter().take(num as usize).map(VaProfile).collect())
    }

    /// List the entrypoints the driver advertises for a given profile.
    ///
    /// Wraps `vaQueryConfigEntrypoints`. Sized via `vaMaxNumEntrypoints`
    /// (capped at 32 — VA-API entrypoint enum has 12 values today, so
    /// the cap is generous and avoids a second dispatch round-trip).
    ///
    /// Returns an empty `Vec` if the profile is not supported by the
    /// driver (libva returns `VA_STATUS_ERROR_UNSUPPORTED_PROFILE` in
    /// that case — we map it to "no entrypoints" so a capability
    /// audit doesn't have to special-case `Err`).
    pub fn entrypoints(&self, profile: VaProfile) -> Result<Vec<i32>, VaError> {
        let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;
        // 32 is well above the largest entrypoint enum value (12) — see
        // `_VAEntrypointMax` in va.h. Stack-allocate to avoid a heap hit.
        let mut buf: [i32; 32] = [0; 32];
        let mut num: i32 = 0;
        // SAFETY: buf is valid for 32 writes; `&mut num` outlives call.
        let status = unsafe {
            (vt.va_query_config_entrypoints)(self.dpy, profile.raw(), buf.as_mut_ptr(), &mut num)
        };
        if status == sys::VA_STATUS_ERROR_UNSUPPORTED_PROFILE {
            return Ok(Vec::new());
        }
        if status != VA_STATUS_SUCCESS {
            return Err(VaError::Va {
                status,
                message: error_str(vt, status),
            });
        }
        if num <= 0 {
            return Ok(Vec::new());
        }
        Ok(buf[..num as usize].to_vec())
    }

    /// True if the driver advertises `entrypoint` for `profile`.
    ///
    /// Convenience wrapper around `entrypoints` for the common
    /// "is this codec/operation pair available?" capability check.
    /// Returns `false` for any error — the alternative would force
    /// every caller to handle `Result<bool>` for a query that's
    /// fundamentally a yes/no.
    pub fn is_supported(&self, profile: VaProfile, entrypoint: i32) -> bool {
        match self.entrypoints(profile) {
            Ok(list) => list.contains(&entrypoint),
            Err(_) => false,
        }
    }

    /// Subset of `profiles()` filtered to those that advertise
    /// `entrypoint`. Useful for "which codecs can I decode?" or
    /// "which codecs can I encode?" capability dumps.
    pub fn profiles_with_entrypoint(&self, entrypoint: i32) -> Result<Vec<VaProfile>, VaError> {
        let all = self.profiles()?;
        let mut out = Vec::with_capacity(all.len());
        for p in all {
            if self.is_supported(p, entrypoint) {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Build the full `(profile, [entrypoints])` matrix advertised by
    /// the driver in a single sweep.
    ///
    /// Intended for callers that need to consult several
    /// `(profile, entrypoint)` pairs in one pass — `engine_info()` and
    /// the cross-codec capability probes are the in-tree consumers.
    /// The naive shape `profile × entrypoint × is_supported` issues
    /// one [`vaQueryConfigEntrypoints`][va] call per `(profile,
    /// entrypoint)` pair; with N codec families and K entrypoints each
    /// across a driver advertising P profiles, that's O(P · K · families)
    /// FFI calls.  Calling `entrypoint_matrix()` once and asking the
    /// returned [`EntrypointMatrix`] is O(P) FFI calls plus O(1)
    /// lookups per query.
    ///
    /// The returned matrix is a snapshot — the driver's profile /
    /// entrypoint advertisements are stable across a process lifetime
    /// in every libva implementation in the wild, so callers can reuse
    /// the result across multiple capability queries without
    /// re-walking.
    ///
    /// [va]: https://intel.github.io/libva/group__api__core.html
    pub fn entrypoint_matrix(&self) -> Result<EntrypointMatrix, VaError> {
        let profiles = self.profiles()?;
        let mut rows = Vec::with_capacity(profiles.len());
        for p in profiles {
            let eps = self.entrypoints(p)?;
            rows.push((p, eps));
        }
        Ok(EntrypointMatrix { rows })
    }
}

/// Snapshot of the `(profile, entrypoints)` matrix advertised by a
/// driver.
///
/// Construct via [`Display::entrypoint_matrix`]. Use [`Self::is_supported`]
/// for yes/no checks, [`Self::profiles_with_entrypoint`] to filter, and
/// [`Self::profiles`] for the full advertised profile list. All lookups
/// are O(rows) over a small fixed list (real drivers expose between
/// ~10 and ~30 profiles); no further FFI traffic.
///
/// The matrix is constructed once per [`Display::entrypoint_matrix`]
/// call. Repeatedly calling [`Display::is_supported`] for the same
/// driver issues one `vaQueryConfigEntrypoints` per call — building
/// the matrix amortises those over a single sweep.
#[derive(Debug, Clone)]
pub struct EntrypointMatrix {
    rows: Vec<(VaProfile, Vec<i32>)>,
}

impl EntrypointMatrix {
    /// The advertised profile list (matches what [`Display::profiles`]
    /// would return).
    pub fn profiles(&self) -> impl Iterator<Item = VaProfile> + '_ {
        self.rows.iter().map(|(p, _)| *p)
    }

    /// Total number of profiles in the matrix.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True iff the matrix is empty (no advertised profiles — sandbox
    /// drivers, partial init, etc.).
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Entrypoints advertised for `profile`. Empty slice if `profile`
    /// is not in the matrix (i.e. the driver doesn't advertise it).
    pub fn entrypoints_for(&self, profile: VaProfile) -> &[i32] {
        self.rows
            .iter()
            .find(|(p, _)| *p == profile)
            .map(|(_, eps)| eps.as_slice())
            .unwrap_or(&[])
    }

    /// True iff the driver advertises `entrypoint` for `profile`.
    ///
    /// Equivalent to [`Display::is_supported`] but does no FFI.
    pub fn is_supported(&self, profile: VaProfile, entrypoint: i32) -> bool {
        self.entrypoints_for(profile).contains(&entrypoint)
    }

    /// Subset of [`Self::profiles`] filtered to those that advertise
    /// `entrypoint`. Equivalent to [`Display::profiles_with_entrypoint`]
    /// but does no FFI.
    pub fn profiles_with_entrypoint(&self, entrypoint: i32) -> Vec<VaProfile> {
        self.rows
            .iter()
            .filter(|(_, eps)| eps.contains(&entrypoint))
            .map(|(p, _)| *p)
            .collect()
    }

    /// True iff any of the supplied profiles advertise `entrypoint`.
    ///
    /// Convenience for "does this codec family support
    /// decode/encode?" — the matrix is consulted once per family
    /// profile and the answer is OR'd across them.
    pub fn any_supports(&self, profiles: &[i32], entrypoint: i32) -> bool {
        profiles
            .iter()
            .any(|raw| self.is_supported(VaProfile(*raw), entrypoint))
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // Best-effort teardown — ignore VAStatus / errno because
        // there's nothing actionable we can do at drop time, and
        // panicking here would mask a more useful upstream panic.
        if let Ok(vt) = sys::vtable() {
            if self.api_version.is_some() && !self.dpy.is_null() {
                // SAFETY: `dpy` came from a successful init pair.
                unsafe { (vt.va_terminate)(self.dpy) };
            }
        }
        if self.fd >= 0 {
            // SAFETY: fd was opened by us via `libc::open` and not
            // closed elsewhere.
            unsafe { libc::close(self.fd) };
        }
    }
}
