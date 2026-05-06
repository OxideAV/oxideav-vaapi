//! H.264 decode glue — the Round 5 retry of the Round 3 silent-fail.
//!
//! Builds a minimal end-to-end IDR decode path on top of the existing
//! [`Display`], [`Config`], [`Context`] wrappers, using the shared
//! [`oxideav_bitstream::h264`] parser instead of the (now-removed)
//! Round 3 in-tree parser. The submission flow is:
//!
//! 1. `oxideav_bitstream::h264::parse_idr_only(stream)` → SPS + PPS +
//!    IDR slice header + slice data slab. (For streaming use, the
//!    `H264VaCodecDecoder` adapter splits on Annex-B boundaries and
//!    caches SPS/PPS across packets so that a slice-only packet can be
//!    decoded against the SPS/PPS seen on a previous packet — same
//!    statefulness `cuvidParser` provides on the NVDEC side.)
//! 2. Build [`VAPictureParameterBufferH264`],
//!    [`VASliceParameterBufferH264`] and [`VAIQMatrixBufferH264`] from
//!    the parsed structs.
//! 3. `vaCreateBuffer × 4` (PicParam + IQMatrix + SliceParam +
//!    SliceData), `vaBeginPicture` → `vaRenderPicture` ×4 →
//!    `vaEndPicture` → `vaSyncSurface`.
//! 4. Surface read-back via `vaCreateImage(NV12) + vaGetImage` (the
//!    `nvidia-vaapi-driver` shim does not implement `vaDeriveImage`,
//!    so we go straight to the create+get fallback).
//! 5. NV12 → I420 deinterleave for the public [`DecodedFrame`] API.
//!
//! ## What we know about the wall
//!
//! Round 3 attempted this exact flow with an in-tree parser and saw
//! every libva entry point return `VA_STATUS_SUCCESS`, but the
//! resulting surface came back as constant `0x80` luma / `0x80/0x80`
//! chroma — i.e. NVDEC was never actually invoked. The Round 3 parser
//! was not unit-tested against a known-good submission, so the wall
//! could equally have been a parser bug or a real driver issue. This
//! module retries the same flow with the shared, unit-tested
//! `oxideav-bitstream` parser and surfaces whatever happens — a
//! cross-validated frame, the same silent-fail signature, or
//! something in between.

use std::ffi::c_void;

use oxideav_bitstream::h264 as bs_h264;
use oxideav_bitstream::BitstreamError;

use crate::config::Config;
use crate::context::Context;
use crate::display::{Display, VaError};
use crate::sys::{
    self, buffer_type, error_str, VABufferID, VAIQMatrixBufferH264, VAImage, VAImageFormat,
    VAPictureH264, VAPictureParameterBufferH264, VASliceParameterBufferH264, VA_FOURCC_NV12,
    VA_INVALID_SURFACE, VA_PICTURE_H264_INVALID, VA_SLICE_DATA_FLAG_ALL, VA_STATUS_SUCCESS,
};

impl From<BitstreamError> for VaError {
    fn from(e: BitstreamError) -> Self {
        VaError::Va {
            status: 0,
            message: format!("bitstream parser: {e}"),
        }
    }
}

/// Decoded frame in I420 layout returned from [`H264VaDecoder::decode_idr`].
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Y plane (`width * height` bytes, row-major).
    pub y: Vec<u8>,
    /// U plane (`(width/2) * (height/2)` bytes, row-major).
    pub u: Vec<u8>,
    /// V plane (`(width/2) * (height/2)` bytes, row-major).
    pub v: Vec<u8>,
}

/// Single-IDR H.264 VA-API decoder. Owns the [`Config`] + [`Context`]
/// for the chosen `(profile, entrypoint)` pair plus a single
/// `VA_RT_FORMAT_YUV420` render-target surface sized to the SPS.
pub struct H264VaDecoder<'d> {
    dpy_raw: sys::VADisplay,
    coded_width: u32,
    coded_height: u32,
    display_width: u32,
    display_height: u32,
    _config: Config<'d>,
    context: Context<'d>,
}

impl<'d> H264VaDecoder<'d> {
    /// Build a decoder configured from the SPS/PPS embedded in
    /// `annex_b`. Allocates the underlying VA-API config + context for
    /// H.264 High at the SPS-derived coded dimensions.
    pub fn new(dpy: &'d Display, annex_b: &[u8]) -> Result<Self, VaError> {
        let parsed = bs_h264::parse_idr_only(annex_b)?;
        Self::from_sps(dpy, &parsed.sps)
    }

    /// Build a decoder from an already-parsed SPS. Used by the
    /// streaming registry adapter that caches SPS/PPS across packets
    /// and constructs the inner decoder lazily once the first SPS NAL
    /// has been observed.
    pub fn from_sps(dpy: &'d Display, sps: &bs_h264::H264Sps) -> Result<Self, VaError> {
        let coded_width = sps.coded_width();
        let coded_height = sps.coded_height();
        let display_width = sps.display_width();
        let display_height = sps.display_height();

        let config = Config::new(
            dpy,
            sys::profile::VAProfileH264High,
            sys::entrypoint::VAEntrypointVLD,
            &[],
        )?;
        let context = Context::new(dpy, &config, coded_width, coded_height, 1)?;

        Ok(Self {
            dpy_raw: dpy.raw(),
            coded_width,
            coded_height,
            display_width,
            display_height,
            _config: config,
            context,
        })
    }

    /// Coded width derived from the SPS (MB-aligned).
    pub fn coded_width(&self) -> u32 {
        self.coded_width
    }

    /// Coded height derived from the SPS (MB-aligned).
    pub fn coded_height(&self) -> u32 {
        self.coded_height
    }

    /// Cropped display width.
    pub fn display_width(&self) -> u32 {
        self.display_width
    }

