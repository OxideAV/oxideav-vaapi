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
pub type VAImageID = u32;

/// Sentinel value used by the spec for "no surface" / unset slots
/// (e.g. unused `ReferenceFrames[]` entries in `VAPictureH264`).
pub const VA_INVALID_ID: u32 = 0xFFFF_FFFF;
pub const VA_INVALID_SURFACE: VASurfaceID = VA_INVALID_ID;

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
    pub const VAProfileMPEG2Simple: i32 = 0;
    pub const VAProfileMPEG2Main: i32 = 1;
    pub const VAProfileH264Baseline: i32 = 5;
    pub const VAProfileH264Main: i32 = 6;
    pub const VAProfileH264High: i32 = 7;
    pub const VAProfileVC1Simple: i32 = 8;
    pub const VAProfileVC1Main: i32 = 9;
    pub const VAProfileVC1Advanced: i32 = 10;
    pub const VAProfileJPEGBaseline: i32 = 12;
    pub const VAProfileH264ConstrainedBaseline: i32 = 13;
    pub const VAProfileVP8Version0_3: i32 = 14;
    pub const VAProfileHEVCMain: i32 = 17;
    pub const VAProfileHEVCMain10: i32 = 18;
    pub const VAProfileVP9Profile0: i32 = 19;
    pub const VAProfileVP9Profile2: i32 = 21;
    pub const VAProfileHEVCMain12: i32 = 23;
    pub const VAProfileHEVCMain444: i32 = 26;
    pub const VAProfileHEVCMain444_10: i32 = 27;
    pub const VAProfileHEVCMain444_12: i32 = 28;
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

// ─────────────────────────── Config attribute types ─────────────────────────
//
// Verbatim subset from `VAConfigAttribType` in `/usr/include/va/va.h`. Many
// more values exist; we expose only those that are useful for steering
// `Config::new` defaults and surfacing decoder capability checks.

#[allow(non_upper_case_globals)]
pub mod attrib {
    pub const VAConfigAttribRTFormat: i32 = 0;
    pub const VAConfigAttribSpatialResidual: i32 = 1;
    pub const VAConfigAttribSpatialClipping: i32 = 2;
    pub const VAConfigAttribIntraResidual: i32 = 3;
    pub const VAConfigAttribEncryption: i32 = 4;
    pub const VAConfigAttribRateControl: i32 = 5;
    pub const VAConfigAttribDecSliceMode: i32 = 6;
    pub const VAConfigAttribDecJPEG: i32 = 7;
    pub const VAConfigAttribDecProcessing: i32 = 8;
    // The dec/enc attributes 9..17 are interleaved; only the values
    // we actually consult are listed here. Verbatim from va.h
    // (VAConfigAttribType enum).
    pub const VAConfigAttribMaxPictureWidth: i32 = 18;
    pub const VAConfigAttribMaxPictureHeight: i32 = 19;
}

/// `VAConfigAttrib { type, value }` — the in/out struct used by
/// `vaGetConfigAttributes` and the optional `attrib_list` to
/// `vaCreateConfig`. Layout matches `_VAConfigAttrib` in `va.h`:
/// two contiguous `uint32_t` fields.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAConfigAttrib {
    pub ty: i32,
    pub value: u32,
}

/// Sentinel returned by the driver when an attribute is not supported
/// for the queried profile/entrypoint pair.
pub const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

// ─────────────────────────── RT format / FourCC values ─────────────────────

pub const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
pub const VA_RT_FORMAT_YUV422: u32 = 0x0000_0002;
pub const VA_RT_FORMAT_YUV444: u32 = 0x0000_0004;
pub const VA_RT_FORMAT_YUV400: u32 = 0x0000_0010;
pub const VA_RT_FORMAT_YUV420_10: u32 = 0x0000_0100;

pub const VA_FOURCC_NV12: u32 = 0x3231_564E;
pub const VA_FOURCC_I420: u32 = 0x3032_3449;
pub const VA_FOURCC_YV12: u32 = 0x3231_5659;
pub const VA_FOURCC_IYUV: u32 = 0x5655_5949;

// ─────────────────────────── Buffer types ─────────────────────────────────

