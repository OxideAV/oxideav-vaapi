//! Every VA buffer the H.264 / HEVC / VP9 / AV1 front ends submit, byte
//! for byte, against golden dumps in `tests/fixtures/vabuf/`.
//!
//! The front ends run against a recording stand-in for libva installed
//! through [`oxideav_vaapi::host::HostLoader`]: it advertises every
//! decode profile, hands out surface and buffer ids, and keeps a copy of
//! each buffer passed to `vaRenderPicture`. No GPU is involved, so this
//! runs on any host. Set `OXIDEAV_VAAPI_WRITE_GOLDEN=1` to rewrite the
//! dumps.
#![cfg(all(
    any(target_os = "linux", not(feature = "system-loader")),
    feature = "hevc",
    feature = "h264",
    feature = "vp9",
    feature = "av1"
))]

use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::fmt::Write as _;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, Once};

use oxideav_vaapi::display::Display;
use oxideav_vaapi::host::HostLoader;
use oxideav_vaapi::sys::{
    self, VA_ATTRIB_NOT_SUPPORTED, VA_RT_FORMAT_YUV420, VA_RT_FORMAT_YUV420_10,
    VA_STATUS_ERROR_UNIMPLEMENTED, VA_STATUS_SUCCESS, VABufferID, VAConfigAttrib, VAConfigID,
    VAContextID, VADisplay, VAImage, VAImageFormat, VAImageID, VAStatus, VASurfaceID, attrib,
    entrypoint, profile,
};

// ───────────────────────────── padding ─────────────────────────────

/// Offset and length of a padding hole.
type Hole = (usize, usize);

/// The padding holes of the `repr(C)` structure a
/// buffer of `kind` with `size`-byte elements holds. Padding is never
/// written by a struct literal, so its bytes are whatever the stack held
/// and are left out of the comparison (shown as `..`). Derived from the
/// field lists in `src/sys.rs` / `src/sys/*.rs` with the C layout rules.
fn holes(kind: u32, size: usize) -> &'static [Hole] {
    use oxideav_vaapi::sys::av1::{VADecPictureParameterBufferAV1, VASliceParameterBufferAV1};
    use oxideav_vaapi::sys::buffer_type::{
        VAPictureParameterBufferType, VASliceParameterBufferType,
    };
    use oxideav_vaapi::sys::hevc::VASliceParameterBufferHEVC;
    use oxideav_vaapi::sys::vp9::{VADecPictureParameterBufferVP9, VASliceParameterBufferVP9};
    use oxideav_vaapi::sys::{VAPictureParameterBufferH264, VASliceParameterBufferH264};
    use std::mem::size_of;
    let table: [(u32, usize, &'static [Hole]); 7] = [
        (
            VAPictureParameterBufferType,
            size_of::<VAPictureParameterBufferH264>(),
            &[(619, 1), (638, 2)],
        ),
        (
            VASliceParameterBufferType,
            size_of::<VASliceParameterBufferH264>(),
            &[
                (25, 3),
                (2335, 1),
                (2465, 1),
                (2723, 1),
                (2853, 1),
                (3110, 2),
            ],
        ),
        (
            VASliceParameterBufferType,
            size_of::<VASliceParameterBufferHEVC>(),
            &[(50, 2), (247, 1), (254, 2)],
        ),
        (
            VAPictureParameterBufferType,
            size_of::<VADecPictureParameterBufferVP9>(),
            &[(45, 1)],
        ),
        (
            VASliceParameterBufferType,
            size_of::<VASliceParameterBufferVP9>(),
            &[
                (30, 2),
                (66, 2),
                (102, 2),
                (138, 2),
                (174, 2),
                (210, 2),
                (246, 2),
                (282, 2),
            ],
        ),
        (
            VAPictureParameterBufferType,
            size_of::<VADecPictureParameterBufferAV1>(),
            &[
                (17, 7),
                (81, 3),
                (393, 1),
                (674, 2),
                (703, 1),
                (706, 2),
                (769, 3),
                (825, 3),
                (881, 3),
                (937, 3),
                (993, 3),
                (1049, 3),
                (1105, 3),
                (1156, 4),
            ],
        ),
        (
            VASliceParameterBufferType,
            size_of::<VASliceParameterBufferAV1>(),
            &[(21, 1)],
        ),
    ];
    // VAPictureParameterBufferHEVC and the IQ matrices have no padding.
    table
        .iter()
        .find(|&&(k, s, _)| k == kind && s == size)
        .map_or(&[], |&(_, _, h)| h)
}