    /// Cropped display height.
    pub fn display_height(&self) -> u32 {
        self.display_height
    }

    /// Render-target surface id (single surface for the IDR decode).
    pub fn surface(&self) -> sys::VASurfaceID {
        self.context.surfaces()[0]
    }

    /// Decode the first IDR access unit in `annex_b` and return the
    /// resulting frame.
    ///
    /// The caller is expected to feed an Annex-B byte slab containing
    /// at least an SPS, a PPS and an IDR slice in that order. The IDR
    /// access unit slab returned by `parse_idr_only` is the slice data
    /// payload — not the SPS/PPS — but for a one-frame file we
    /// submit the entire input as the slice data slab and let the
    /// driver-side bitstream extractor seek to the IDR NAL. Both
    /// approaches are valid against the libva spec; submitting the
    /// whole file is more conservative and matches what
    /// `oxideav-vdpau` does for VDPAU.
    pub fn decode_idr(&self, annex_b: &[u8]) -> Result<DecodedFrame, VaError> {
        let parsed = bs_h264::parse_idr_only(annex_b)?;
        self.decode_slice(
            &parsed.sps,
            &parsed.pps,
            &parsed.slice_header,
            bs_h264::NAL_TYPE_IDR,
            parsed.idr_access_unit,
        )
    }

    /// Decode a single H.264 slice given the cached SPS/PPS the slice
    /// references. Used by the streaming registry adapter where SPS
    /// and PPS may have been parsed from earlier packets, but the
    /// current packet contains only slice data.
    ///
    /// `slice_access_unit` must start with an Annex-B start code
    /// followed by the slice NAL (header byte + EBSP).
    pub fn decode_slice(
        &self,
        sps: &bs_h264::H264Sps,
        pps: &bs_h264::H264Pps,
        slice_header: &bs_h264::H264SliceHeader,
        nal_unit_type: u8,
        slice_access_unit: &[u8],
    ) -> Result<DecodedFrame, VaError> {
        // Compute the absolute bit offset (from the start of the NAL
        // header byte, after emulation-prevention stripping) of the
        // start of slice_data() — VA-API requires this so the GPU
        // bitstream extractor knows where to begin entropy-decoding.
        let slice_data_bit_offset =
            compute_slice_data_bit_offset(sps, pps, nal_unit_type, slice_access_unit)?;

        // Build parameter buffers.
        let mut pic_param = build_pic_param(sps, pps, slice_header, self.surface());
        let mut iq_matrix = VAIQMatrixBufferH264::flat();

        // The slice data we submit to the GPU is the slice NAL itself
        // (start code + NAL header + slice_header + slice_data + …).
        let slice_data: &[u8] = slice_access_unit;

        // The slice_data_offset field in VASliceParameterBufferH264
        // points at the NAL header byte (i.e. just past the start
        // code) inside the submitted slice data buffer. Compute that
        // offset by counting the start-code bytes (3 or 4).
        let nal_header_offset = start_code_len(slice_data) as u32;

        let mut slice_param = build_slice_param(
            pps,
            slice_header,
            nal_unit_type,
            slice_data.len() as u32,
            nal_header_offset,
            slice_data_bit_offset,
        );

        // Submit. We allocate four buffers, render them in order, then
        // destroy them all in the reverse-of-creation order on the way
        // out. Note: the libva spec says destroying the buffers is
        // optional — vaEndPicture transfers ownership — but cleaning
        // up explicitly is safer if we ever loop.
        let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;

        let pic_param_buf = create_buffer(
            self.dpy_raw,
            self.context.id(),
            buffer_type::VAPictureParameterBufferType,
            std::mem::size_of::<VAPictureParameterBufferH264>() as u32,
            &mut pic_param as *mut _ as *mut c_void,
        )?;
        let iq_matrix_buf = create_buffer(
            self.dpy_raw,
            self.context.id(),
            buffer_type::VAIQMatrixBufferType,
            std::mem::size_of::<VAIQMatrixBufferH264>() as u32,
            &mut iq_matrix as *mut _ as *mut c_void,
        )?;
        let slice_param_buf = create_buffer(
            self.dpy_raw,
            self.context.id(),
            buffer_type::VASliceParameterBufferType,
            std::mem::size_of::<VASliceParameterBufferH264>() as u32,
            &mut slice_param as *mut _ as *mut c_void,
        )?;
        let slice_data_buf = create_buffer(
            self.dpy_raw,
            self.context.id(),
            buffer_type::VASliceDataBufferType,
            slice_data.len() as u32,
            slice_data.as_ptr() as *mut c_void,
        )?;

        // SAFETY: surface and context are valid (we own them).
        let status =
            unsafe { (vt.va_begin_picture)(self.dpy_raw, self.context.id(), self.surface()) };
        if status != VA_STATUS_SUCCESS {
            destroy_buffer(self.dpy_raw, slice_data_buf);
            destroy_buffer(self.dpy_raw, slice_param_buf);
            destroy_buffer(self.dpy_raw, iq_matrix_buf);
            destroy_buffer(self.dpy_raw, pic_param_buf);
            return Err(VaError::Va {
                status,
                message: error_str(vt, status),
            });
        }

        // vaRenderPicture: submit the four buffers in a single call.
        let mut buffers = [pic_param_buf, iq_matrix_buf, slice_param_buf, slice_data_buf];
        let status = unsafe {
            (vt.va_render_picture)(
                self.dpy_raw,
                self.context.id(),
                buffers.as_mut_ptr(),
                buffers.len() as i32,
            )
        };
        if status != VA_STATUS_SUCCESS {
            // End picture even on render error — libva transitions the
            // context state machine on EndPicture.
            let _ = unsafe { (vt.va_end_picture)(self.dpy_raw, self.context.id()) };
            destroy_buffer(self.dpy_raw, slice_data_buf);
            destroy_buffer(self.dpy_raw, slice_param_buf);
            destroy_buffer(self.dpy_raw, iq_matrix_buf);
            destroy_buffer(self.dpy_raw, pic_param_buf);
            return Err(VaError::Va {
                status,
                message: error_str(vt, status),
            });
        }

        // SAFETY: matched with begin_picture.
        let status = unsafe { (vt.va_end_picture)(self.dpy_raw, self.context.id()) };
        if status != VA_STATUS_SUCCESS {
            destroy_buffer(self.dpy_raw, slice_data_buf);
            destroy_buffer(self.dpy_raw, slice_param_buf);
            destroy_buffer(self.dpy_raw, iq_matrix_buf);
            destroy_buffer(self.dpy_raw, pic_param_buf);
            return Err(VaError::Va {
                status,
                message: error_str(vt, status),
            });
        }

        // SAFETY: surface valid.
        let status = unsafe { (vt.va_sync_surface)(self.dpy_raw, self.surface()) };
        if status != VA_STATUS_SUCCESS {
            destroy_buffer(self.dpy_raw, slice_data_buf);
            destroy_buffer(self.dpy_raw, slice_param_buf);
            destroy_buffer(self.dpy_raw, iq_matrix_buf);
            destroy_buffer(self.dpy_raw, pic_param_buf);
            return Err(VaError::Va {
                status,
                message: error_str(vt, status),
            });
        }

        // Buffers are no longer needed after EndPicture/Sync.
        destroy_buffer(self.dpy_raw, slice_data_buf);
        destroy_buffer(self.dpy_raw, slice_param_buf);
        destroy_buffer(self.dpy_raw, iq_matrix_buf);
        destroy_buffer(self.dpy_raw, pic_param_buf);

        // Read back via `vaCreateImage(NV12) + vaGetImage`. The
        // `nvidia-vaapi-driver` shim does not implement
        // `vaDeriveImage` — we always go through the create-then-get
        // fallback.
        let frame = self.read_back_nv12_as_i420()?;
        Ok(frame)
    }

