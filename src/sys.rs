//! Runtime-loaded VA-API library handles.
//!
//! Loaded once via `OnceLock` on first use and cached for the process
//! lifetime. If the dlopen fails the cache stores the error so
//! subsequent calls don't repeatedly hammer the dynamic linker.
//!
//! Libraries needed for a usable VA-API bridge:
//!
//! | Library              | Purpose                                            |
//! |----------------------|----------------------------------------------------|
//! | libva.so.2           | core dispatch (`vaInitialize`, `vaCreateConfig`…)  |
//! | libva-drm.so.2       | DRM render-node display backend (`vaGetDisplayDRM`) |
//!
//! Other backends (`libva-x11`, `libva-wayland`, `libva-glx`) exist
//! and may be added later — DRM is the headless / server-friendly
//! default and is what oxideav uses for transcoding pipelines.

use libloading::Library;
use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::OnceLock;

// ─────────────────────────── opaque VA types ──────────────────────────────────

/// VA dispatch handle. Returned by `vaGetDisplayDRM` (or other backend
/// constructors). Treated opaquely; we only pass the pointer around.
pub type VADisplay = *mut c_void;

/// VA config / context / surface / buffer IDs. All are 32-bit handles
/// in libva's ABI.
pub type VAConfigID = u32;
pub type VAContextID = u32;
pub type VASurfaceID = u32;
pub type VABufferID = u32;

/// VAStatus — return code for almost every libva entry point.
///
/// Spec defines this as a bare `int`; on every supported ABI that's a
/// 32-bit signed value. Constants in `va.h` are written as unsigned
/// hex but fit in `i32` (well within `0x..0026`). The sole exception
/// is `VA_STATUS_ERROR_UNKNOWN = 0xFFFFFFFF`, which sign-extends to
/// `-1` here.
pub type VAStatus = i32;

/// Success status: `VA_STATUS_SUCCESS == 0`.
pub const VA_STATUS_SUCCESS: VAStatus = 0;
pub const VA_STATUS_ERROR_OPERATION_FAILED: VAStatus = 0x0000_0001;
pub const VA_STATUS_ERROR_ALLOCATION_FAILED: VAStatus = 0x0000_0002;
pub const VA_STATUS_ERROR_INVALID_DISPLAY: VAStatus = 0x0000_0003;
pub const VA_STATUS_ERROR_INVALID_CONFIG: VAStatus = 0x0000_0004;
pub const VA_STATUS_ERROR_INVALID_CONTEXT: VAStatus = 0x0000_0005;
pub const VA_STATUS_ERROR_UNSUPPORTED_PROFILE: VAStatus = 0x0000_000c;
pub const VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT: VAStatus = 0x0000_000d;
pub const VA_STATUS_ERROR_INVALID_PARAMETER: VAStatus = 0x0000_0012;
pub const VA_STATUS_ERROR_UNIMPLEMENTED: VAStatus = 0x0000_0014;
/// `0xFFFFFFFF` sign-extends to `-1` on the 32-bit signed `VAStatus`.
pub const VA_STATUS_ERROR_UNKNOWN: VAStatus = -1;

// ─────────────────────────── VAProfile / VAEntrypoint ─────────────────────────

/// Subset of `VAProfile` values we care about for codec selection.
///
/// Full enum is large and most variants aren't relevant for the codecs
/// oxideav implements. Verbatim values from `/usr/include/va/va.h`.
#[allow(non_upper_case_globals)]
pub mod profile {
    pub const VAProfileNone: i32 = -1;
    pub const VAProfileH264Baseline: i32 = 5;
    pub const VAProfileH264Main: i32 = 6;
    pub const VAProfileH264High: i32 = 7;
    pub const VAProfileHEVCMain: i32 = 17;
    pub const VAProfileHEVCMain10: i32 = 18;
    pub const VAProfileAV1Profile0: i32 = 32;
    pub const VAProfileAV1Profile1: i32 = 33;
}

/// Subset of `VAEntrypoint` values we care about. Verbatim from
/// `/usr/include/va/va.h`.
#[allow(non_upper_case_globals)]
pub mod entrypoint {
    pub const VAEntrypointVLD: i32 = 1; // decode
    pub const VAEntrypointEncSlice: i32 = 6; // encode (slice level)
    pub const VAEntrypointEncSliceLP: i32 = 8; // encode low-power
    pub const VAEntrypointVideoProc: i32 = 10; // pre/post-processing
}

// ─────────────────────────── function pointer types ──────────────────────────

// Dispatch (libva.so.2)
pub type FnVaInitialize = unsafe extern "C" fn(
    dpy: VADisplay,
    major_version: *mut i32,
    minor_version: *mut i32,
) -> VAStatus;

