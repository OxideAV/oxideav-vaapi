//! VP9 decode buffers (`va_dec_vp9.h`).
//!
//! Layouts follow the libva header field for field; the `pic_fields`
//! and `segment_flags` bitfield unions are their storage words, built
//! with the shifts below (bit 0 is the first declared field on every
//! libva ABI).

use super::{VA_PADDING_LOW, VA_PADDING_MEDIUM, VASurfaceID};

/// Bit positions in `VADecPictureParameterBufferVP9::pic_fields`.
pub mod pic_fields {
    pub const SUBSAMPLING_X: u32 = 0;
    pub const SUBSAMPLING_Y: u32 = 1;
    pub const FRAME_TYPE: u32 = 2;
    pub const SHOW_FRAME: u32 = 3;
    pub const ERROR_RESILIENT_MODE: u32 = 4;
    pub const INTRA_ONLY: u32 = 5;
    pub const ALLOW_HIGH_PRECISION_MV: u32 = 6;
    pub const MCOMP_FILTER_TYPE: u32 = 7; // 3 bits
    pub const FRAME_PARALLEL_DECODING_MODE: u32 = 10;
    pub const RESET_FRAME_CONTEXT: u32 = 11; // 2 bits
    pub const REFRESH_FRAME_CONTEXT: u32 = 13;
    pub const FRAME_CONTEXT_IDX: u32 = 14; // 2 bits
    pub const SEGMENTATION_ENABLED: u32 = 16;
    pub const SEGMENTATION_TEMPORAL_UPDATE: u32 = 17;
    pub const SEGMENTATION_UPDATE_MAP: u32 = 18;
    pub const LAST_REF_FRAME: u32 = 19; // 3 bits
    pub const LAST_REF_FRAME_SIGN_BIAS: u32 = 22;
    pub const GOLDEN_REF_FRAME: u32 = 23; // 3 bits
    pub const GOLDEN_REF_FRAME_SIGN_BIAS: u32 = 26;
    pub const ALT_REF_FRAME: u32 = 27; // 3 bits
    pub const ALT_REF_FRAME_SIGN_BIAS: u32 = 30;
    pub const LOSSLESS_FLAG: u32 = 31;
}

/// Bit positions in `VASegmentParameterVP9::segment_flags`.
pub mod segment_flags {
    pub const SEGMENT_REFERENCE_ENABLED: u16 = 0;
    pub const SEGMENT_REFERENCE: u16 = 1; // 2 bits
    pub const SEGMENT_REFERENCE_SKIPPED: u16 = 3;
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct VADecPictureParameterBufferVP9 {
    pub frame_width: u16,
    pub frame_height: u16,
    pub reference_frames: [VASurfaceID; 8],
    pub pic_fields: u32,
    pub filter_level: u8,
    pub sharpness_level: u8,
    pub log2_tile_rows: u8,
    pub log2_tile_columns: u8,
    pub frame_header_length_in_bytes: u8,
    pub first_partition_size: u16,
    pub mb_segment_tree_probs: [u8; 7],
    pub segment_pred_probs: [u8; 3],
    pub profile: u8,
    pub bit_depth: u8,
    pub va_reserved: [u32; VA_PADDING_MEDIUM],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct VASegmentParameterVP9 {
    pub segment_flags: u16,
    pub filter_level: [[u8; 2]; 4],
    pub luma_ac_quant_scale: i16,
    pub luma_dc_quant_scale: i16,
    pub chroma_ac_quant_scale: i16,
    pub chroma_dc_quant_scale: i16,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct VASliceParameterBufferVP9 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub seg_param: [VASegmentParameterVP9; 8],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    // Sizes and offsets of the x86-64 / aarch64 libva ABI.
    #[test]
    fn layouts() {
        assert_eq!(
            offset_of!(VADecPictureParameterBufferVP9, reference_frames),
            4
        );
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, pic_fields), 36);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, filter_level), 40);
        assert_eq!(
            offset_of!(VADecPictureParameterBufferVP9, first_partition_size),
            46
        );
        assert_eq!(
            offset_of!(VADecPictureParameterBufferVP9, mb_segment_tree_probs),
            48
        );
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, profile), 58);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, va_reserved), 60);
        assert_eq!(size_of::<VADecPictureParameterBufferVP9>(), 92);
        assert_eq!(offset_of!(VASegmentParameterVP9, filter_level), 2);
        assert_eq!(offset_of!(VASegmentParameterVP9, luma_ac_quant_scale), 10);
        assert_eq!(offset_of!(VASegmentParameterVP9, va_reserved), 20);
        assert_eq!(size_of::<VASegmentParameterVP9>(), 36);
        assert_eq!(align_of::<VASegmentParameterVP9>(), 4);
        assert_eq!(offset_of!(VASliceParameterBufferVP9, seg_param), 12);
        assert_eq!(size_of::<VASliceParameterBufferVP9>(), 12 + 8 * 36 + 16);
    }
}
