//! HEVC decode buffers (`va_dec_hevc.h`, `VAPictureHEVC` from `va.h`).
//!
//! Layouts follow the libva headers field for field; bitfield unions
//! are the `u32` storage word, built with the `*_BIT` shifts below
//! (bit 0 is the first declared field on every libva ABI).

use super::{VA_PADDING_LOW, VA_PADDING_MEDIUM, VASurfaceID};

pub const VA_PICTURE_HEVC_INVALID: u32 = 0x01;
pub const VA_PICTURE_HEVC_FIELD_PIC: u32 = 0x02;
pub const VA_PICTURE_HEVC_BOTTOM_FIELD: u32 = 0x04;
pub const VA_PICTURE_HEVC_LONG_TERM_REFERENCE: u32 = 0x08;
pub const VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE: u32 = 0x10;
pub const VA_PICTURE_HEVC_RPS_ST_CURR_AFTER: u32 = 0x20;
pub const VA_PICTURE_HEVC_RPS_LT_CURR: u32 = 0x40;

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct VAPictureHEVC {
    pub picture_id: VASurfaceID,
    pub pic_order_cnt: i32,
    pub flags: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

impl VAPictureHEVC {
    pub fn invalid() -> Self {
        Self {
            picture_id: super::VA_INVALID_SURFACE,
            pic_order_cnt: 0,
            flags: VA_PICTURE_HEVC_INVALID,
            va_reserved: [0; VA_PADDING_LOW],
        }
    }
}

/// Bit positions in `VAPictureParameterBufferHEVC::pic_fields`.
pub mod pic_fields {
    pub const CHROMA_FORMAT_IDC: u32 = 0; // 2 bits
    pub const SEPARATE_COLOUR_PLANE: u32 = 2;
    pub const PCM_ENABLED: u32 = 3;
    pub const SCALING_LIST_ENABLED: u32 = 4;
    pub const TRANSFORM_SKIP_ENABLED: u32 = 5;
    pub const AMP_ENABLED: u32 = 6;
    pub const STRONG_INTRA_SMOOTHING_ENABLED: u32 = 7;
    pub const SIGN_DATA_HIDING_ENABLED: u32 = 8;
    pub const CONSTRAINED_INTRA_PRED: u32 = 9;
    pub const CU_QP_DELTA_ENABLED: u32 = 10;
    pub const WEIGHTED_PRED: u32 = 11;
    pub const WEIGHTED_BIPRED: u32 = 12;
    pub const TRANSQUANT_BYPASS_ENABLED: u32 = 13;
    pub const TILES_ENABLED: u32 = 14;
    pub const ENTROPY_CODING_SYNC_ENABLED: u32 = 15;
    pub const PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED: u32 = 16;
    pub const LOOP_FILTER_ACROSS_TILES_ENABLED: u32 = 17;
    pub const PCM_LOOP_FILTER_DISABLED: u32 = 18;
    pub const NO_PIC_REORDERING: u32 = 19;
    pub const NO_BI_PRED: u32 = 20;
}

/// Bit positions in `VAPictureParameterBufferHEVC::slice_parsing_fields`.
pub mod slice_parsing_fields {
    pub const LISTS_MODIFICATION_PRESENT: u32 = 0;
    pub const LONG_TERM_REF_PICS_PRESENT: u32 = 1;
    pub const SPS_TEMPORAL_MVP_ENABLED: u32 = 2;
    pub const CABAC_INIT_PRESENT: u32 = 3;
    pub const OUTPUT_FLAG_PRESENT: u32 = 4;
    pub const DEPENDENT_SLICE_SEGMENTS_ENABLED: u32 = 5;
    pub const PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT: u32 = 6;
    pub const SAMPLE_ADAPTIVE_OFFSET_ENABLED: u32 = 7;
    pub const DEBLOCKING_FILTER_OVERRIDE_ENABLED: u32 = 8;
    pub const PPS_DISABLE_DEBLOCKING_FILTER: u32 = 9;
    pub const SLICE_SEGMENT_HEADER_EXTENSION_PRESENT: u32 = 10;
    pub const RAP_PIC: u32 = 11;
    pub const IDR_PIC: u32 = 12;
    pub const INTRA_PIC: u32 = 13;
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAPictureParameterBufferHEVC {
    pub curr_pic: VAPictureHEVC,
    pub reference_frames: [VAPictureHEVC; 15],
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    pub pic_fields: u32,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub init_qp_minus26: i8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    pub slice_parsing_fields: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pic_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub num_extra_slice_header_bits: u8,
    pub st_rps_bits: u32,
    pub va_reserved: [u32; VA_PADDING_MEDIUM],
}

/// Bit positions in `VASliceParameterBufferHEVC::long_slice_flags`.
pub mod long_slice_flags {
    pub const LAST_SLICE_OF_PIC: u32 = 0;
    pub const DEPENDENT_SLICE_SEGMENT: u32 = 1;
    pub const SLICE_TYPE: u32 = 2; // 2 bits
    pub const COLOR_PLANE_ID: u32 = 4; // 2 bits
    pub const SLICE_SAO_LUMA: u32 = 6;
    pub const SLICE_SAO_CHROMA: u32 = 7;
    pub const MVD_L1_ZERO: u32 = 8;
    pub const CABAC_INIT: u32 = 9;
    pub const SLICE_TEMPORAL_MVP_ENABLED: u32 = 10;
    pub const SLICE_DEBLOCKING_FILTER_DISABLED: u32 = 11;
    pub const COLLOCATED_FROM_L0: u32 = 12;
    pub const SLICE_LOOP_FILTER_ACROSS_SLICES_ENABLED: u32 = 13;
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VASliceParameterBufferHEVC {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_byte_offset: u32,
    pub slice_segment_address: u32,
    pub ref_pic_list: [[u8; 15]; 2],
    pub long_slice_flags: u32,
    pub collocated_ref_idx: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub chroma_offset_l0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub chroma_offset_l1: [[i8; 2]; 15],
    pub five_minus_max_num_merge_cand: u8,
    pub num_entry_point_offsets: u16,
    pub entry_offset_to_subset_array: u16,
    pub slice_data_num_emu_prevn_bytes: u16,
    pub va_reserved: [u32; VA_PADDING_LOW - 2],
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VAIQMatrixBufferHEVC {
    pub scaling_list_4x4: [[u8; 16]; 6],
    pub scaling_list_8x8: [[u8; 64]; 6],
    pub scaling_list_16x16: [[u8; 64]; 6],
    pub scaling_list_32x32: [[u8; 64]; 2],
    pub scaling_list_dc_16x16: [u8; 6],
    pub scaling_list_dc_32x32: [u8; 2],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

const _: () = assert!(core::mem::size_of::<VAPictureHEVC>() == 28);
const _: () = assert!(core::mem::size_of::<VAPictureParameterBufferHEVC>() == 604);
const _: () = assert!(core::mem::size_of::<VASliceParameterBufferHEVC>() == 264);
const _: () = assert!(core::mem::size_of::<VAIQMatrixBufferHEVC>() == 1016);