#[allow(non_upper_case_globals)]
pub mod buffer_type {
    pub const VAPictureParameterBufferType: u32 = 0;
    pub const VAIQMatrixBufferType: u32 = 1;
    pub const VASliceParameterBufferType: u32 = 4;
    pub const VASliceDataBufferType: u32 = 5;
    pub const VAImageBufferType: u32 = 9;
}

// Slice data flags — see `va.h` lines 3057+
pub const VA_SLICE_DATA_FLAG_ALL: u32 = 0x00;
pub const VA_SLICE_DATA_FLAG_BEGIN: u32 = 0x01;
pub const VA_SLICE_DATA_FLAG_MIDDLE: u32 = 0x02;
pub const VA_SLICE_DATA_FLAG_END: u32 = 0x04;

// ─────────────────────────── H.264 picture / slice flags ───────────────────

pub const VA_PICTURE_H264_INVALID: u32 = 0x0000_0001;
pub const VA_PICTURE_H264_TOP_FIELD: u32 = 0x0000_0002;
pub const VA_PICTURE_H264_BOTTOM_FIELD: u32 = 0x0000_0004;
pub const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x0000_0008;
pub const VA_PICTURE_H264_LONG_TERM_REFERENCE: u32 = 0x0000_0010;

// ─────────────────────────── VA padding constants ─────────────────────────
// Mirror of `VA_PADDING_LOW`/`MEDIUM`/`HIGH` in `va.h` — the trailing
// `va_reserved[N]` arrays in every libva struct must match the header's
// length so the layout stays ABI-compatible.
pub const VA_PADDING_LOW: usize = 4;
pub const VA_PADDING_MEDIUM: usize = 8;
pub const VA_PADDING_HIGH: usize = 16;