pub type FnVaTerminate = unsafe extern "C" fn(dpy: VADisplay) -> VAStatus;

pub type FnVaErrorStr = unsafe extern "C" fn(error_status: VAStatus) -> *const c_char;

pub type FnVaQueryVendorString = unsafe extern "C" fn(dpy: VADisplay) -> *const c_char;

pub type FnVaMaxNumProfiles = unsafe extern "C" fn(dpy: VADisplay) -> i32;

pub type FnVaQueryConfigProfiles = unsafe extern "C" fn(
    dpy: VADisplay,
    profile_list: *mut i32,
    num_profiles: *mut i32,
) -> VAStatus;

pub type FnVaCreateConfig = unsafe extern "C" fn(
    dpy: VADisplay,
    profile: i32,
    entrypoint: i32,
    attrib_list: *mut c_void,
    num_attribs: i32,
    config_id: *mut VAConfigID,
) -> VAStatus;

pub type FnVaCreateContext = unsafe extern "C" fn(
    dpy: VADisplay,
    config_id: VAConfigID,
    picture_width: i32,
    picture_height: i32,
    flag: i32,
    render_targets: *mut VASurfaceID,
    num_render_targets: i32,
    context: *mut VAContextID,
) -> VAStatus;

pub type FnVaCreateSurfaces = unsafe extern "C" fn(
    dpy: VADisplay,
    format: u32,
    width: u32,
    height: u32,
    surfaces: *mut VASurfaceID,
    num_surfaces: u32,
    attrib_list: *mut c_void,
    num_attribs: u32,
) -> VAStatus;

pub type FnVaDestroySurfaces = unsafe extern "C" fn(
    dpy: VADisplay,
    surface_list: *mut VASurfaceID,
    num_surfaces: i32,
) -> VAStatus;

pub type FnVaBeginPicture = unsafe extern "C" fn(
    dpy: VADisplay,
    context: VAContextID,
    render_target: VASurfaceID,
) -> VAStatus;

pub type FnVaRenderPicture = unsafe extern "C" fn(
    dpy: VADisplay,
    context: VAContextID,
    buffers: *mut VABufferID,
    num_buffers: i32,
) -> VAStatus;

pub type FnVaEndPicture =
    unsafe extern "C" fn(dpy: VADisplay, context: VAContextID) -> VAStatus;

pub type FnVaCreateBuffer = unsafe extern "C" fn(
    dpy: VADisplay,
    context: VAContextID,
    buf_type: u32,
    size: u32,
    num_elements: u32,
    data: *mut c_void,
    buf_id: *mut VABufferID,
) -> VAStatus;

// DRM backend (libva-drm.so.2)
pub type FnVaGetDisplayDRM = unsafe extern "C" fn(fd: i32) -> VADisplay;

// ─────────────────────────── Vtable ───────────────────────────────────────────

/// Resolved function pointers for the bootstrap VA-API symbol set.
///
/// All fields are `unsafe extern "C" fn(...)` pointer types — callers
/// are responsible for the FFI invariants (correct argument types,
/// dispatch lifetime, `VAStatus` checking).
pub struct Vtable {
    // libva
    pub va_initialize: FnVaInitialize,
    pub va_terminate: FnVaTerminate,
    pub va_error_str: FnVaErrorStr,
    pub va_query_vendor_string: FnVaQueryVendorString,
    pub va_max_num_profiles: FnVaMaxNumProfiles,
    pub va_query_config_profiles: FnVaQueryConfigProfiles,
    pub va_create_config: FnVaCreateConfig,
    pub va_create_context: FnVaCreateContext,
    pub va_create_surfaces: FnVaCreateSurfaces,
    pub va_destroy_surfaces: FnVaDestroySurfaces,
    pub va_begin_picture: FnVaBeginPicture,
    pub va_render_picture: FnVaRenderPicture,
    pub va_end_picture: FnVaEndPicture,
    pub va_create_buffer: FnVaCreateBuffer,
    // libva-drm
    pub va_get_display_drm: FnVaGetDisplayDRM,
    // Keep libraries alive
    _va: Library,
    _va_drm: Library,
}

/// Smoke-test wrapper used by tests + by the pre-flight load check
/// in `register()`. Holds the raw `Library` handles so callers can
/// assert that dlopen succeeded without paying the full dlsym tour.
pub struct FrameworkSmoke {
    pub libva: Library,
    pub libva_drm: Library,
}

// ─────────────────────────── Caches ───────────────────────────────────────────