    fn read_back_nv12_as_i420(&self) -> Result<DecodedFrame, VaError> {
        let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;

        // Build a NV12 VAImageFormat. NV12 is the natural NVDEC output
        // layout — Y plane followed by interleaved UV.
        let mut nv12_fmt = VAImageFormat {
            fourcc: VA_FOURCC_NV12,
            byte_order: 1, // VA_LSB_FIRST = 1
            bits_per_pixel: 12,
            depth: 0,
            red_mask: 0,
            green_mask: 0,
            blue_mask: 0,
            alpha_mask: 0,
            va_reserved: [0; 4],
        };

        let mut img: VAImage = VAImage::zeroed();
        // SAFETY: format / image pointers valid; ints sized.
        let status = unsafe {
            (vt.va_create_image)(
                self.dpy_raw,
                &mut nv12_fmt,
                self.coded_width as i32,
                self.coded_height as i32,
                &mut img,
            )
        };
        if status != VA_STATUS_SUCCESS {
            return Err(VaError::Va {
                status,
                message: format!("vaCreateImage(NV12): {}", error_str(vt, status)),
            });
        }

        // SAFETY: image just created, surface valid.
        let status = unsafe {
            (vt.va_get_image)(
                self.dpy_raw,
                self.surface(),
                0,
                0,
                self.coded_width,
                self.coded_height,
                img.image_id,
            )
        };
        if status != VA_STATUS_SUCCESS {
            // Best-effort cleanup.
            unsafe {
                let _ = (vt.va_destroy_image)(self.dpy_raw, img.image_id);
            }
            return Err(VaError::Va {
                status,
                message: format!("vaGetImage(NV12): {}", error_str(vt, status)),
            });
        }

        // Map the buffer behind the image.
        let mut p: *mut c_void = std::ptr::null_mut();
        // SAFETY: img.buf is the buffer just allocated by createImage.
        let status = unsafe { (vt.va_map_buffer)(self.dpy_raw, img.buf, &mut p) };
        if status != VA_STATUS_SUCCESS {
            unsafe {
                let _ = (vt.va_destroy_image)(self.dpy_raw, img.image_id);
            }
            return Err(VaError::Va {
                status,
                message: format!("vaMapBuffer(image): {}", error_str(vt, status)),
            });
        }

        // Copy NV12 → planar I420.
        let frame = unsafe { copy_nv12_image_to_i420(p as *const u8, &img) };

        // SAFETY: same buffer.
        let _ = unsafe { (vt.va_unmap_buffer)(self.dpy_raw, img.buf) };
        // SAFETY: same image.
        let _ = unsafe { (vt.va_destroy_image)(self.dpy_raw, img.image_id) };

        Ok(frame)
    }
}

// ─────────────────────────── helpers ─────────────────────────────────────────

fn create_buffer(
    dpy: sys::VADisplay,
    ctx: sys::VAContextID,
    buf_type: u32,
    size: u32,
    data: *mut c_void,
) -> Result<VABufferID, VaError> {
    let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;
    let mut id: VABufferID = 0;
    // SAFETY: dpy/ctx valid; data points to a `size`-byte allocation
    // owned by the caller.
    let status =
        unsafe { (vt.va_create_buffer)(dpy, ctx, buf_type, size, 1, data, &mut id) };
    if status != VA_STATUS_SUCCESS {
        return Err(VaError::Va {
            status,
            message: format!("vaCreateBuffer(type={buf_type}): {}", error_str(vt, status)),
        });
    }
    Ok(id)
}

fn destroy_buffer(dpy: sys::VADisplay, id: VABufferID) {
    if let Ok(vt) = sys::vtable() {
        // SAFETY: id came from a successful vaCreateBuffer.
        unsafe {
            let _ = (vt.va_destroy_buffer)(dpy, id);
        }
    }
}

