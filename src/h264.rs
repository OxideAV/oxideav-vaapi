//! H.264 decode through VA-API.
//!
//! NAL units, parameter sets and slice headers come from
//! oxideav-bitstream's stream front end
//! ([`oxideav_bitstream::h264::stream::H264Parser`]), which also finds
//! picture boundaries (§7.4.1.2.4); picture order counts (§8.2.1),
//! reference list initialisation and modification (§8.2.4) and reference
//! marking (§8.2.5) from its [`Dpb`]. This module keeps a surface for
//! every reference picture and fills
//! `VAPictureParameterBufferH264`, `VAIQMatrixBufferH264` and one
//! `VASliceParameterBufferH264` per slice with its resolved reference
//! lists and prediction weights.
//!
//! Scope: frame pictures (progressive and MBAFF) of 4:2:0 8-bit streams
//! in the Constrained Baseline, Main and High profiles. Field pictures,
//! other chroma formats and bit depths, FMO / ASO / redundant slices and
//! frame_num gaps are [`DecodeError::Unsupported`], so a caller can hand
//! the stream to a software decoder.
//!
//! Pictures come back in decode order, one per coded picture, as soon as
//! the hardware has finished them; output ordering is the caller's.

use std::collections::BTreeMap;

use oxideav_bitstream::h264::dpb::{Dpb, FrameStore, RefPicture};
use oxideav_bitstream::h264::scaling::picture_scaling_matrix;
use oxideav_bitstream::h264::slice::{PredWeight, SliceHeader, SliceType};
use oxideav_bitstream::h264::stream::{H264Event, H264Parser as Parser};
use oxideav_bitstream::h264::{H264Pps as Pps, H264Sps as Sps};

use crate::display::Display;
use crate::session::{Buffer, DecodeError, DecodeSession, Planes, SurfaceFormat, check_decode};
use crate::sys::{
    VA_PICTURE_H264_LONG_TERM_REFERENCE, VA_PICTURE_H264_SHORT_TERM_REFERENCE,
    VA_SLICE_DATA_FLAG_ALL, VAIQMatrixBufferH264, VAPictureH264, VAPictureParameterBufferH264,
    VASliceParameterBufferH264, buffer_type, profile,
};

/// Bit positions in `VAPictureParameterBufferH264::seq_fields`.
mod seq_fields {
    pub const CHROMA_FORMAT_IDC: u32 = 0; // 2 bits
    pub const GAPS_IN_FRAME_NUM_VALUE_ALLOWED: u32 = 3;
    pub const FRAME_MBS_ONLY: u32 = 4;
    pub const MB_ADAPTIVE_FRAME_FIELD: u32 = 5;
    pub const DIRECT_8X8_INFERENCE: u32 = 6;
    pub const MIN_LUMA_BI_PRED_SIZE_8X8: u32 = 7;
    pub const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 8; // 4 bits
    pub const PIC_ORDER_CNT_TYPE: u32 = 12; // 2 bits
    pub const LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4: u32 = 14; // 4 bits
    pub const DELTA_PIC_ORDER_ALWAYS_ZERO: u32 = 18;
}

/// Bit positions in `VAPictureParameterBufferH264::pic_fields`.
mod pic_fields {
    pub const ENTROPY_CODING_MODE: u32 = 0;
    pub const WEIGHTED_PRED: u32 = 1;
    pub const WEIGHTED_BIPRED_IDC: u32 = 2; // 2 bits
    pub const TRANSFORM_8X8_MODE: u32 = 4;
    pub const CONSTRAINED_INTRA_PRED: u32 = 6;
    pub const PIC_ORDER_PRESENT: u32 = 7;
    pub const DEBLOCKING_FILTER_CONTROL_PRESENT: u32 = 8;
    pub const REDUNDANT_PIC_CNT_PRESENT: u32 = 9;
    pub const REFERENCE_PIC: u32 = 10;
}