static VTABLE: OnceLock<Result<Vtable, String>> = OnceLock::new();
static FRAMEWORK: OnceLock<Result<FrameworkSmoke, String>> = OnceLock::new();

/// Get (or load) the fully-resolved vtable. Returns the cached `Err`
/// if a previous load attempt failed.
pub fn vtable() -> Result<&'static Vtable, &'static str> {
    VTABLE
        .get_or_init(load_vtable)
        .as_ref()
        .map_err(|s| s.as_str())
}

/// Cheap framework-load check used by `register()`. Resolves the two
/// libraries but does no dlsym work.
pub fn framework() -> Result<&'static FrameworkSmoke, &'static str> {
    FRAMEWORK
        .get_or_init(load_smoke)
        .as_ref()
        .map_err(|s| s.as_str())
}

fn load_smoke() -> Result<FrameworkSmoke, String> {
    Ok(FrameworkSmoke {
        libva: open("libva.so.2")?,
        libva_drm: open("libva-drm.so.2")?,
    })
}

fn load_vtable() -> Result<Vtable, String> {
    let libva = open("libva.so.2")?;
    let libva_drm = open("libva-drm.so.2")?;

    macro_rules! sym {
        ($lib:expr, $name:expr, $ty:ty) => {{
            let s: libloading::Symbol<$ty> = unsafe {
                $lib.get(concat!($name, "\0").as_bytes())
                    .map_err(|e| format!("dlsym {}: {}", $name, e))?
            };
            *s
        }};
    }

    Ok(Vtable {
        va_initialize: sym!(libva, "vaInitialize", FnVaInitialize),
        va_terminate: sym!(libva, "vaTerminate", FnVaTerminate),
        va_error_str: sym!(libva, "vaErrorStr", FnVaErrorStr),
        va_query_vendor_string: sym!(
            libva,
            "vaQueryVendorString",
            FnVaQueryVendorString
        ),
        va_max_num_profiles: sym!(libva, "vaMaxNumProfiles", FnVaMaxNumProfiles),
        va_query_config_profiles: sym!(
            libva,
            "vaQueryConfigProfiles",
            FnVaQueryConfigProfiles
        ),
        va_create_config: sym!(libva, "vaCreateConfig", FnVaCreateConfig),
        va_create_context: sym!(libva, "vaCreateContext", FnVaCreateContext),
        va_create_surfaces: sym!(libva, "vaCreateSurfaces", FnVaCreateSurfaces),
        va_destroy_surfaces: sym!(libva, "vaDestroySurfaces", FnVaDestroySurfaces),
        va_begin_picture: sym!(libva, "vaBeginPicture", FnVaBeginPicture),
        va_render_picture: sym!(libva, "vaRenderPicture", FnVaRenderPicture),
        va_end_picture: sym!(libva, "vaEndPicture", FnVaEndPicture),
        va_create_buffer: sym!(libva, "vaCreateBuffer", FnVaCreateBuffer),
        va_get_display_drm: sym!(libva_drm, "vaGetDisplayDRM", FnVaGetDisplayDRM),
        _va: libva,
        _va_drm: libva_drm,
    })
}

fn open(path: &str) -> Result<Library, String> {
    // SAFETY: dlopen on a soname with no init callbacks; equivalent to
    // a normal program startup load.
    unsafe { Library::new(path) }.map_err(|e| format!("dlopen {path}: {e}"))
}

/// Decode a `vaErrorStr` return into a Rust `String`. Returns a
/// fallback if the driver hands back a null pointer (it shouldn't for
/// known status codes, but treat it defensively).
pub fn error_str(vt: &Vtable, status: VAStatus) -> String {
    let p = unsafe { (vt.va_error_str)(status) };
    if p.is_null() {
        return format!("VAStatus 0x{:x}", status as u32);
    }
    // SAFETY: `vaErrorStr` returns a pointer to a static C string in
    // libva. The pointer is valid for the lifetime of the process and
    // the bytes are NUL-terminated.
    let cstr = unsafe { std::ffi::CStr::from_ptr(p) };
    cstr.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: both libraries on this machine load cleanly.
    #[test]
    fn frameworks_load() {
        let fw = framework().expect("framework load");
        // Confirm a stable VA entry point is present in libva so we
        // know the dynamic linker served the right SO.
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.libva
                .get(b"vaInitialize\0")
                .expect("vaInitialize symbol")
        };
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.libva_drm
                .get(b"vaGetDisplayDRM\0")
                .expect("vaGetDisplayDRM symbol")
        };
    }

    /// Verify the full vtable resolves all symbols.
    #[test]
    fn vtable_resolves() {
        vtable().expect("vtable load");
    }
}
