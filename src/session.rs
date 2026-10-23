//! A VLD decode session: one config, one context, a fixed pool of
//! render-target surfaces, picture submission and surface read-back.
//!
//! Codec front ends ([`crate::hevc`], ...) own the bitstream state and
//! decide which surface holds which picture; this module only moves
//! parameter buffers to the driver and pixels back out.
//!
//! Read-back copies a surface's visible rectangle into caller planes in
//! the surface's own layout: NV12 (8-bit 4:2:0) or P010 (10-bit 4:2:0,
//! samples in the high bits of little-endian 16-bit words). It maps the
//! surface directly (`vaDeriveImage`) when the driver allows it and
//! otherwise copies through an image (`vaCreateImage` + `vaGetImage`).

use std::ffi::c_void;
use std::fmt;

use crate::display::{Display, VaError};
use crate::sys::{
    self, entrypoint, error_str, VABufferID, VAConfigID, VAContextID, VAImage, VAImageFormat,
    VASurfaceID, VA_INVALID_ID, VA_RT_FORMAT_YUV420, VA_RT_FORMAT_YUV420_10, VA_STATUS_SUCCESS,
};

/// `VA_FOURCC_P010`.
pub const VA_FOURCC_P010: u32 = 0x3031_3050;

/// Why a hardware decode could not be done.
#[derive(Debug)]
pub enum DecodeError {
    /// The stream needs something this driver or this bridge does not do
    /// (profile, bit depth, chroma format, size, coding tool). Nothing was
    /// submitted; another decoder can take the stream.
    Unsupported(String),
    /// The bitstream is malformed or refers to state it never set up.
    Bitstream(String),
    /// A VA call failed.
    Va(VaError),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Unsupported(s) => write!(f, "unsupported: {s}"),
            DecodeError::Bitstream(s) => write!(f, "bitstream: {s}"),
            DecodeError::Va(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<VaError> for DecodeError {
    fn from(e: VaError) -> Self {
        DecodeError::Va(e)
    }
}

pub(crate) fn va_error(vt: &sys::Vtable, what: &str, status: i32) -> DecodeError {
    DecodeError::Va(VaError::Va {
        status,
        message: format!("{what}: {}", error_str(vt, status)),
    })
}

fn vtable() -> Result<&'static sys::Vtable, DecodeError> {
    sys::vtable().map_err(|e| DecodeError::Va(VaError::Sys(e.to_string())))
}

/// Sample layout of a session's surfaces.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SurfaceFormat {
    /// 8-bit 4:2:0, Y plane then interleaved CbCr.
    Nv12,
    /// 10-bit 4:2:0 in 16-bit words, Y plane then interleaved CbCr.
    P010,
}

impl SurfaceFormat {
    pub fn rt_format(self) -> u32 {
        match self {
            SurfaceFormat::Nv12 => VA_RT_FORMAT_YUV420,
            SurfaceFormat::P010 => VA_RT_FORMAT_YUV420_10,
        }
    }
    pub fn fourcc(self) -> u32 {
        match self {
            SurfaceFormat::Nv12 => sys::VA_FOURCC_NV12,
            SurfaceFormat::P010 => VA_FOURCC_P010,
        }
    }
    /// Bytes per sample.
    pub fn sample_bytes(self) -> usize {
        match self {
            SurfaceFormat::Nv12 => 1,
            SurfaceFormat::P010 => 2,
        }
    }
}

/// One parameter or data buffer of a picture submission.
#[derive(Copy, Clone, Debug)]
pub struct Buffer<'a> {
    /// `VABufferType`.
    pub kind: u32,
    /// Size of one element in bytes.
    pub element_size: usize,
    /// The elements, `element_size * n` bytes.
    pub data: &'a [u8],
}