/// A decoded picture. Its surface stays valid until the next call into
/// the decoder.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct H264Picture {
    /// Surface slot in the decoder's session.
    pub slot: usize,
    /// `PicOrderCnt( CurrPic )`, after an MMCO 5 has reset it.
    pub poc: i32,
    /// The frame cropping rectangle: left, top, width, height.
    pub crop: (u32, u32, u32, u32),
    /// 8.
    pub bit_depth: u8,
    /// `true` for an IDR picture.
    pub idr: bool,
    /// `true` when output order restarts at this picture: an IDR, or a
    /// picture with memory_management_control_operation 5.
    pub starts_sequence: bool,
}

/// What a sequence needs from the driver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct H264Requirements {
    pub va_profile: i32,
    pub coded_width: u32,
    pub coded_height: u32,
    pub surfaces: usize,
}

struct Slice {
    header: SliceHeader,
    /// The coded NAL unit, header byte included.
    coded: Vec<u8>,
    /// Bits of the NAL header byte and the slice header (RBSP bits).
    data_bit_offset: u32,
}

struct InProgress {
    sps: Sps,
    pps: Pps,
    nal_unit_type: u8,
    nal_ref_idc: u8,
    slices: Vec<Slice>,
}

/// See the module documentation.
pub struct H264Decoder {
    display: Option<Display>,
    session: Option<DecodeSession>,
    requirements: Option<H264Requirements>,
    parser: Parser,
    dpb: Dpb,
    /// Surface slot of each DPB frame store, by key.
    slots: BTreeMap<u32, usize>,
    prev_ref_frame_num: Option<u32>,
    pending: Option<InProgress>,
    /// Slots of pictures returned since the last call that the DPB does not hold.
    returned_only: Vec<usize>,
    out: Vec<H264Picture>,
}

impl H264Decoder {
    pub fn new(display: Display) -> Self {
        H264Decoder {
            display: Some(display),
            session: None,
            requirements: None,
            parser: Parser::new(),
            dpb: Dpb::new(),
            slots: BTreeMap::new(),
            prev_ref_frame_num: None,
            pending: None,
            returned_only: Vec::new(),
            out: Vec::new(),
        }
    }

