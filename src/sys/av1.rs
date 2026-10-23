//! AV1 decode buffers (`va_dec_av1.h`).
//!
//! Layouts follow the libva header field for field; the bitfield unions
//! are their storage words, built with the shifts below (bit 0 is the
//! first declared field on every libva ABI).

use super::{VA_PADDING_LOW, VA_PADDING_MEDIUM, VASurfaceID};

/// Bit positions in `seq_info_fields`.
pub mod seq_info {
    pub const STILL_PICTURE: u32 = 0;
    pub const USE_128X128_SUPERBLOCK: u32 = 1;
    pub const ENABLE_FILTER_INTRA: u32 = 2;
    pub const ENABLE_INTRA_EDGE_FILTER: u32 = 3;
    pub const ENABLE_INTERINTRA_COMPOUND: u32 = 4;
    pub const ENABLE_MASKED_COMPOUND: u32 = 5;
    pub const ENABLE_DUAL_FILTER: u32 = 6;
    pub const ENABLE_ORDER_HINT: u32 = 7;
    pub const ENABLE_JNT_COMP: u32 = 8;
    pub const ENABLE_CDEF: u32 = 9;
    pub const MONO_CHROME: u32 = 10;
    pub const COLOR_RANGE: u32 = 11;
    pub const SUBSAMPLING_X: u32 = 12;
    pub const SUBSAMPLING_Y: u32 = 13;
    pub const CHROMA_SAMPLE_POSITION: u32 = 14;
    pub const FILM_GRAIN_PARAMS_PRESENT: u32 = 15;
}

/// Bit positions in `pic_info_fields`.
pub mod pic_info {
    pub const FRAME_TYPE: u32 = 0; // 2 bits
    pub const SHOW_FRAME: u32 = 2;
    pub const SHOWABLE_FRAME: u32 = 3;
    pub const ERROR_RESILIENT_MODE: u32 = 4;
    pub const DISABLE_CDF_UPDATE: u32 = 5;
    pub const ALLOW_SCREEN_CONTENT_TOOLS: u32 = 6;
    pub const FORCE_INTEGER_MV: u32 = 7;
    pub const ALLOW_INTRABC: u32 = 8;
    pub const USE_SUPERRES: u32 = 9;
    pub const ALLOW_HIGH_PRECISION_MV: u32 = 10;
    pub const IS_MOTION_MODE_SWITCHABLE: u32 = 11;
    pub const USE_REF_FRAME_MVS: u32 = 12;
    pub const DISABLE_FRAME_END_UPDATE_CDF: u32 = 13;
    pub const UNIFORM_TILE_SPACING_FLAG: u32 = 14;
    pub const ALLOW_WARPED_MOTION: u32 = 15;
    pub const LARGE_SCALE_TILE: u32 = 16;
}

/// Bit positions in `loop_filter_info_fields`.
pub mod loop_filter_info {
    pub const SHARPNESS_LEVEL: u8 = 0; // 3 bits
    pub const MODE_REF_DELTA_ENABLED: u8 = 3;
    pub const MODE_REF_DELTA_UPDATE: u8 = 4;
}

/// Bit positions in `qmatrix_fields`.
pub mod qmatrix {
    pub const USING_QMATRIX: u16 = 0;
    pub const QM_Y: u16 = 1; // 4 bits
    pub const QM_U: u16 = 5; // 4 bits
    pub const QM_V: u16 = 9; // 4 bits
}

/// Bit positions in `mode_control_fields`.
pub mod mode_control {
    pub const DELTA_Q_PRESENT_FLAG: u32 = 0;
    pub const LOG2_DELTA_Q_RES: u32 = 1; // 2 bits
    pub const DELTA_LF_PRESENT_FLAG: u32 = 3;
    pub const LOG2_DELTA_LF_RES: u32 = 4; // 2 bits
    pub const DELTA_LF_MULTI: u32 = 6;
    pub const TX_MODE: u32 = 7; // 2 bits
    pub const REFERENCE_SELECT: u32 = 9;
    pub const REDUCED_TX_SET_USED: u32 = 10;
    pub const SKIP_MODE_PRESENT: u32 = 11;
}