impl<'a> Buffer<'a> {
    /// A buffer holding one plain-data VA structure.
    pub fn of<T: Copy>(kind: u32, value: &'a T) -> Self {
        // SAFETY: T is a `repr(C)` VA structure of plain integers.
        let data = unsafe {
            std::slice::from_raw_parts(value as *const T as *const u8, std::mem::size_of::<T>())
        };
        Buffer {
            kind,
            element_size: data.len(),
            data,
        }
    }
    /// A buffer of raw bytes (slice data).
    pub fn bytes(kind: u32, data: &'a [u8]) -> Self {
        Buffer {
            kind,
            element_size: data.len(),
            data,
        }
    }
}

/// Destination planes for [`DecodeSession::read`]: luma, then the
/// interleaved chroma plane, with their row strides in bytes.
pub struct Planes<'a> {
    pub luma: &'a mut [u8],
    pub luma_stride: usize,
    pub chroma: &'a mut [u8],
    pub chroma_stride: usize,
}

/// See the module documentation.
pub struct DecodeSession {
    /// Always `Some` until `into_display` takes it.
    display: Option<Display>,
    config: VAConfigID,
    context: VAContextID,
    surfaces: Vec<VASurfaceID>,
    in_use: Vec<bool>,
    format: SurfaceFormat,
    width: u32,
    height: u32,
    /// `None` until the first read; then whether `vaDeriveImage` works.
    derive_works: Option<bool>,
}

impl DecodeSession {
    /// Create a VLD session for `profile` on `display` with `count`
    /// surfaces of `width` x `height` (the coded size). The display is
    /// returned with the error when creation fails.
    pub fn new(
        display: Display,
        profile: i32,
        format: SurfaceFormat,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Self, (Display, DecodeError)> {
        let vt = match vtable() {
            Ok(vt) => vt,
            Err(e) => return Err((display, e)),
        };
        let mut session = DecodeSession {
            display: Some(display),
            config: VA_INVALID_ID,
            context: VA_INVALID_ID,
            surfaces: Vec::new(),
            in_use: Vec::new(),
            format,
            width,
            height,
            derive_works: None,
        };
        match session.create(vt, profile, count) {
            Ok(()) => Ok(session),
            Err(e) => Err((session.into_display(), e)),
        }
    }

    fn create(&mut self, vt: &sys::Vtable, profile: i32, count: usize) -> Result<(), DecodeError> {
        let dpy = self.dpy();
        let mut attrib = sys::VAConfigAttrib {
            ty: sys::attrib::VAConfigAttribRTFormat,
            value: 0,
        };
        let status = crate::host::native(|| unsafe {
            (vt.va_get_config_attributes)(dpy, profile, entrypoint::VAEntrypointVLD, &mut attrib, 1)
        });
        if status != VA_STATUS_SUCCESS {
            return Err(DecodeError::Unsupported(format!(
                "profile {profile}: {}",
                error_str(vt, status)
            )));
        }
        let rt = self.format.rt_format();
        if attrib.value == sys::VA_ATTRIB_NOT_SUPPORTED || attrib.value & rt == 0 {
            return Err(DecodeError::Unsupported(format!(
                "profile {profile}: no render target format {rt:#x}"
            )));
        }
        attrib.value = rt;
        let mut config = VA_INVALID_ID;
        let status = crate::host::native(|| unsafe {
            (vt.va_create_config)(
                dpy,
                profile,
                entrypoint::VAEntrypointVLD,
                &mut attrib as *mut sys::VAConfigAttrib as *mut c_void,
                1,
                &mut config,
            )
        });
        if status != VA_STATUS_SUCCESS {
            return Err(DecodeError::Unsupported(format!(
                "vaCreateConfig: {}",
                error_str(vt, status)
            )));
        }
        self.config = config;
        let mut surfaces = vec![VA_INVALID_ID; count];
        let status = crate::host::native(|| unsafe {
            (vt.va_create_surfaces)(
                dpy,
                rt,
                self.width,
                self.height,
                surfaces.as_mut_ptr(),
                count as u32,
                std::ptr::null_mut(),
                0,
            )
        });
        if status != VA_STATUS_SUCCESS {
            return Err(va_error(vt, "vaCreateSurfaces", status));
        }
        self.surfaces = surfaces;
        self.in_use = vec![false; count];
        let mut context = VA_INVALID_ID;
        let status = crate::host::native(|| unsafe {
            (vt.va_create_context)(
                dpy,
                config,
                self.width as i32,
                self.height as i32,
                0,
                self.surfaces.as_mut_ptr(),
                count as i32,
                &mut context,
            )
        });
        if status != VA_STATUS_SUCCESS {
            return Err(DecodeError::Unsupported(format!(
                "vaCreateContext {}x{}: {}",
                self.width,
                self.height,
                error_str(vt, status)
            )));
        }
        self.context = context;
        Ok(())
    }

    /// Tear the session down and give the display back.
    pub fn into_display(mut self) -> Display {
        self.destroy();
        self.display.take().expect("display present until taken")
    }

    fn dpy(&self) -> sys::VADisplay {
        self.display.as_ref().map_or(std::ptr::null_mut(), Display::raw)
    }

    fn destroy(&mut self) {
        let Ok(vt) = sys::vtable() else { return };
        if self.display.is_none() {
            return;
        }
        let dpy = self.dpy();
        if self.context != VA_INVALID_ID {
            crate::host::native(|| unsafe { (vt.va_destroy_context)(dpy, self.context) });
            self.context = VA_INVALID_ID;
        }
        if !self.surfaces.is_empty() {
            crate::host::native(|| unsafe {
                (vt.va_destroy_surfaces)(dpy, self.surfaces.as_mut_ptr(), self.surfaces.len() as i32)
            });
            self.surfaces.clear();
        }
        if self.config != VA_INVALID_ID {
            crate::host::native(|| unsafe { (vt.va_destroy_config)(dpy, self.config) });
            self.config = VA_INVALID_ID;
        }
    }

    pub fn display(&self) -> &Display {
        self.display.as_ref().expect("display present until taken")
    }
    pub fn format(&self) -> SurfaceFormat {
        self.format
    }
    /// Coded size of the surfaces.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn surface_count(&self) -> usize {
        self.surfaces.len()
    }
    pub fn surface(&self, slot: usize) -> VASurfaceID {
        self.surfaces[slot]
    }