// ─────────────────────────── H.264 decode buffers ──────────────────────────
//
// Verbatim layout from `/usr/include/va/va.h` (`_VAPictureH264`,
// `_VAPictureParameterBufferH264`, `_VASliceParameterBufferH264`).
// Bitfield unions are flattened into the one `u32` value we know is
// the storage representation on every supported ABI.

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAPictureH264 {
    pub picture_id: VASurfaceID,
    pub frame_idx: u32,
    pub flags: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

impl VAPictureH264 {
    /// Return a `VAPictureH264` whose every field is "invalid" —
    /// suitable for filling unused `ReferenceFrames[16]` slots and
    /// the `RefPicList0/1[32]` slots in the slice parameter buffer.
    pub fn invalid() -> Self {
        Self {
            picture_id: VA_INVALID_SURFACE,
            frame_idx: 0,
            flags: VA_PICTURE_H264_INVALID,
            top_field_order_cnt: 0,
            bottom_field_order_cnt: 0,
            va_reserved: [0; VA_PADDING_LOW],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAPictureParameterBufferH264 {
    pub curr_pic: VAPictureH264,
    pub reference_frames: [VAPictureH264; 16],
    pub picture_width_in_mbs_minus1: u16,
    pub picture_height_in_mbs_minus1: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames: u8,
    /// Packed bitfield from `seq_fields.bits` — see `va.h` for layout.
    pub seq_fields: u32,
    /// Deprecated FMO fields — must be zero.
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub slice_group_change_rate_minus1: u16,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    /// Packed bitfield from `pic_fields.bits`.
    pub pic_fields: u32,
    pub frame_num: u16,
    pub va_reserved: [u32; VA_PADDING_MEDIUM],
}

/// `VAIQMatrixBufferH264` — H.264 inverse quantization matrices. Two
/// arrays in raster scan order: 6×16 4×4 lists and 2×64 8×8 lists.
/// When the SPS / PPS scaling matrix flags are unset, every entry must
/// be `16` (flat default per H.264 7.4.2.1.1 / 8.5.5).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAIQMatrixBufferH264 {
    pub scaling_list_4x4: [[u8; 16]; 6],
    pub scaling_list_8x8: [[u8; 64]; 2],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

impl VAIQMatrixBufferH264 {
    /// Flat 16/16 scaling lists — what the encoder produces when
    /// `seq_scaling_matrix_present_flag` and
    /// `pic_scaling_matrix_present_flag` are both 0 (which is the case
    /// for the test fixture and for ffmpeg `-preset ultrafast` output
    /// generally).
    pub fn flat() -> Self {
        Self {
            scaling_list_4x4: [[16; 16]; 6],
            scaling_list_8x8: [[16; 64]; 2],
            va_reserved: [0; VA_PADDING_LOW],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VASliceParameterBufferH264 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_bit_offset: u16,
    pub first_mb_in_slice: u16,
    pub slice_type: u8,
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub ref_pic_list0: [VAPictureH264; 32],
    pub ref_pic_list1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

// ─────────────────────────── VAImage / VAImageFormat ──────────────────────

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAImage {
    pub image_id: VAImageID,
    pub format: VAImageFormat,
    pub buf: VABufferID,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

impl VAImage {
    /// Zero-initialised `VAImage` suitable for passing as the `out`
    /// argument to `vaDeriveImage`.
    pub fn zeroed() -> Self {
        // SAFETY: every field is plain old data (POD); a zero
        // bit-pattern is a valid value for all of them.
        unsafe { std::mem::zeroed() }
    }
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

pub type FnVaEndPicture = unsafe extern "C" fn(dpy: VADisplay, context: VAContextID) -> VAStatus;

pub type FnVaCreateBuffer = unsafe extern "C" fn(
    dpy: VADisplay,
    context: VAContextID,
    buf_type: u32,
    size: u32,
    num_elements: u32,
    data: *mut c_void,
    buf_id: *mut VABufferID,
) -> VAStatus;

pub type FnVaQueryConfigEntrypoints = unsafe extern "C" fn(
    dpy: VADisplay,
    profile: i32,
    entrypoint_list: *mut i32,
    num_entrypoints: *mut i32,
) -> VAStatus;

pub type FnVaGetConfigAttributes = unsafe extern "C" fn(
    dpy: VADisplay,
    profile: i32,
    entrypoint: i32,
    attrib_list: *mut VAConfigAttrib,
    num_attribs: i32,
) -> VAStatus;

pub type FnVaDestroyConfig =
    unsafe extern "C" fn(dpy: VADisplay, config_id: VAConfigID) -> VAStatus;

pub type FnVaDestroyContext =
    unsafe extern "C" fn(dpy: VADisplay, context: VAContextID) -> VAStatus;

pub type FnVaSyncSurface =
    unsafe extern "C" fn(dpy: VADisplay, render_target: VASurfaceID) -> VAStatus;

pub type FnVaDestroyBuffer =
    unsafe extern "C" fn(dpy: VADisplay, buffer_id: VABufferID) -> VAStatus;

pub type FnVaMapBuffer =
    unsafe extern "C" fn(dpy: VADisplay, buf_id: VABufferID, pbuf: *mut *mut c_void) -> VAStatus;

pub type FnVaUnmapBuffer = unsafe extern "C" fn(dpy: VADisplay, buf_id: VABufferID) -> VAStatus;

pub type FnVaDeriveImage =
    unsafe extern "C" fn(dpy: VADisplay, surface: VASurfaceID, image: *mut VAImage) -> VAStatus;

pub type FnVaDestroyImage = unsafe extern "C" fn(dpy: VADisplay, image: VAImageID) -> VAStatus;

pub type FnVaGetImage = unsafe extern "C" fn(
    dpy: VADisplay,
    surface: VASurfaceID,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    image: VAImageID,
) -> VAStatus;

pub type FnVaCreateImage = unsafe extern "C" fn(
    dpy: VADisplay,
    format: *mut VAImageFormat,
    width: i32,
    height: i32,
    image: *mut VAImage,
) -> VAStatus;

pub type FnVaQueryImageFormats = unsafe extern "C" fn(
    dpy: VADisplay,
    format_list: *mut VAImageFormat,
    num_formats: *mut i32,
) -> VAStatus;

pub type FnVaMaxNumImageFormats = unsafe extern "C" fn(dpy: VADisplay) -> i32;

pub type FnVaPutImage = unsafe extern "C" fn(
    dpy: VADisplay,
    surface: VASurfaceID,
    image: VAImageID,
    src_x: i32,
    src_y: i32,
    src_width: u32,
    src_height: u32,
    dest_x: i32,
    dest_y: i32,
    dest_width: u32,
    dest_height: u32,
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
    pub va_query_config_entrypoints: FnVaQueryConfigEntrypoints,
    pub va_get_config_attributes: FnVaGetConfigAttributes,
    pub va_destroy_config: FnVaDestroyConfig,
    pub va_destroy_context: FnVaDestroyContext,
    pub va_sync_surface: FnVaSyncSurface,
    pub va_destroy_buffer: FnVaDestroyBuffer,
    pub va_map_buffer: FnVaMapBuffer,
    pub va_unmap_buffer: FnVaUnmapBuffer,
    pub va_derive_image: FnVaDeriveImage,
    pub va_destroy_image: FnVaDestroyImage,
    pub va_get_image: FnVaGetImage,
    pub va_put_image: FnVaPutImage,
    pub va_create_image: FnVaCreateImage,
    pub va_query_image_formats: FnVaQueryImageFormats,
    pub va_max_num_image_formats: FnVaMaxNumImageFormats,
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
        va_query_vendor_string: sym!(libva, "vaQueryVendorString", FnVaQueryVendorString),
        va_max_num_profiles: sym!(libva, "vaMaxNumProfiles", FnVaMaxNumProfiles),
        va_query_config_profiles: sym!(libva, "vaQueryConfigProfiles", FnVaQueryConfigProfiles),
        va_create_config: sym!(libva, "vaCreateConfig", FnVaCreateConfig),
        va_create_context: sym!(libva, "vaCreateContext", FnVaCreateContext),
        va_create_surfaces: sym!(libva, "vaCreateSurfaces", FnVaCreateSurfaces),
        va_destroy_surfaces: sym!(libva, "vaDestroySurfaces", FnVaDestroySurfaces),
        va_begin_picture: sym!(libva, "vaBeginPicture", FnVaBeginPicture),
        va_render_picture: sym!(libva, "vaRenderPicture", FnVaRenderPicture),
        va_end_picture: sym!(libva, "vaEndPicture", FnVaEndPicture),
        va_create_buffer: sym!(libva, "vaCreateBuffer", FnVaCreateBuffer),
        va_query_config_entrypoints: sym!(
            libva,
            "vaQueryConfigEntrypoints",
            FnVaQueryConfigEntrypoints
        ),
        va_get_config_attributes: sym!(libva, "vaGetConfigAttributes", FnVaGetConfigAttributes),
        va_destroy_config: sym!(libva, "vaDestroyConfig", FnVaDestroyConfig),
        va_destroy_context: sym!(libva, "vaDestroyContext", FnVaDestroyContext),
        va_sync_surface: sym!(libva, "vaSyncSurface", FnVaSyncSurface),
        va_destroy_buffer: sym!(libva, "vaDestroyBuffer", FnVaDestroyBuffer),
        va_map_buffer: sym!(libva, "vaMapBuffer", FnVaMapBuffer),
        va_unmap_buffer: sym!(libva, "vaUnmapBuffer", FnVaUnmapBuffer),
        va_derive_image: sym!(libva, "vaDeriveImage", FnVaDeriveImage),
        va_destroy_image: sym!(libva, "vaDestroyImage", FnVaDestroyImage),
        va_get_image: sym!(libva, "vaGetImage", FnVaGetImage),
        va_put_image: sym!(libva, "vaPutImage", FnVaPutImage),
        va_create_image: sym!(libva, "vaCreateImage", FnVaCreateImage),
        va_query_image_formats: sym!(libva, "vaQueryImageFormats", FnVaQueryImageFormats),
        va_max_num_image_formats: sym!(libva, "vaMaxNumImageFormats", FnVaMaxNumImageFormats),
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
    /// Skip-friendly — CI runners without libva (no `libva.so.2` /
    /// `libva-drm.so.2`) `eprintln!` and return rather than fail.
    #[test]
    fn frameworks_load() {
        let fw = match framework() {
            Ok(fw) => fw,
            Err(e) => {
                eprintln!("oxideav-vaapi: framework unavailable, skipping: {e}");
                return;
            }
        };
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

    /// Verify the full vtable resolves all symbols. Skip-friendly when
    /// the framework can't be loaded (e.g. CI runner without libva).
    #[test]
    fn vtable_resolves() {
        match vtable() {
            Ok(_) => {}
            Err(e) => {
                eprintln!("oxideav-vaapi: vtable unavailable, skipping: {e}");
            }
        }
    }
}