    /// The VA profile and coded size a sequence needs, or why this bridge
    /// cannot decode it.
    pub fn requirements(sps: &Sps) -> Result<H264Requirements, DecodeError> {
        if sps.chroma_format_idc != 1 || sps.separate_colour_plane_flag {
            return Err(DecodeError::Unsupported(format!(
                "H.264 chroma_format_idc {}",
                sps.chroma_format_idc
            )));
        }
        if sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
            return Err(DecodeError::Unsupported("H.264 above 8 bits".into()));
        }
        if sps.qpprime_y_zero_transform_bypass_flag {
            return Err(DecodeError::Unsupported(
                "H.264 lossless macroblocks".into(),
            ));
        }
        // constraint_set1_flag, the second bit of the coded byte.
        let constrained = sps.constraint_set_flags & 0x40 != 0;
        let va_profile = match sps.profile_idc {
            66 if constrained => profile::VAProfileH264ConstrainedBaseline,
            77 => profile::VAProfileH264Main,
            // A Baseline stream that is also Main conformant (constraint_set1).
            66 => {
                return Err(DecodeError::Unsupported(
                    "H.264 Baseline without constraint_set1".into(),
                ));
            }
            100 => profile::VAProfileH264High,
            p => return Err(DecodeError::Unsupported(format!("H.264 profile_idc {p}"))),
        };
        let coded_width = sps.pic_width_in_mbs() * 16;
        let coded_height = sps.frame_height_in_mbs() * 16;
        let surfaces = (sps.max_num_ref_frames as usize + 2).clamp(4, 18);
        Ok(H264Requirements {
            va_profile,
            coded_width,
            coded_height,
            surfaces,
        })
    }

    /// Whether `display` decodes such a sequence.
    pub fn supports(display: &Display, r: &H264Requirements) -> Result<(), DecodeError> {
        check_decode(
            display,
            r.va_profile,
            SurfaceFormat::Nv12,
            r.coded_width,
            r.coded_height,
        )
    }

    /// Create the session for the last sequence parameter set seen, so a
    /// driver that refuses it is found before any picture.
    pub fn prepare(&mut self) -> Result<(), DecodeError> {
        let sps = (0..32)
            .rev()
            .find_map(|id| self.parser.sps(id).cloned())
            .ok_or_else(|| DecodeError::Bitstream("no sequence parameter set".into()))?;
        let r = Self::requirements(&sps)?;
        self.ensure_session(r, true)
    }

    pub fn session(&self) -> Option<&DecodeSession> {
        self.session.as_ref()
    }

    /// Feed coded NAL units (header byte first, emulation prevention in
    /// place); returns the pictures they completed.
    pub fn decode<'a>(
        &mut self,
        units: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Vec<H264Picture>, DecodeError> {
        for slot in std::mem::take(&mut self.returned_only) {
            if let Some(s) = self.session.as_mut() {
                s.release(slot);
            }
        }
        for unit in units {
            self.push(unit)?;
        }
        Ok(std::mem::take(&mut self.out))
    }

    /// Finish the picture in progress (the end of an access unit).
    pub fn flush(&mut self) -> Result<Vec<H264Picture>, DecodeError> {
        self.finish_picture()?;
        Ok(std::mem::take(&mut self.out))
    }

    pub fn read(
        &mut self,
        picture: &H264Picture,
        planes: &mut Planes<'_>,
    ) -> Result<(), DecodeError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| DecodeError::Bitstream("no session".into()))?;
        let (x, y, w, h) = picture.crop;
        session.read(picture.slot, x, y, w, h, planes)
    }

    fn ensure_session(&mut self, r: H264Requirements, idr: bool) -> Result<(), DecodeError> {
        if self.requirements == Some(r) && self.session.is_some() {
            return Ok(());
        }
        if self.session.is_some() && !idr {
            return Err(DecodeError::Bitstream(
                "sequence change without an IDR picture".into(),
            ));
        }
        let display = match self.session.take() {
            Some(s) => s.into_display(),
            None => self
                .display
                .take()
                .ok_or_else(|| DecodeError::Bitstream("decoder lost its display".into()))?,
        };
        self.dpb.clear();
        self.slots.clear();
        self.returned_only.clear();
        self.requirements = None;
        match DecodeSession::new(
            display,
            r.va_profile,
            SurfaceFormat::Nv12,
            r.coded_width,
            r.coded_height,
            r.surfaces,
        ) {
            Ok(s) => {
                self.session = Some(s);
                self.requirements = Some(r);
                Ok(())
            }
            Err((display, e)) => {
                self.display = Some(display);
                Err(e)
            }
        }
    }

    fn push(&mut self, coded: &[u8]) -> Result<(), DecodeError> {
        if coded.is_empty() {
            return Ok(());
        }
        let nal_type = coded[0] & 0x1F;
        // A non-VCL unit that begins an access unit ends the picture (§7.4.1.2.3).
        if matches!(nal_type, 6..=11 | 13..=18) {
            self.finish_picture()?;
        }
        let event = self
            .parser
            .process_nal(coded)
            .map_err(|e| DecodeError::Bitstream(format!("H.264 NAL unit: {e:?}")))?;
        let H264Event::Slice(slice) = event else {
            return Ok(());
        };
        let (sps, pps) = self
            .parser
            .active(&slice)
            .map(|(s, p)| (s.clone(), p.clone()))
            .ok_or_else(|| DecodeError::Bitstream("slice without parameter sets".into()))?;
        let first_of_picture = slice.first_of_picture;
        let header = slice.header;
        let (nal_unit_type, nal_ref_idc) = (header.nal_unit_type, header.nal_ref_idc);
        if header.field_pic_flag {
            return Err(DecodeError::Unsupported("H.264 field pictures".into()));
        }
        if pps.num_slice_groups_minus1 > 0 {
            return Err(DecodeError::Unsupported("H.264 slice groups".into()));
        }
        if header.redundant_pic_cnt > 0 {
            return Ok(());
        }
        if matches!(header.slice_type, SliceType::SP | SliceType::SI) {
            return Err(DecodeError::Unsupported("H.264 switching slices".into()));
        }
        if self.pending.is_none() || first_of_picture {
            self.finish_picture()?;
            self.begin_picture(nal_unit_type, nal_ref_idc, &header, sps, pps)?;
        }
        // In RBSP bits: the driver skips emulation prevention bytes itself.
        let data_bit_offset = 8 + header.header_bits as u32;
        self.pending
            .as_mut()
            .expect("picture begun")
            .slices
            .push(Slice {
                header,
                coded: coded.to_vec(),
                data_bit_offset,
            });
        Ok(())
    }

    fn begin_picture(
        &mut self,
        nal_unit_type: u8,
        nal_ref_idc: u8,
        header: &SliceHeader,
        sps: Sps,
        pps: Pps,
    ) -> Result<(), DecodeError> {
        let idr = nal_unit_type == 5;
        if header.first_mb_in_slice != 0 {
            return Err(DecodeError::Unsupported(
                "H.264 arbitrary slice order".into(),
            ));
        }
        let r = Self::requirements(&sps)?;
        if !idr && self.session.is_none() {
            return Err(DecodeError::Bitstream(
                "stream does not start with an IDR picture".into(),
            ));
        }
        self.ensure_session(r, idr)?;
        let max_frame_num = sps.max_frame_num();
        if !idr
            && let Some(prev) = self.prev_ref_frame_num
            && header.frame_num != prev
            && header.frame_num != (prev + 1) % max_frame_num
        {
            return Err(if sps.gaps_in_frame_num_value_allowed_flag {
                DecodeError::Unsupported("H.264 frame_num gap".into())
            } else {
                DecodeError::Bitstream("frame_num out of sequence".into())
            });
        }
        self.pending = Some(InProgress {
            sps,
            pps,
            nal_unit_type,
            nal_ref_idc,
            slices: Vec::new(),
        });
        Ok(())
    }

    fn finish_picture(&mut self) -> Result<(), DecodeError> {
        let Some(p) = self.pending.take() else {
            return Ok(());
        };
        if p.slices.is_empty() {
            return Ok(());
        }
        // The DPB is only committed once the picture has been decoded.
        let saved = self.dpb.clone();
        let result = self.decode_picture(&p);
        if result.is_err() {
            self.dpb = saved;
        }
        result
    }

    fn decode_picture(&mut self, p: &InProgress) -> Result<(), DecodeError> {
        let sps = &p.sps;
        let pps = &p.pps;
        let first = &p.slices[0].header;
        let idr = p.nal_unit_type == 5;
        let reference = p.nal_ref_idc != 0;
        let bitstream = |e: oxideav_bitstream::BitstreamError| {
            DecodeError::Bitstream(format!("H.264 reference pictures: {e:?}"))
        };
        let current = self.dpb.begin_picture(sps, first).map_err(bitstream)?;

        let session = self.session.as_mut().expect("session ensured");
        let target = session.acquire().ok_or_else(|| {
            DecodeError::Bitstream("more pictures held than the DPB allows".into())
        })?;

        // ReferenceFrames[]: every frame store with a reference field.
        let picture_of = |s: &FrameStore, session: &DecodeSession, slots: &BTreeMap<u32, usize>| {
            let slot = *slots.get(&s.key)?;
            let long = s.long_term_frame_idx();
            Some(VAPictureH264 {
                picture_id: session.surface(slot),
                frame_idx: long.unwrap_or(s.frame_num),
                flags: if long.is_some() {
                    VA_PICTURE_H264_LONG_TERM_REFERENCE
                } else {
                    VA_PICTURE_H264_SHORT_TERM_REFERENCE
                },
                top_field_order_cnt: s.top().poc,
                bottom_field_order_cnt: s.bottom().poc,
                va_reserved: [0; 4],
            })
        };
        let mut pic = picture_parameters(sps, pps, first, reference);
        pic.curr_pic = VAPictureH264 {
            picture_id: session.surface(target),
            frame_idx: first.frame_num,
            flags: 0,
            top_field_order_cnt: current.poc.top_field_order_cnt,
            bottom_field_order_cnt: current.poc.bottom_field_order_cnt,
            va_reserved: [0; 4],
        };
        let mut n = 0;
        for s in self.dpb.references() {
            if let Some(v) = picture_of(s, session, &self.slots)
                && n < 16
            {
                pic.reference_frames[n] = v;
                n += 1;
            }
        }
        let iq = iq_matrix(sps, pps);

        let mut slices = Vec::with_capacity(p.slices.len());
        for s in &p.slices {
            let h = &s.header;
            let mut sp = slice_parameters(h, pps, s.coded.len(), s.data_bit_offset);
            if h.slice_type.has_list0() {
                if self.dpb.references().next().is_none() {
                    session.release(target);
                    return Err(DecodeError::Bitstream(
                        "inter slice without reference pictures".into(),
                    ));
                }
                let lists = match self.dpb.ref_pic_lists(sps, h) {
                    Ok(l) => l,
                    Err(e) => {
                        session.release(target);
                        return Err(bitstream(e));
                    }
                };
                for (list, out) in [
                    (&lists.list0, &mut sp.ref_pic_list0),
                    (&lists.list1, &mut sp.ref_pic_list1),
                ] {
                    for (i, entry) in list.iter().enumerate().take(32) {
                        // "No reference picture" entries stay invalid.
                        let Some(RefPicture { key, .. }) = entry else {
                            continue;
                        };
                        let store = self.dpb.store(*key);
                        match store.and_then(|s| picture_of(s, session, &self.slots)) {
                            Some(v) => out[i] = v,
                            None => {
                                session.release(target);
                                return Err(DecodeError::Unsupported(
                                    "H.264 reference without a decoded picture".into(),
                                ));
                            }
                        }
                    }
                }
            }
            slices.push(sp);
        }

        let mut buffers = vec![
            Buffer::of(buffer_type::VAPictureParameterBufferType, &pic),
            Buffer::of(buffer_type::VAIQMatrixBufferType, &iq),
        ];
        for (sp, s) in slices.iter().zip(&p.slices) {
            buffers.push(Buffer::of(buffer_type::VASliceParameterBufferType, sp));
            buffers.push(Buffer::bytes(buffer_type::VASliceDataBufferType, &s.coded));
        }
        if let Err(e) = session
            .submit(target, &buffers)
            .and_then(|()| session.sync(target))
        {
            session.release(target);
            return Err(e);
        }

        // §8.2.5: marking, then the picture joins the DPB if it is a reference.
        let finished = match self.dpb.finish_picture(sps) {
            Ok(f) => f,
            Err(e) => {
                session.release(target);
                return Err(bitstream(e));
            }
        };
        if reference {
            self.prev_ref_frame_num = Some(if finished.mmco5 { 0 } else { first.frame_num });
        }
        // Stores no longer used for reference give their surfaces back.
        for key in &finished.released {
            if let Some(slot) = self.slots.remove(key) {
                session.release(slot);
            }
        }
        if finished.stored {
            self.slots.insert(finished.key, target);
        } else {
            self.returned_only.push(target);
        }

        let (left, top, crop_w, crop_h) = sps.crop_rect();
        self.out.push(H264Picture {
            slot: target,
            poc: finished.pic_order_cnt,
            crop: (left, top, crop_w, crop_h),
            bit_depth: 8,
            idr,
            starts_sequence: idr || finished.mmco5,
        });
        Ok(())
    }
}