/// Length of the Annex-B start code at the beginning of `slice_data`
/// (3 or 4 bytes). Returns 0 if no start code is present (caller has
/// already framed past it).
fn start_code_len(slice_data: &[u8]) -> usize {
    if slice_data.len() >= 4
        && slice_data[0] == 0
        && slice_data[1] == 0
        && slice_data[2] == 0
        && slice_data[3] == 1
    {
        4
    } else if slice_data.len() >= 3
        && slice_data[0] == 0
        && slice_data[1] == 0
        && slice_data[2] == 1
    {
        3
    } else {
        0
    }
}

/// Build the H.264 picture parameter buffer from cached SPS/PPS plus
/// the parsed slice header for the current frame.
fn build_pic_param(
    sps: &bs_h264::H264Sps,
    pps: &bs_h264::H264Pps,
    sh: &bs_h264::H264SliceHeader,
    target_surface: sys::VASurfaceID,
) -> VAPictureParameterBufferH264 {

    // CurrPic — the surface the GPU will write into.
    let curr_pic = VAPictureH264 {
        picture_id: target_surface,
        frame_idx: sh.frame_num,
        flags: VA_PICTURE_H264_INVALID & 0, // cleared — frame is valid
        // For an IDR with pic_order_cnt_type==0 and idr_pic_id=0,
        // the POC is 0/0 (8.2.1).
        top_field_order_cnt: 0,
        bottom_field_order_cnt: 0,
        va_reserved: [0; 4],
    };

    // No reference frames for an IDR — every slot is "invalid".
    let invalid_pic = VAPictureH264 {
        picture_id: VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        top_field_order_cnt: 0,
        bottom_field_order_cnt: 0,
        va_reserved: [0; 4],
    };
    let reference_frames = [invalid_pic; 16];

    // Bitfield packing — see va.h `seq_fields.bits` definition for
    // the bit positions. Order is (low-to-high in the bitfield):
    //   chroma_format_idc:2,
    //   residual_colour_transform_flag:1, // separate_colour_plane_flag
    //   gaps_in_frame_num_value_allowed_flag:1,
    //   frame_mbs_only_flag:1,
    //   mb_adaptive_frame_field_flag:1,
    //   direct_8x8_inference_flag:1,
    //   MinLumaBiPredSize8x8:1,
    //   log2_max_frame_num_minus4:4,
    //   pic_order_cnt_type:2,
    //   log2_max_pic_order_cnt_lsb_minus4:4,
    //   delta_pic_order_always_zero_flag:1
    let mut seq_fields: u32 = 0;
    seq_fields |= (sps.chroma_format_idc as u32) & 0x3;
    seq_fields |= ((sps.separate_colour_plane_flag as u32) & 0x1) << 2;
    seq_fields |= ((sps.gaps_in_frame_num_value_allowed_flag as u32) & 0x1) << 3;
    seq_fields |= ((sps.frame_mbs_only_flag as u32) & 0x1) << 4;
    seq_fields |= ((sps.mb_adaptive_frame_field_flag as u32) & 0x1) << 5;
    seq_fields |= ((sps.direct_8x8_inference_flag as u32) & 0x1) << 6;
    // MinLumaBiPredSize8x8 = 1 if level_idc >= 31 (per A.3.3.2),
    // otherwise 0. For our 320x240 fixture we're at level 1.0 → 0.
    let min_luma_bipred = if sps.level_idc >= 31 { 1u32 } else { 0u32 };
    seq_fields |= min_luma_bipred << 7;
    seq_fields |= ((sps.log2_max_frame_num_minus4 as u32) & 0xF) << 8;
    seq_fields |= ((sps.pic_order_cnt_type as u32) & 0x3) << 12;
    seq_fields |= ((sps.log2_max_pic_order_cnt_lsb_minus4 as u32) & 0xF) << 14;
    seq_fields |= ((sps.delta_pic_order_always_zero_flag as u32) & 0x1) << 18;

    // pic_fields:
    //   entropy_coding_mode_flag:1,
    //   weighted_pred_flag:1,
    //   weighted_bipred_idc:2,
    //   transform_8x8_mode_flag:1,
    //   field_pic_flag:1,
    //   constrained_intra_pred_flag:1,
    //   pic_order_present_flag:1,
    //   deblocking_filter_control_present_flag:1,
    //   redundant_pic_cnt_present_flag:1,
    //   reference_pic_flag:1
    let mut pic_fields: u32 = 0;
    pic_fields |= (pps.entropy_coding_mode_flag as u32) & 0x1;
    pic_fields |= ((pps.weighted_pred_flag as u32) & 0x1) << 1;
    pic_fields |= ((pps.weighted_bipred_idc as u32) & 0x3) << 2;
    pic_fields |= ((pps.transform_8x8_mode_flag as u32) & 0x1) << 4;
    pic_fields |= ((sh.field_pic_flag as u32) & 0x1) << 5;
    pic_fields |= ((pps.constrained_intra_pred_flag as u32) & 0x1) << 6;
    pic_fields |= ((pps.bottom_field_pic_order_in_frame_present_flag as u32) & 0x1) << 7;
    pic_fields |= ((pps.deblocking_filter_control_present_flag as u32) & 0x1) << 8;
    pic_fields |= ((pps.redundant_pic_cnt_present_flag as u32) & 0x1) << 9;
    // reference_pic_flag = nal_ref_idc != 0; for an IDR, nal_ref_idc is
    // always > 0 (NAL type 5 must have nal_ref_idc != 0 per 7.4.1).
    pic_fields |= 1u32 << 10;

    VAPictureParameterBufferH264 {
        curr_pic,
        reference_frames,
        picture_width_in_mbs_minus1: sps.pic_width_in_mbs_minus1 as u16,
        picture_height_in_mbs_minus1: sps.pic_height_in_map_units_minus1 as u16,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        num_ref_frames: sps.max_num_ref_frames as u8,
        seq_fields,
        num_slice_groups_minus1: 0,
        slice_group_map_type: 0,
        slice_group_change_rate_minus1: 0,
        pic_init_qp_minus26: pps.pic_init_qp_minus26 as i8,
        pic_init_qs_minus26: pps.pic_init_qs_minus26 as i8,
        chroma_qp_index_offset: pps.chroma_qp_index_offset as i8,
        second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset as i8,
        pic_fields,
        frame_num: sh.frame_num as u16,
        va_reserved: [0; 8],
    }
}