// ───────────────────────────── recording libva ─────────────────────────────

#[derive(Default)]
struct Mock {
    next_id: u32,
    buffers: HashMap<VABufferID, (u32, u32, u32, Vec<u8>)>,
    target: Option<VASurfaceID>,
    log: String,
    pictures: usize,
}

static MOCK: Mutex<Option<Mock>> = Mutex::new(None);
/// Serialises the tests: the stand-in is process-wide.
static SERIAL: Mutex<()> = Mutex::new(());

fn with<R>(f: impl FnOnce(&mut Mock) -> R) -> R {
    let mut g = MOCK.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Mock::default))
}

fn fresh_id() -> u32 {
    with(|m| {
        m.next_id += 1;
        m.next_id
    })
}

const PROFILES: [i32; 8] = [
    profile::VAProfileH264ConstrainedBaseline,
    profile::VAProfileH264Main,
    profile::VAProfileH264High,
    profile::VAProfileHEVCMain,
    profile::VAProfileHEVCMain10,
    profile::VAProfileVP9Profile0,
    profile::VAProfileVP9Profile2,
    profile::VAProfileAV1Profile0,
];

unsafe extern "C" fn va_initialize(_: VADisplay, major: *mut i32, minor: *mut i32) -> VAStatus {
    unsafe {
        *major = 1;
        *minor = 20;
    }
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_status(_: VADisplay) -> VAStatus {
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_error_str(_: VAStatus) -> *const c_char {
    c"mock VA status".as_ptr()
}
unsafe extern "C" fn va_vendor(_: VADisplay) -> *const c_char {
    c"oxideav-vaapi recording stand-in".as_ptr()
}
unsafe extern "C" fn va_max_num_profiles(_: VADisplay) -> i32 {
    PROFILES.len() as i32
}
unsafe extern "C" fn va_query_config_profiles(
    _: VADisplay,
    list: *mut i32,
    n: *mut i32,
) -> VAStatus {
    unsafe {
        std::ptr::copy_nonoverlapping(PROFILES.as_ptr(), list, PROFILES.len());
        *n = PROFILES.len() as i32;
    }
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_query_config_entrypoints(
    _: VADisplay,
    p: i32,
    list: *mut i32,
    n: *mut i32,
) -> VAStatus {
    unsafe {
        if PROFILES.contains(&p) {
            *list = entrypoint::VAEntrypointVLD;
            *n = 1;
        } else {
            *n = 0;
        }
    }
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_get_config_attributes(
    _: VADisplay,
    _: i32,
    _: i32,
    list: *mut VAConfigAttrib,
    n: i32,
) -> VAStatus {
    for i in 0..n as usize {
        let a = unsafe { &mut *list.add(i) };
        a.value = match a.ty {
            attrib::VAConfigAttribRTFormat => VA_RT_FORMAT_YUV420 | VA_RT_FORMAT_YUV420_10,
            attrib::VAConfigAttribMaxPictureWidth | attrib::VAConfigAttribMaxPictureHeight => 8192,
            _ => VA_ATTRIB_NOT_SUPPORTED,
        };
    }
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_create_config(
    _: VADisplay,
    _: i32,
    _: i32,
    _: *mut c_void,
    _: i32,
    id: *mut VAConfigID,
) -> VAStatus {
    unsafe { *id = fresh_id() };
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_create_context(
    _: VADisplay,
    _: VAConfigID,
    _: i32,
    _: i32,
    _: i32,
    _: *mut VASurfaceID,
    _: i32,
    id: *mut VAContextID,
) -> VAStatus {
    unsafe { *id = fresh_id() };
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_create_surfaces(
    _: VADisplay,
    format: u32,
    width: u32,
    height: u32,
    surfaces: *mut VASurfaceID,
    n: u32,
    _: *mut c_void,
    _: u32,
) -> VAStatus {
    let mut ids = Vec::new();
    for i in 0..n as usize {
        let id = fresh_id();
        ids.push(id);
        unsafe { *surfaces.add(i) = id };
    }
    with(|m| {
        let _ = writeln!(
            m.log,
            "surfaces format={format:#x} {width}x{height} ids={ids:?}"
        );
    });
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_destroy_surfaces(_: VADisplay, _: *mut VASurfaceID, _: i32) -> VAStatus {
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_destroy_id(_: VADisplay, _: u32) -> VAStatus {
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_destroy_buffer(_: VADisplay, id: VABufferID) -> VAStatus {
    with(|m| m.buffers.remove(&id));
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_create_buffer(
    _: VADisplay,
    _: VAContextID,
    kind: u32,
    size: u32,
    count: u32,
    data: *mut c_void,
    id: *mut VABufferID,
) -> VAStatus {
    let bytes = if data.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data as *const u8, (size * count) as usize) }.to_vec()
    };
    let new = fresh_id();
    with(|m| m.buffers.insert(new, (kind, size, count, bytes)));
    unsafe { *id = new };
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_begin_picture(
    _: VADisplay,
    _: VAContextID,
    target: VASurfaceID,
) -> VAStatus {
    with(|m| m.target = Some(target));
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_render_picture(
    _: VADisplay,
    _: VAContextID,
    ids: *mut VABufferID,
    n: i32,
) -> VAStatus {
    let ids = unsafe { std::slice::from_raw_parts(ids, n as usize) }.to_vec();
    with(|m| {
        let _ = writeln!(
            m.log,
            "picture {} target={}",
            m.pictures,
            m.target.unwrap_or(u32::MAX)
        );
        m.pictures += 1;
        for id in ids {
            let (kind, size, count, bytes) = &m.buffers[&id];
            let holes = holes(*kind, *size as usize);
            let mut hex = String::with_capacity(bytes.len() * 2);
            for (i, b) in bytes.iter().enumerate() {
                let at = i % *size as usize;
                if holes.iter().any(|&(o, n)| (o..o + n).contains(&at)) {
                    hex.push_str("..");
                } else {
                    let _ = write!(hex, "{b:02x}");
                }
            }
            let _ = writeln!(
                m.log,
                "  buffer type={kind} size={size} count={count} {hex}"
            );
        }
    });
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_end_picture(_: VADisplay, _: VAContextID) -> VAStatus {
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_map_buffer(_: VADisplay, _: VABufferID, _: *mut *mut c_void) -> VAStatus {
    VA_STATUS_ERROR_UNIMPLEMENTED
}
unsafe extern "C" fn va_derive_image(_: VADisplay, _: VASurfaceID, _: *mut VAImage) -> VAStatus {
    VA_STATUS_ERROR_UNIMPLEMENTED
}
unsafe extern "C" fn va_destroy_image(_: VADisplay, _: VAImageID) -> VAStatus {
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_get_image(
    _: VADisplay,
    _: VASurfaceID,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
    _: VAImageID,
) -> VAStatus {
    VA_STATUS_ERROR_UNIMPLEMENTED
}
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn va_put_image(
    _: VADisplay,
    _: VASurfaceID,
    _: VAImageID,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) -> VAStatus {
    VA_STATUS_ERROR_UNIMPLEMENTED
}
unsafe extern "C" fn va_create_image(
    _: VADisplay,
    _: *mut VAImageFormat,
    _: i32,
    _: i32,
    _: *mut VAImage,
) -> VAStatus {
    VA_STATUS_ERROR_UNIMPLEMENTED
}
unsafe extern "C" fn va_query_image_formats(
    _: VADisplay,
    _: *mut VAImageFormat,
    n: *mut i32,
) -> VAStatus {
    unsafe { *n = 0 };
    VA_STATUS_SUCCESS
}
unsafe extern "C" fn va_max_num_image_formats(_: VADisplay) -> i32 {
    0
}
unsafe extern "C" fn va_get_display_drm(_: i32) -> VADisplay {
    std::ptr::NonNull::<u64>::dangling().as_ptr() as VADisplay
}
unsafe extern "C" fn c_open(_: *const c_char, _: i32) -> i32 {
    3
}
unsafe extern "C" fn c_close(_: i32) -> i32 {
    0
}

unsafe extern "C" fn resolve(_: *mut c_void, name: *const c_char) -> *mut c_void {
    let name = unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("");
    let f: *const () = match name {
        "vaInitialize" => va_initialize as *const (),
        "vaTerminate" => va_status as *const (),
        "vaErrorStr" => va_error_str as *const (),
        "vaQueryVendorString" => va_vendor as *const (),
        "vaMaxNumProfiles" => va_max_num_profiles as *const (),
        "vaQueryConfigProfiles" => va_query_config_profiles as *const (),
        "vaCreateConfig" => va_create_config as *const (),
        "vaCreateContext" => va_create_context as *const (),
        "vaCreateSurfaces" => va_create_surfaces as *const (),
        "vaDestroySurfaces" => va_destroy_surfaces as *const (),
        "vaBeginPicture" => va_begin_picture as *const (),
        "vaRenderPicture" => va_render_picture as *const (),
        "vaEndPicture" => va_end_picture as *const (),
        "vaCreateBuffer" => va_create_buffer as *const (),
        "vaQueryConfigEntrypoints" => va_query_config_entrypoints as *const (),
        "vaGetConfigAttributes" => va_get_config_attributes as *const (),
        "vaDestroyConfig" | "vaDestroyContext" | "vaSyncSurface" | "vaUnmapBuffer" => {
            va_destroy_id as *const ()
        }
        "vaDestroyBuffer" => va_destroy_buffer as *const (),
        "vaMapBuffer" => va_map_buffer as *const (),
        "vaDeriveImage" => va_derive_image as *const (),
        "vaDestroyImage" => va_destroy_image as *const (),
        "vaGetImage" => va_get_image as *const (),
        "vaPutImage" => va_put_image as *const (),
        "vaCreateImage" => va_create_image as *const (),
        "vaQueryImageFormats" => va_query_image_formats as *const (),
        "vaMaxNumImageFormats" => va_max_num_image_formats as *const (),
        "vaGetDisplayDRM" => va_get_display_drm as *const (),
        "open" => c_open as *const (),
        "close" => c_close as *const (),
        _ => std::ptr::null(),
    };
    f as *mut c_void
}

/// Install the stand-in (once), reset its state, and open a display on it.
fn begin() -> (MutexGuard<'static, ()>, Display) {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // SAFETY: every resolved function has the libva / libc prototype
        // of its name and lives for the process.
        let installed =
            unsafe { oxideav_vaapi::host::install(HostLoader::new(std::ptr::null_mut(), resolve)) };
        assert!(installed, "host loader installed before any VA call");
    });
    with(|m| *m = Mock::default());
    let display = Display::open_drm(Path::new("/dev/dri/renderD128")).expect("stand-in display");
    assert!(sys::vtable().is_ok());
    (guard, display)
}

fn take_log() -> String {
    with(|m| std::mem::take(&mut m.log))
}

// ───────────────────────────── streams ─────────────────────────────

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .expect("fixture")
}

fn annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut e = starts.get(k + 1).map_or(data.len(), |n| n - 3);
            while e > s && data[e - 1] == 0 {
                e -= 1;
            }
            &data[s..e]
        })
        .collect()
}

fn ivf(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = u16::from_le_bytes([data[6], data[7]]) as usize;
    while at + 12 <= data.len() {
        let n = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        out.push(&data[at + 12..at + 12 + n]);
        at += 12 + n;
    }
    out
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/vabuf")
        .join(format!("{name}.txt"))
}

fn check(name: &str, log: String) {
    let path = golden_path(name);
    if std::env::var_os("OXIDEAV_VAAPI_WRITE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &log).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).expect("golden dump");
    if want != log {
        if let Some(dir) = std::env::var_os("OXIDEAV_VAAPI_ACTUAL_DIR") {
            let _ = std::fs::write(Path::new(&dir).join(format!("{name}.txt")), &log);
        }
        let line = want
            .lines()
            .zip(log.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(want.lines().count().min(log.lines().count()));
        let show = |s: &str| s.lines().nth(line).unwrap_or("<end>").to_owned();
        panic!(
            "{name}: VA buffers differ from the golden dump at line {}\n  want: {}\n  got:  {}",
            line + 1,
            show(&want),
            show(&log)
        );
    }
}

fn run_h264(name: &str) {
    use oxideav_vaapi::h264::H264Decoder;
    let (_guard, display) = begin();
    let data = fixture(&format!("{name}.h264"));
    let mut dec = H264Decoder::new(display);
    let mut log = String::new();
    for unit in annexb(&data) {
        for p in dec.decode([unit]).unwrap() {
            let _ = writeln!(log, "{}out poc={} crop={:?}", take_log(), p.poc, p.crop);
        }
    }
    for p in dec.flush().unwrap() {
        let _ = writeln!(log, "{}out poc={} crop={:?}", take_log(), p.poc, p.crop);
    }
    log.push_str(&take_log());
    check(name, log);
}

fn run_hevc(name: &str) {
    use oxideav_vaapi::hevc::HevcDecoder;
    let (_guard, display) = begin();
    let data = fixture(&format!("{name}.hevc"));
    let mut dec = HevcDecoder::new(display);
    let mut log = String::new();
    for unit in annexb(&data) {
        for p in dec.decode([unit]).unwrap() {
            let _ = writeln!(
                log,
                "{}out poc={} output={} crop={:?}",
                take_log(),
                p.poc,
                p.output,
                p.crop
            );
        }
    }
    for p in dec.flush().unwrap() {
        let _ = writeln!(
            log,
            "{}out poc={} output={} crop={:?}",
            take_log(),
            p.poc,
            p.output,
            p.crop
        );
    }
    log.push_str(&take_log());
    check(name, log);
}

fn run_vp9(name: &str) {
    use oxideav_vaapi::vp9::Vp9Decoder;
    let (_guard, display) = begin();
    let data = fixture(&format!("{name}.ivf"));
    let mut dec = Vp9Decoder::new(display);
    let mut log = String::new();
    for sample in ivf(&data) {
        let p = dec.decode(sample).unwrap();
        log.push_str(&take_log());
        if let Some(p) = p {
            let _ = writeln!(log, "out {}x{}", p.width, p.height);
        }
    }
    log.push_str(&take_log());
    check(name, log);
}

fn run_av1(name: &str) {
    use oxideav_vaapi::av1::Av1Decoder;
    let (_guard, display) = begin();
    let data = fixture(&format!("{name}.ivf"));
    let mut dec = Av1Decoder::new(display);
    let mut log = String::new();
    for sample in ivf(&data) {
        let p = dec.decode(sample).unwrap();
        log.push_str(&take_log());
        if let Some(p) = p {
            let _ = writeln!(log, "out {}x{}", p.width, p.height);
        }
    }
    log.push_str(&take_log());
    check(name, log);
}

#[test]
fn h264() {
    run_h264("h264");
}

#[test]
fn hevc() {
    run_hevc("hevc");
}

#[test]
fn vp9() {
    run_vp9("vp9");
}

#[test]
fn av1() {
    run_av1("av1");
}

// Wider coverage than the GPU decode fixtures: explicit weighted
// prediction, scaling matrices, several slices, MBAFF, CAVLC and temporal
// direct, Constrained Baseline, HEVC Main 10 with scaling lists and open
// GOPs, VP9 segmentation / hidden alt-ref frames / tiles / profile 2 /
// error resilience / lossless, AV1 10-bit with film grain, a hierarchical
// GOP and superres.

#[test]
fn h264_wp_cqm() {
    run_h264("h264_wp_cqm");
}

#[test]
fn h264_cavlc() {
    run_h264("h264_cavlc");
}

#[test]
fn h264_cbp() {
    run_h264("h264_cbp");
}

#[test]
fn h264_mbaff() {
    run_h264("h264_mbaff");
}

#[test]
fn hevc_10bit() {
    run_hevc("hevc_10bit");
}

#[test]
fn hevc_cra() {
    run_hevc("hevc_cra");
}

/// Custom scaling lists (all sizes, DC values), wavefront entry points,
/// transquant bypass, weighted prediction.
#[test]
fn hevc_sl() {
    run_hevc("hevc_sl");
}

#[test]
fn vp9_seg() {
    run_vp9("vp9_seg");
}

#[test]
fn vp9_10bit() {
    run_vp9("vp9_10bit");
}

/// Lossless frames, two tile rows.
#[test]
fn vp9_lossless() {
    run_vp9("vp9_lossless");
}

#[test]
fn av1_10bit_fg() {
    run_av1("av1_10bit_fg");
}

#[test]
fn av1_hier() {
    run_av1("av1_hier");
}

/// Superres, compound references, skip mode, switchable motion modes.
#[test]
fn av1_tiles_sr() {
    run_av1("av1_tiles_sr");
}