fn bit(flag: bool, at: u32) -> u32 {
    u32::from(flag) << at
}

fn picture_parameters(
    sps: &Sps,
    pps: &Pps,
    h: &SliceHeader,
    reference: bool,
) -> VAPictureParameterBufferH264 {
    let transform_8x8 = pps.transform_8x8_mode_flag;
    // Equal to chroma_qp_index_offset when the PPS does not code it.
    let second_chroma = pps.second_chroma_qp_index_offset;
    use seq_fields::*;
    let seq = (u32::from(sps.chroma_format_idc) & 3) << CHROMA_FORMAT_IDC
        | bit(sps.gaps_in_frame_num_value_allowed_flag, GAPS_IN_FRAME_NUM_VALUE_ALLOWED)
        | bit(sps.frame_mbs_only_flag, FRAME_MBS_ONLY)
        | bit(sps.mb_adaptive_frame_field_flag, MB_ADAPTIVE_FRAME_FIELD)
        | bit(sps.direct_8x8_inference_flag, DIRECT_8X8_INFERENCE)
        // Table A-1 / A.3.3: MinLumaBiPredSize 8x8 from level 3.1 in Main and High.
        | bit(sps.level_idc >= 31 && sps.profile_idc != 66, MIN_LUMA_BI_PRED_SIZE_8X8)
        | (u32::from(sps.log2_max_frame_num_minus4) & 15) << LOG2_MAX_FRAME_NUM_MINUS4
        | (u32::from(sps.pic_order_cnt_type) & 3) << PIC_ORDER_CNT_TYPE
        | (u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) & 15)
            << LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4
        | bit(sps.delta_pic_order_always_zero_flag, DELTA_PIC_ORDER_ALWAYS_ZERO);
    use pic_fields::*;
    let fields = bit(pps.entropy_coding_mode_flag, ENTROPY_CODING_MODE)
        | bit(pps.weighted_pred_flag, WEIGHTED_PRED)
        | (u32::from(pps.weighted_bipred_idc) & 3) << WEIGHTED_BIPRED_IDC
        | bit(transform_8x8, TRANSFORM_8X8_MODE)
        | bit(pps.constrained_intra_pred_flag, CONSTRAINED_INTRA_PRED)
        | bit(
            pps.bottom_field_pic_order_in_frame_present_flag,
            PIC_ORDER_PRESENT,
        )
        | bit(
            pps.deblocking_filter_control_present_flag,
            DEBLOCKING_FILTER_CONTROL_PRESENT,
        )
        | bit(
            pps.redundant_pic_cnt_present_flag,
            REDUNDANT_PIC_CNT_PRESENT,
        )
        | bit(reference, REFERENCE_PIC);
    VAPictureParameterBufferH264 {
        curr_pic: VAPictureH264::invalid(),
        reference_frames: [VAPictureH264::invalid(); 16],
        picture_width_in_mbs_minus1: sps.pic_width_in_mbs_minus1 as u16,
        picture_height_in_mbs_minus1: (sps.frame_height_in_mbs() - 1) as u16,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        num_ref_frames: sps.max_num_ref_frames as u8,
        seq_fields: seq,
        num_slice_groups_minus1: 0,
        slice_group_map_type: 0,
        slice_group_change_rate_minus1: 0,
        pic_init_qp_minus26: pps.pic_init_qp_minus26 as i8,
        pic_init_qs_minus26: pps.pic_init_qs_minus26 as i8,
        chroma_qp_index_offset: pps.chroma_qp_index_offset as i8,
        second_chroma_qp_index_offset: second_chroma as i8,
        pic_fields: fields,
        frame_num: h.frame_num as u16,
        va_reserved: [0; 8],
    }
}