/// Build the H.264 slice parameter buffer from cached PPS + parsed
/// slice header.
///
/// `nal_unit_type` is currently unused (an IDR I-slice has no inter
/// references and the ref-pic lists are filled with invalid sentinels
/// regardless), but it's threaded through for future support of
/// non-IDR slices.
fn build_slice_param(
    pps: &bs_h264::H264Pps,
    sh: &bs_h264::H264SliceHeader,
    nal_unit_type: u8,
    slice_data_size: u32,
    slice_data_offset: u32,
    slice_data_bit_offset: u16,
) -> VASliceParameterBufferH264 {
    let _ = nal_unit_type;

    // For an I-slice all ref pic list entries are unused.
    let invalid_pic = VAPictureH264 {
        picture_id: VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        top_field_order_cnt: 0,
        bottom_field_order_cnt: 0,
        va_reserved: [0; 4],
    };
    let ref_pic_list0 = [invalid_pic; 32];
    let ref_pic_list1 = [invalid_pic; 32];

    VASliceParameterBufferH264 {
        slice_data_size,
        slice_data_offset,
        slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
        slice_data_bit_offset,
        first_mb_in_slice: sh.first_mb_in_slice as u16,
        slice_type: sh.slice_type,
        direct_spatial_mv_pred_flag: 0,
        num_ref_idx_l0_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        cabac_init_idc: 0,
        slice_qp_delta: 0,
        disable_deblocking_filter_idc: 0,
        slice_alpha_c0_offset_div2: 0,
        slice_beta_offset_div2: 0,
        ref_pic_list0,
        ref_pic_list1,
        luma_log2_weight_denom: 0,
        chroma_log2_weight_denom: 0,
        luma_weight_l0_flag: 0,
        luma_weight_l0: [0; 32],
        luma_offset_l0: [0; 32],
        chroma_weight_l0_flag: 0,
        chroma_weight_l0: [[0; 2]; 32],
        chroma_offset_l0: [[0; 2]; 32],
        luma_weight_l1_flag: 0,
        luma_weight_l1: [0; 32],
        luma_offset_l1: [0; 32],
        chroma_weight_l1_flag: 0,
        chroma_weight_l1: [[0; 2]; 32],
        chroma_offset_l1: [[0; 2]; 32],
        va_reserved: [0; 4],
    }
}