/// Bit positions in `loop_restoration_fields`.
pub mod loop_restoration {
    pub const YFRAME_RESTORATION_TYPE: u16 = 0; // 2 bits
    pub const CBFRAME_RESTORATION_TYPE: u16 = 2; // 2 bits
    pub const CRFRAME_RESTORATION_TYPE: u16 = 4; // 2 bits
    pub const LR_UNIT_SHIFT: u16 = 6; // 2 bits
    pub const LR_UV_SHIFT: u16 = 8;
}

/// Bit positions in `segment_info_fields`.
pub mod segment_info {
    pub const ENABLED: u32 = 0;
    pub const UPDATE_MAP: u32 = 1;
    pub const TEMPORAL_UPDATE: u32 = 2;
    pub const UPDATE_DATA: u32 = 3;
}

/// Bit positions in `film_grain_info_fields`.
pub mod film_grain_info {
    pub const APPLY_GRAIN: u32 = 0;
    pub const CHROMA_SCALING_FROM_LUMA: u32 = 1;
    pub const GRAIN_SCALING_MINUS_8: u32 = 2; // 2 bits
    pub const AR_COEFF_LAG: u32 = 4; // 2 bits
    pub const AR_COEFF_SHIFT_MINUS_6: u32 = 6; // 2 bits
    pub const GRAIN_SCALE_SHIFT: u32 = 8; // 2 bits
    pub const OVERLAP_FLAG: u32 = 10;
    pub const CLIP_TO_RESTRICTED_RANGE: u32 = 11;
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VASegmentationStructAV1 {
    pub segment_info_fields: u32,
    pub feature_data: [[i16; 8]; 8],
    pub feature_mask: [u8; 8],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAFilmGrainStructAV1 {
    pub film_grain_info_fields: u32,
    pub grain_seed: u16,
    pub num_y_points: u8,
    pub point_y_value: [u8; 14],
    pub point_y_scaling: [u8; 14],
    pub num_cb_points: u8,
    pub point_cb_value: [u8; 10],
    pub point_cb_scaling: [u8; 10],
    pub num_cr_points: u8,
    pub point_cr_value: [u8; 10],
    pub point_cr_scaling: [u8; 10],
    pub ar_coeffs_y: [i8; 24],
    pub ar_coeffs_cb: [i8; 25],
    pub ar_coeffs_cr: [i8; 25],
    pub cb_mult: u8,
    pub cb_luma_mult: u8,
    pub cb_offset: u16,
    pub cr_mult: u8,
    pub cr_luma_mult: u8,
    pub cr_offset: u16,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAWarpedMotionParamsAV1 {
    /// `VAAV1TransformationType`: 0 identity, 1 translation, 2 rotzoom, 3 affine.
    pub wmtype: u32,
    pub wmmat: [i32; 8],
    pub invalid: u8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VADecPictureParameterBufferAV1 {
    pub profile: u8,
    pub order_hint_bits_minus_1: u8,
    pub bit_depth_idx: u8,
    pub matrix_coefficients: u8,
    pub seq_info_fields: u32,
    pub current_frame: VASurfaceID,
    pub current_display_picture: VASurfaceID,
    pub anchor_frames_num: u8,
    pub anchor_frames_list: *mut VASurfaceID,
    pub frame_width_minus1: u16,
    pub frame_height_minus1: u16,
    pub output_frame_width_in_tiles_minus_1: u16,
    pub output_frame_height_in_tiles_minus_1: u16,
    pub ref_frame_map: [VASurfaceID; 8],
    pub ref_frame_idx: [u8; 7],
    pub primary_ref_frame: u8,
    pub order_hint: u8,
    pub seg_info: VASegmentationStructAV1,
    pub film_grain_info: VAFilmGrainStructAV1,
    pub tile_cols: u8,
    pub tile_rows: u8,
    pub width_in_sbs_minus_1: [u16; 63],
    pub height_in_sbs_minus_1: [u16; 63],
    pub tile_count_minus_1: u16,
    pub context_update_tile_id: u16,
    pub pic_info_fields: u32,
    pub superres_scale_denominator: u8,
    pub interp_filter: u8,
    pub filter_level: [u8; 2],
    pub filter_level_u: u8,
    pub filter_level_v: u8,
    pub loop_filter_info_fields: u8,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
    pub base_qindex: u8,
    pub y_dc_delta_q: i8,
    pub u_dc_delta_q: i8,
    pub u_ac_delta_q: i8,
    pub v_dc_delta_q: i8,
    pub v_ac_delta_q: i8,
    pub qmatrix_fields: u16,
    pub mode_control_fields: u32,
    pub cdef_damping_minus_3: u8,
    pub cdef_bits: u8,
    pub cdef_y_strengths: [u8; 8],
    pub cdef_uv_strengths: [u8; 8],
    pub loop_restoration_fields: u16,
    pub wm: [VAWarpedMotionParamsAV1; 7],
    pub va_reserved: [u32; VA_PADDING_MEDIUM],
}

impl VADecPictureParameterBufferAV1 {
    /// Every field zero, the anchor list null.
    pub fn zeroed() -> Self {
        // SAFETY: plain integers and one raw pointer, all valid as zero.
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct VASliceParameterBufferAV1 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub tile_row: u16,
    pub tile_column: u16,
    pub tg_start: u16,
    pub tg_end: u16,
    pub anchor_frame_idx: u8,
    pub tile_idx_in_tile_list: u16,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    // Offsets and sizes of libva 2.22's va_dec_av1.h on LP64 (x86-64,
    // aarch64), as a C compiler lays the header out.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn layouts() {
        type P = VADecPictureParameterBufferAV1;
        type G = VAFilmGrainStructAV1;
        type T = VASliceParameterBufferAV1;
        assert_eq!(size_of::<VASegmentationStructAV1>(), 156);
        assert_eq!(size_of::<G>(), 176);
        assert_eq!(size_of::<VAWarpedMotionParamsAV1>(), 56);
        assert_eq!(size_of::<P>(), 1160);
        assert_eq!(size_of::<T>(), 40);
        for (offset, expected) in [
            (offset_of!(G, ar_coeffs_y), 77),
            (offset_of!(G, cb_mult), 151),
            (offset_of!(G, cb_offset), 154),
            (offset_of!(G, cr_offset), 158),
            (offset_of!(G, va_reserved), 160),
            (offset_of!(P, current_frame), 8),
            (offset_of!(P, anchor_frames_num), 16),
            (offset_of!(P, anchor_frames_list), 24),
            (offset_of!(P, frame_width_minus1), 32),
            (offset_of!(P, ref_frame_map), 40),
            (offset_of!(P, ref_frame_idx), 72),
            (offset_of!(P, order_hint), 80),
            (offset_of!(P, seg_info), 84),
            (offset_of!(P, film_grain_info), 240),
            (offset_of!(P, tile_cols), 416),
            (offset_of!(P, width_in_sbs_minus_1), 418),
            (offset_of!(P, tile_count_minus_1), 670),
            (offset_of!(P, pic_info_fields), 676),
            (offset_of!(P, superres_scale_denominator), 680),
            (offset_of!(P, loop_filter_info_fields), 686),
            (offset_of!(P, ref_deltas), 687),
            (offset_of!(P, base_qindex), 697),
            (offset_of!(P, qmatrix_fields), 704),
            (offset_of!(P, mode_control_fields), 708),
            (offset_of!(P, cdef_damping_minus_3), 712),
            (offset_of!(P, cdef_y_strengths), 714),
            (offset_of!(P, loop_restoration_fields), 730),
            (offset_of!(P, wm), 732),
            (offset_of!(P, va_reserved), 1124),
            (offset_of!(T, tile_row), 12),
            (offset_of!(T, anchor_frame_idx), 20),
            (offset_of!(T, tile_idx_in_tile_list), 22),
            (offset_of!(T, va_reserved), 24),
        ] {
            assert_eq!(offset, expected);
        }
    }
}