/// The effective scaling lists (PPS over SPS, fall-back rules A and B,
/// §7.4.2.1.1 / §7.4.2.2) for `VAIQMatrixBufferH264`: the six 4x4 lists
/// and the two luma 8x8 lists (Intra Y, Inter Y).
fn iq_matrix(sps: &Sps, pps: &Pps) -> VAIQMatrixBufferH264 {
    let lists = picture_scaling_matrix(sps, pps);
    let mut m = VAIQMatrixBufferH264::flat();
    m.scaling_list_4x4 = lists.raster_4x4();
    let raster_8x8 = lists.raster_8x8();
    m.scaling_list_8x8 = [raster_8x8[0], raster_8x8[1]];
    m
}

fn slice_parameters(
    h: &SliceHeader,
    pps: &Pps,
    coded_len: usize,
    data_bit_offset: u32,
) -> VASliceParameterBufferH264 {
    let mut sp = VASliceParameterBufferH264 {
        slice_data_size: coded_len as u32,
        slice_data_offset: 0,
        slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
        slice_data_bit_offset: data_bit_offset as u16,
        first_mb_in_slice: h.first_mb_in_slice as u16,
        slice_type: h.slice_type.code(),
        direct_spatial_mv_pred_flag: u8::from(h.direct_spatial_mv_pred_flag),
        num_ref_idx_l0_active_minus1: if h.slice_type.has_list0() {
            h.num_ref_idx_l0_active_minus1 as u8
        } else {
            0
        },
        num_ref_idx_l1_active_minus1: if h.slice_type.has_list1() {
            h.num_ref_idx_l1_active_minus1 as u8
        } else {
            0
        },
        cabac_init_idc: h.cabac_init_idc as u8,
        slice_qp_delta: h.slice_qp_delta as i8,
        disable_deblocking_filter_idc: h.disable_deblocking_filter_idc as u8,
        slice_alpha_c0_offset_div2: h.slice_alpha_c0_offset_div2 as i8,
        slice_beta_offset_div2: h.slice_beta_offset_div2 as i8,
        ref_pic_list0: [VAPictureH264::invalid(); 32],
        ref_pic_list1: [VAPictureH264::invalid(); 32],
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
    };
    // §7.4.3.2: explicit weights; entries without them take the defaults.
    let explicit = match h.slice_type {
        SliceType::P | SliceType::SP => pps.weighted_pred_flag,
        SliceType::B => pps.weighted_bipred_idc == 1,
        _ => false,
    };
    if let (true, Some(w)) = (explicit, h.pred_weight_table.as_ref()) {
        sp.luma_log2_weight_denom = w.luma_log2_weight_denom as u8;
        sp.chroma_log2_weight_denom = w.chroma_log2_weight_denom as u8;
        let mut default = PredWeight::default();
        default.luma_weight = 1 << w.luma_log2_weight_denom;
        default.chroma_weight = [1 << w.chroma_log2_weight_denom; 2];
        let lists = [(&w.l0, true), (&w.l1, h.slice_type.has_list1())];
        for (k, (list, used)) in lists.into_iter().enumerate() {
            if !used {
                continue;
            }
            let (lw, lo, cw, co, lf, cf) = if k == 0 {
                (
                    &mut sp.luma_weight_l0,
                    &mut sp.luma_offset_l0,
                    &mut sp.chroma_weight_l0,
                    &mut sp.chroma_offset_l0,
                    &mut sp.luma_weight_l0_flag,
                    &mut sp.chroma_weight_l0_flag,
                )
            } else {
                (
                    &mut sp.luma_weight_l1,
                    &mut sp.luma_offset_l1,
                    &mut sp.chroma_weight_l1,
                    &mut sp.chroma_offset_l1,
                    &mut sp.luma_weight_l1_flag,
                    &mut sp.chroma_weight_l1_flag,
                )
            };
            *lf = 1;
            *cf = 1;
            for i in 0..32 {
                // Entries without explicit weights carry the §7.4.3.2 defaults.
                let e = list.get(i).copied().unwrap_or(default);
                lw[i] = e.luma_weight as i16;
                lo[i] = e.luma_offset as i16;
                for j in 0..2 {
                    cw[i][j] = e.chroma_weight[j] as i16;
                    co[i][j] = e.chroma_offset[j] as i16;
                }
            }
        }
    }
    sp
}