/// Compute `slice_data_bit_offset` in bits — number of bits in the
/// slice NAL covering NAL header byte (8) + slice_header(), with
/// emulation-prevention bytes already stripped (per va.h doc string).
///
/// We re-walk the slice header from RBSP bytes, tracking
/// [`oxideav_bitstream::bit_reader::BitReader::bit_pos`] all the way
/// through to where slice_data() begins. The minimal parser in
/// `oxideav-bitstream` stops earlier (at pic_order_cnt_lsb), so we
/// extend it here to cover an IDR I-slice with no FMO and the
/// CAVLC/CABAC trailing fields up to slice_data().
fn compute_slice_data_bit_offset(
    sps: &bs_h264::H264Sps,
    pps: &bs_h264::H264Pps,
    nal_unit_type: u8,
    access_unit: &[u8],
) -> Result<u16, VaError> {
    use oxideav_bitstream::bit_reader::BitReader;

    // Locate the slice NAL body inside the access unit slab and strip
    // emulation-prevention bytes. The access unit slab starts with
    // the start code, then the NAL header byte, then the EBSP.
    let sc = start_code_len(access_unit);
    if access_unit.len() <= sc {
        return Err(VaError::Va {
            status: 0,
            message: "slice_data_bit_offset: empty slice NAL".into(),
        });
    }
    let nal_header_byte = access_unit[sc];
    let ebsp = &access_unit[sc + 1..];
    let rbsp = bs_h264::ebsp_to_rbsp(ebsp);

    let mut r = BitReader::new(&rbsp);

    // 7.3.3 slice_header() — re-parse for bit-counting purposes.
    let _first_mb_in_slice = r.ue()?;
    let slice_type = r.ue()?;
    let _pps_id = r.ue()?;
    if sps.separate_colour_plane_flag {
        let _colour_plane_id = r.u(2);
    }
    let _frame_num = r.u(sps.log2_max_frame_num_minus4 as u32 + 4);
    let mut field_pic_flag = false;
    if !sps.frame_mbs_only_flag {
        field_pic_flag = r.u(1) != 0;
        if field_pic_flag {
            let _bottom_field_flag = r.u(1);
        }
    }
    if nal_unit_type == bs_h264::NAL_TYPE_IDR {
        let _idr_pic_id = r.ue()?;
    }
    if sps.pic_order_cnt_type == 0 {
        let _pic_order_cnt_lsb = r.u(sps.log2_max_pic_order_cnt_lsb_minus4 as u32 + 4);
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            let _delta_pic_order_cnt_bottom = r.se()?;
        }
    } else if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero_flag {
        let _d0 = r.se()?;
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            let _d1 = r.se()?;
        }
    }
    if pps.redundant_pic_cnt_present_flag {
        let _redundant_pic_cnt = r.ue()?;
    }

    let slice_type_mod5 = slice_type % 5;
    let is_b = slice_type_mod5 == 1;
    let is_p = slice_type_mod5 == 0 || slice_type_mod5 == 3;
    let is_i = slice_type_mod5 == 2 || slice_type_mod5 == 4;
    let is_sp = slice_type_mod5 == 3;
    let is_si = slice_type_mod5 == 4;
    if is_b {
        let _direct_spatial_mv_pred_flag = r.u(1);
    }
    let mut _l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1 as u32;
    let mut _l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1 as u32;
    if is_p || is_sp || is_b {
        let num_ref_idx_active_override_flag = r.u(1);
        if num_ref_idx_active_override_flag != 0 {
            _l0_active_minus1 = r.ue()?;
            if is_b {
                _l1_active_minus1 = r.ue()?;
            }
        }
    }
    // ref_pic_list_modification() — for IDR I-slice both list flags
    // are 0 (no entries). Spec: 7.3.3.1.
    if !is_i && !is_si {
        let ref_pic_list_modification_flag_l0 = r.u(1);
        if ref_pic_list_modification_flag_l0 != 0 {
            // modification_of_pic_nums_idc loop — iterate until 3.
            loop {
                let m = r.ue()?;
                if m == 3 {
                    break;
                }
                if m == 0 || m == 1 {
                    let _abs_diff_pic_num_minus1 = r.ue()?;
                } else if m == 2 {
                    let _long_term_pic_num = r.ue()?;
                } else {
                    return Err(VaError::Va {
                        status: 0,
                        message: format!("ref_pic_list_modification_l0: invalid modification_of_pic_nums_idc={m}"),
                    });
                }
            }
        }
    }
    if is_b {
        let ref_pic_list_modification_flag_l1 = r.u(1);
        if ref_pic_list_modification_flag_l1 != 0 {
            loop {
                let m = r.ue()?;
                if m == 3 {
                    break;
                }
                if m == 0 || m == 1 {
                    let _abs = r.ue()?;
                } else if m == 2 {
                    let _ltpn = r.ue()?;
                } else {
                    return Err(VaError::Va {
                        status: 0,
                        message: format!("ref_pic_list_modification_l1: invalid m={m}"),
                    });
                }
            }
        }
    }
    // pred_weight_table() — only emitted for weighted P/SP or B slices.
    if (pps.weighted_pred_flag && (is_p || is_sp))
        || (pps.weighted_bipred_idc == 1 && is_b)
    {
        return Err(VaError::Va {
            status: 0,
            message: "compute_slice_data_bit_offset: pred_weight_table not implemented (fixture should not need it)".into(),
        });
    }
    // dec_ref_pic_marking() — H.264 7.3.3.3. For nal_ref_idc != 0
    // (always for IDR slice = NAL type 5), this block is emitted. For
    // an IDR (nal_unit_type == 5):
    //   no_output_of_prior_pics_flag  u(1)
    //   long_term_reference_flag       u(1)
    let nal_ref_idc = (nal_header_byte >> 5) & 0x3;
    if nal_ref_idc != 0 {
        if nal_unit_type == bs_h264::NAL_TYPE_IDR {
            let _no_output_of_prior_pics_flag = r.u(1);
            let _long_term_reference_flag = r.u(1);
        } else {
            let adaptive_ref_pic_marking_mode_flag = r.u(1);
            if adaptive_ref_pic_marking_mode_flag != 0 {
                loop {
                    let mm = r.ue()?;
                    if mm == 0 {
                        break;
                    }
                    match mm {
                        1 | 3 => {
                            let _diff = r.ue()?;
                        }
                        2 => {
                            let _ltpn = r.ue()?;
                        }
                        4 => {
                            let _max = r.ue()?;
                        }
                        5 => {}
                        6 => {
                            let _ltfi = r.ue()?;
                        }
                        _ => {
                            return Err(VaError::Va {
                                status: 0,
                                message: format!("dec_ref_pic_marking: invalid mm={mm}"),
                            });
                        }
                    }
                    if mm == 3 {
                        let _ltfi = r.ue()?;
                    }
                }
            }
        }
    }
    // cabac_init_idc — only for entropy_coding_mode_flag && slice_type
    // is not I and not SI.
    if pps.entropy_coding_mode_flag && !is_i && !is_si {
        let _cabac_init_idc = r.ue()?;
    }
    let _slice_qp_delta = r.se()?;
    if is_sp || is_si {
        if is_sp {
            let _sp_for_switch_flag = r.u(1);
        }
        let _slice_qs_delta = r.se()?;
    }
    if pps.deblocking_filter_control_present_flag {
        let disable_deblocking_filter_idc = r.ue()?;
        if disable_deblocking_filter_idc != 1 {
            let _slice_alpha = r.se()?;
            let _slice_beta = r.se()?;
        }
    }
    // FMO slice_group_change_cycle skipped (we reject FMO upstream).

    // For CABAC (entropy_coding_mode_flag), slice_data starts at the
    // next byte boundary. For CAVLC, it starts immediately at the
    // current bit position.
    let mut bits_in_header_rbsp = r.bit_pos() as u32;
    if pps.entropy_coding_mode_flag {
        let rem = bits_in_header_rbsp % 8;
        if rem != 0 {
            bits_in_header_rbsp += 8 - rem;
        }
    }

    // va.h: slice_data_bit_offset is "relative to and includes the
    // NAL unit byte" — so add 8 for the NAL header byte itself.
    let total_bits = 8 + bits_in_header_rbsp;
    if total_bits > u16::MAX as u32 {
        return Err(VaError::Va {
            status: 0,
            message: format!("slice_data_bit_offset overflow: {total_bits}"),
        });
    }
    Ok(total_bits as u16)
}

// ─────────────────────────── oxideav_core::Decoder integration ───────────────