    /// Take a free surface slot.
    pub fn acquire(&mut self) -> Option<usize> {
        let slot = self.in_use.iter().position(|u| !u)?;
        self.in_use[slot] = true;
        Some(slot)
    }

    /// Give a slot back to the pool.
    pub fn release(&mut self, slot: usize) {
        if let Some(u) = self.in_use.get_mut(slot) {
            *u = false;
        }
    }

    /// Submit one picture into `slot`: every buffer is created, the
    /// picture begun, rendered and ended, and the buffers destroyed. The
    /// decode may still be running when this returns ([`Self::sync`]).
    pub fn submit(&mut self, slot: usize, buffers: &[Buffer<'_>]) -> Result<(), DecodeError> {
        let vt = vtable()?;
        let dpy = self.dpy();
        let mut ids: Vec<VABufferID> = Vec::with_capacity(buffers.len());
        let result = (|| {
            for b in buffers {
                if b.element_size == 0 || b.data.len() % b.element_size != 0 {
                    return Err(DecodeError::Bitstream("empty or ragged buffer".into()));
                }
                let mut id = VA_INVALID_ID;
                let status = crate::host::native(|| unsafe {
                    (vt.va_create_buffer)(
                        dpy,
                        self.context,
                        b.kind,
                        b.element_size as u32,
                        (b.data.len() / b.element_size) as u32,
                        b.data.as_ptr() as *mut c_void,
                        &mut id,
                    )
                });
                if status != VA_STATUS_SUCCESS {
                    return Err(va_error(vt, "vaCreateBuffer", status));
                }
                ids.push(id);
            }
            let target = self.surfaces[slot];
            let status =
                crate::host::native(|| unsafe { (vt.va_begin_picture)(dpy, self.context, target) });
            if status != VA_STATUS_SUCCESS {
                return Err(va_error(vt, "vaBeginPicture", status));
            }
            let status = crate::host::native(|| unsafe {
                (vt.va_render_picture)(dpy, self.context, ids.as_mut_ptr(), ids.len() as i32)
            });
            // vaEndPicture runs even after a failed render, to close the picture.
            let end = crate::host::native(|| unsafe { (vt.va_end_picture)(dpy, self.context) });
            if status != VA_STATUS_SUCCESS {
                return Err(va_error(vt, "vaRenderPicture", status));
            }
            if end != VA_STATUS_SUCCESS {
                return Err(va_error(vt, "vaEndPicture", end));
            }
            Ok(())
        })();
        for id in ids {
            crate::host::native(|| unsafe { (vt.va_destroy_buffer)(dpy, id) });
        }
        result
    }

    /// Wait for the decode into `slot` to finish.
    pub fn sync(&self, slot: usize) -> Result<(), DecodeError> {
        let vt = vtable()?;
        let target = self.surfaces[slot];
        let status = crate::host::native(|| unsafe { (vt.va_sync_surface)(self.dpy(), target) });
        if status != VA_STATUS_SUCCESS {
            return Err(va_error(vt, "vaSyncSurface", status));
        }
        Ok(())
    }

    /// Copy the `width` x `height` rectangle at (`x`, `y`) of `slot`
    /// (synchronized) into `out`. `x` and `y` must be even.
    pub fn read(
        &mut self,
        slot: usize,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        out: &mut Planes<'_>,
    ) -> Result<(), DecodeError> {
        if x % 2 != 0
            || y % 2 != 0
            || width == 0
            || height == 0
            || x.checked_add(width).map_or(true, |r| r > self.width)
            || y.checked_add(height).map_or(true, |b| b > self.height)
        {
            return Err(DecodeError::Bitstream("read rectangle outside the surface".into()));
        }
        let row = width as usize * self.format.sample_bytes();
        let chroma_row = ((width as usize + 1) & !1) * self.format.sample_bytes();
        let chroma_rows = (height as usize + 1) / 2;
        if out.luma_stride < row
            || out.chroma_stride < chroma_row
            || out.luma.len() < out.luma_stride * (height as usize - 1) + row
            || out.chroma.len() < out.chroma_stride * (chroma_rows - 1) + chroma_row
        {
            return Err(DecodeError::Bitstream("destination planes too small".into()));
        }
        let vt = vtable()?;
        if self.derive_works != Some(false) {
            match self.read_derived(vt, slot, x, y, height, row, chroma_row, out) {
                Ok(()) => {
                    self.derive_works = Some(true);
                    return Ok(());
                }
                Err(e) if self.derive_works == Some(true) => return Err(e),
                Err(_) => self.derive_works = Some(false),
            }
        }
        self.read_through_image(vt, slot, x, y, height, row, chroma_row, out)
    }

    #[allow(clippy::too_many_arguments)]
    fn read_derived(
        &self,
        vt: &sys::Vtable,
        slot: usize,
        x: u32,
        y: u32,
        height: u32,
        row: usize,
        chroma_row: usize,
        out: &mut Planes<'_>,
    ) -> Result<(), DecodeError> {
        let dpy = self.dpy();
        let mut image = VAImage::zeroed();
        let surface = self.surfaces[slot];
        let status = crate::host::native(|| unsafe { (vt.va_derive_image)(dpy, surface, &mut image) });
        if status != VA_STATUS_SUCCESS {
            return Err(va_error(vt, "vaDeriveImage", status));
        }
        let result = self.copy_image(vt, &image, x, y, height, row, chroma_row, out);
        crate::host::native(|| unsafe { (vt.va_destroy_image)(dpy, image.image_id) });
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn read_through_image(
        &self,
        vt: &sys::Vtable,
        slot: usize,
        x: u32,
        y: u32,
        height: u32,
        row: usize,
        chroma_row: usize,
        out: &mut Planes<'_>,
    ) -> Result<(), DecodeError> {
        let dpy = self.dpy();
        let mut format: VAImageFormat = unsafe { std::mem::zeroed() };
        format.fourcc = self.format.fourcc();
        format.byte_order = 1; // VA_LSB_FIRST
        format.bits_per_pixel = if self.format == SurfaceFormat::Nv12 { 12 } else { 24 };
        let mut image = VAImage::zeroed();
        let status = crate::host::native(|| unsafe {
            (vt.va_create_image)(dpy, &mut format, self.width as i32, self.height as i32, &mut image)
        });
        if status != VA_STATUS_SUCCESS {
            return Err(va_error(vt, "vaCreateImage", status));
        }
        let surface = self.surfaces[slot];
        let status = crate::host::native(|| unsafe {
            (vt.va_get_image)(dpy, surface, 0, 0, self.width, self.height, image.image_id)
        });
        let result = if status != VA_STATUS_SUCCESS {
            Err(va_error(vt, "vaGetImage", status))
        } else {
            self.copy_image(vt, &image, x, y, height, row, chroma_row, out)
        };
        crate::host::native(|| unsafe { (vt.va_destroy_image)(dpy, image.image_id) });
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_image(
        &self,
        vt: &sys::Vtable,
        image: &VAImage,
        x: u32,
        y: u32,
        height: u32,
        row: usize,
        chroma_row: usize,
        out: &mut Planes<'_>,
    ) -> Result<(), DecodeError> {
        if image.format.fourcc != self.format.fourcc() || image.num_planes < 2 {
            return Err(DecodeError::Unsupported(format!(
                "image fourcc {:#x} with {} planes",
                image.format.fourcc, image.num_planes
            )));
        }
        let sb = self.format.sample_bytes();
        let (lp, cp) = (image.pitches[0] as usize, image.pitches[1] as usize);
        let (lo, co) = (image.offsets[0] as usize, image.offsets[1] as usize);
        let (x, y, h) = (x as usize, y as usize, height as usize);
        let chroma_rows = (h + 1) / 2;
        let size = image.data_size as usize;
        if lp < (x * sb + row)
            || cp < (x * sb + chroma_row)
            || lo + lp * (y + h - 1) + x * sb + row > size
            || co + cp * (y / 2 + chroma_rows - 1) + x * sb + chroma_row > size
        {
            return Err(DecodeError::Unsupported("image layout smaller than the surface".into()));
        }
        let dpy = self.dpy();
        let mut data: *mut c_void = std::ptr::null_mut();
        let status = crate::host::native(|| unsafe { (vt.va_map_buffer)(dpy, image.buf, &mut data) });
        if status != VA_STATUS_SUCCESS || data.is_null() {
            return Err(va_error(vt, "vaMapBuffer", status));
        }
        // SAFETY: the mapping covers `data_size` bytes until unmapped;
        // every read below was bounds-checked against it.
        let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size) };
        for r in 0..h {
            let src = lo + lp * (y + r) + x * sb;
            out.luma[r * out.luma_stride..r * out.luma_stride + row]
                .copy_from_slice(&bytes[src..src + row]);
        }
        for r in 0..chroma_rows {
            let src = co + cp * (y / 2 + r) + x * sb;
            out.chroma[r * out.chroma_stride..r * out.chroma_stride + chroma_row]
                .copy_from_slice(&bytes[src..src + chroma_row]);
        }
        crate::host::native(|| unsafe { (vt.va_unmap_buffer)(dpy, image.buf) });
        Ok(())
    }
}

impl Drop for DecodeSession {
    fn drop(&mut self) {
        self.destroy();
    }
}