#[cfg(feature = "registry")]
mod registry_impl {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters, Decoder, Frame, Packet, VideoFrame, VideoPlane};

    /// Streaming H.264 decoder for the codec registry.
    ///
    /// Owns its own [`Display`] handle (opened from
    /// `/dev/dri/renderD128`), caches SPS / PPS as they arrive in the
    /// packet stream (so a slice-only packet that follows a packet
    /// containing SPS+PPS still decodes — same statefulness
    /// `cuvidParser` provides on the NVDEC side), and lazily
    /// constructs the inner [`H264VaDecoder`] once the first SPS NAL
    /// has been observed.
    ///
    /// Each `send_packet` walks the packet's Annex-B NALs:
    ///
    /// * `NAL_TYPE_SPS` → parse and cache.
    /// * `NAL_TYPE_PPS` → parse and cache.
    /// * `NAL_TYPE_IDR` / `NAL_TYPE_NON_IDR_SLICE` → require both
    ///   cached SPS and cached PPS, parse the slice header, build
    ///   parameter buffers from the cached SPS/PPS plus the parsed
    ///   slice header, and submit. Errors out if no SPS/PPS has been
    ///   seen yet.
    /// * Other NAL types (AUD, SEI, etc.) are ignored.
    ///
    /// Scope: this is the same single-frame I/IDR shape as
    /// [`H264VaDecoder::decode_idr`] — there is no DPB, no P/B-frame
    /// inter-prediction, no frame reordering. The intent is to land
    /// the cross-validated proof that VA-API decode works on this
    /// driver and to give the registry a priority-10 hardware
    /// implementation that the future framework-level scheduler can
    /// pick up. A full streaming decoder will be a follow-up round.
    pub struct H264VaCodecDecoder {
        codec_id: CodecId,
        // Heap-pinned so the &Display borrow inside `decoder` stays
        // valid for as long as `Self` lives. We only ever borrow it
        // immutably from one place (the inner `H264VaDecoder`) so
        // Box::leak avoids the self-referential-struct headache.
        display: &'static Display,
        // None until the first SPS has been observed.
        decoder: Option<H264VaDecoder<'static>>,
        // Cached parameter sets. Populated as SPS / PPS NALs flow
        // through `send_packet`; consulted on every slice NAL.
        cached_sps: Option<bs_h264::H264Sps>,
        cached_pps: Option<bs_h264::H264Pps>,
        pending: Option<Frame>,
    }

    // SAFETY: `VADisplay` is documented as not thread-safe, but the
    // `Decoder: Send` contract is "owner moves between threads with
    // exclusive access" — not "concurrent access". A single-threaded
    // decode path that calls `send_packet` / `receive_frame` strictly
    // from one thread at a time satisfies libva's serialization rule.
    unsafe impl Send for H264VaCodecDecoder {}

    impl H264VaCodecDecoder {
        const RENDER_NODE: &'static str = "/dev/dri/renderD128";

        pub fn new(codec_id: CodecId) -> oxideav_core::Result<Self> {
            let dpy = Display::open_drm(std::path::Path::new(Self::RENDER_NODE)).map_err(
                |e| {
                    oxideav_core::Error::unsupported(format!(
                        "VA-API: open DRM render node failed: {e}"
                    ))
                },
            )?;
            // Heap-pin the display so the &'static borrow inside
            // `decoder` is sound. Dropped manually in `Drop`.
            let display: &'static mut Display = Box::leak(Box::new(dpy));
            Ok(Self {
                codec_id,
                display,
                decoder: None,
                cached_sps: None,
                cached_pps: None,
                pending: None,
            })
        }

        /// Decode one slice NAL, building an Annex-B access-unit slab
        /// from the NAL bytes (so `decode_slice` sees a 4-byte start
        /// code at index 0 like it would in a full Annex-B stream).
        fn decode_one_slice(
            &mut self,
            nal: &[u8],
            nal_unit_type: u8,
            pts: Option<i64>,
        ) -> oxideav_core::Result<()> {
            let sps = self.cached_sps.as_ref().ok_or_else(|| {
                oxideav_core::Error::other(
                    "VA-API: no SPS available before slice (stream malformed?)",
                )
            })?;
            let pps = self.cached_pps.as_ref().ok_or_else(|| {
                oxideav_core::Error::other(
                    "VA-API: no PPS available before slice (stream malformed?)",
                )
            })?;

            // Parse just enough of the slice header to populate the
            // VA pic/slice parameter buffers.
            if nal.is_empty() {
                return Err(oxideav_core::Error::other("VA-API: empty slice NAL"));
            }
            let rbsp = bs_h264::ebsp_to_rbsp(&nal[1..]);
            let slice_header = bs_h264::parse_slice_header_minimal(&rbsp, nal_unit_type, sps, pps)
                .map_err(|e| {
                    oxideav_core::Error::other(format!("VA-API: parse slice header: {e}"))
                })?;

            // Build a fresh Annex-B access-unit slab: 4-byte start
            // code + the slice NAL (header byte + EBSP) — this is what
            // `decode_slice` expects.
            let mut access_unit = Vec::with_capacity(4 + nal.len());
            access_unit.extend_from_slice(&[0, 0, 0, 1]);
            access_unit.extend_from_slice(nal);

            // Lazily build the inner decoder now that we have an SPS.
            if self.decoder.is_none() {
                let dec = H264VaDecoder::from_sps(self.display, sps).map_err(|e| {
                    oxideav_core::Error::other(format!("VA-API: H264VaDecoder::from_sps: {e}"))
                })?;
                self.decoder = Some(dec);
            }
            let dec = self.decoder.as_ref().expect("just constructed");

            let frame = dec
                .decode_slice(sps, pps, &slice_header, nal_unit_type, &access_unit)
                .map_err(|e| {
                    oxideav_core::Error::other(format!("VA-API: decode_slice: {e}"))
                })?;

            // Convert DecodedFrame (I420) → oxideav_core::VideoFrame.
            let w = dec.display_width() as usize;
            let h = dec.display_height() as usize;
            let cw = w / 2;
            let ch = h / 2;
            // Crop to display rectangle so consumers get the
            // intended-output sub-rectangle.
            let y = crop_plane(&frame.y, frame.width as usize, frame.height as usize, w, h);
            let u = crop_plane(
                &frame.u,
                (frame.width / 2) as usize,
                (frame.height / 2) as usize,
                cw,
                ch,
            );
            let v = crop_plane(
                &frame.v,
                (frame.width / 2) as usize,
                (frame.height / 2) as usize,
                cw,
                ch,
            );
            self.pending = Some(Frame::Video(VideoFrame {
                pts,
                planes: vec![
                    VideoPlane { stride: w, data: y },
                    VideoPlane { stride: cw, data: u },
                    VideoPlane { stride: cw, data: v },
                ],
            }));
            Ok(())
        }
    }

    impl Drop for H264VaCodecDecoder {
        fn drop(&mut self) {
            // Drop the inner decoder first (releases its Config /
            // Context which borrow Display) and then reclaim the
            // leaked Display box.
            self.decoder = None;
            // SAFETY: pointer was created from a Box::leak in `new`
            // and has not been re-boxed elsewhere.
            unsafe {
                let p = self.display as *const Display as *mut Display;
                let _ = Box::from_raw(p);
            }
        }
    }

    impl Decoder for H264VaCodecDecoder {
        fn codec_id(&self) -> &CodecId {
            &self.codec_id
        }

        fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
            if packet.data.is_empty() {
                return Ok(());
            }
            // Walk every NAL in the packet's Annex-B byte slab. Each
            // NAL drives one of three actions: cache (SPS/PPS), decode
            // (slice), or ignore (everything else).
            for nal in bs_h264::split_annex_b(&packet.data) {
                if nal.is_empty() {
                    continue;
                }
                let (_, _, nal_type) = bs_h264::nal_header(nal[0]);
                match nal_type {
                    bs_h264::NAL_TYPE_SPS => {
                        let sps = bs_h264::parse_sps_nal(nal).map_err(|e| {
                            oxideav_core::Error::other(format!(
                                "VA-API: parse SPS NAL: {e}"
                            ))
                        })?;
                        self.cached_sps = Some(sps);
                    }
                    bs_h264::NAL_TYPE_PPS => {
                        let pps = bs_h264::parse_pps_nal(nal).map_err(|e| {
                            oxideav_core::Error::other(format!(
                                "VA-API: parse PPS NAL: {e}"
                            ))
                        })?;
                        self.cached_pps = Some(pps);
                    }
                    bs_h264::NAL_TYPE_IDR | bs_h264::NAL_TYPE_NON_IDR_SLICE => {
                        self.decode_one_slice(nal, nal_type, packet.pts)?;
                    }
                    _ => {
                        // AUD, SEI, etc. — not relevant for
                        // single-frame IDR-only decode.
                    }
                }
            }
            Ok(())
        }

        fn receive_frame(&mut self) -> oxideav_core::Result<Frame> {
            match self.pending.take() {
                Some(f) => Ok(f),
                None => Err(oxideav_core::Error::NeedMore),
            }
        }

        fn flush(&mut self) -> oxideav_core::Result<()> {
            Ok(())
        }
    }

    fn crop_plane(src: &[u8], src_w: usize, _src_h: usize, dst_w: usize, dst_h: usize) -> Vec<u8> {
        let mut out = vec![0u8; dst_w * dst_h];
        for row in 0..dst_h {
            let s = row * src_w;
            let d = row * dst_w;
            out[d..d + dst_w].copy_from_slice(&src[s..s + dst_w]);
        }
        out
    }

    /// Decoder factory used by [`crate::register`] to wire H.264 High
    /// VA-API decode into the codec registry at priority 10.
    pub fn h264_decoder_factory(
        params: &CodecParameters,
    ) -> oxideav_core::Result<Box<dyn Decoder>> {
        Ok(Box::new(H264VaCodecDecoder::new(params.codec_id.clone())?))
    }
}

#[cfg(feature = "registry")]
pub use registry_impl::{h264_decoder_factory, H264VaCodecDecoder};

/// Read the NV12 image bytes the driver wrote into `image_buf` and
/// return them as a planar I420 [`DecodedFrame`].
///
/// The caller is responsible for sizing the source plane: only the
/// `display_width × display_height` rectangle of each plane is
/// retained; padding rows / columns from MB alignment are dropped.
///
/// # Safety
///
/// `base` must point at a contiguous NV12 buffer produced by
/// `vaGetImage`, valid for `image.data_size` bytes. `image.pitches`
/// and `image.offsets` describe the plane layout.
unsafe fn copy_nv12_image_to_i420(base: *const u8, image: &VAImage) -> DecodedFrame {
    let w = image.width as usize;
    let h = image.height as usize;
    let y_pitch = image.pitches[0] as usize;
    let uv_pitch = image.pitches[1] as usize;
    let y_offset = image.offsets[0] as usize;
    let uv_offset = image.offsets[1] as usize;

    let cw = w / 2;
    let ch = h / 2;

    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];

    // Y plane row-by-row (skip stride padding).
    for row in 0..h {
        let src = unsafe { base.add(y_offset + row * y_pitch) };
        let dst = &mut y[row * w..row * w + w];
        unsafe {
            std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), w);
        }
    }
    // UV interleaved → split.
    for row in 0..ch {
        let src_row = unsafe { base.add(uv_offset + row * uv_pitch) };
        for col in 0..cw {
            let pair = unsafe { std::slice::from_raw_parts(src_row.add(col * 2), 2) };
            u[row * cw + col] = pair[0];
            v[row * cw + col] = pair[1];
        }
    }

    DecodedFrame {
        width: w as u32,
        height: h as u32,
        y,
        u,
        v,
    }
}
