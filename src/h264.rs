//! H.264 decode through VA-API.
//!
//! NAL units, parameter sets and slice headers come from oxideav-h264's
//! parser ([`oxideav_h264::decoder::Decoder`]); picture order counts
//! (§8.2.1), reference list initialisation and modification (§8.2.4) and
//! reference marking (§8.2.5) from its `poc` and `ref_list` modules. This
//! module finds picture boundaries (§7.4.1.2.4), keeps the DPB with a
//! surface for every reference picture, and fills
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

use oxideav_h264::decoder::{Decoder as Parser, Event};
use oxideav_h264::poc::{PocResult, PocSlice, PocSps, PocState, derive_poc};
use oxideav_h264::pps::Pps;
use oxideav_h264::ref_list::{self, DpbEntry, MmcoOp as RefMmco, PicStructure, RefMarking, RplmOp};
use oxideav_h264::slice_header::{
    MmcoOp as SliceMmco, RefPicListModificationOp, SliceHeader, SliceType,
};
use oxideav_h264::sps::Sps;
use oxideav_h264::transform::{select_scaling_list_4x4, select_scaling_list_8x8};

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
    poc: PocResult,
    slices: Vec<Slice>,
}

/// See the module documentation.
pub struct H264Decoder {
    display: Option<Display>,
    session: Option<DecodeSession>,
    requirements: Option<H264Requirements>,
    parser: Parser,
    poc_state: PocState,
    dpb: Vec<DpbEntry>,
    /// Surface slot of each DPB entry, by `dpb_key`.
    slots: BTreeMap<u32, usize>,
    next_key: u32,
    prev_ref_frame_num: Option<u32>,
    prev_had_mmco5: bool,
    prev_reference_top_foc: i32,
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
            poc_state: PocState::default(),
            dpb: Vec::new(),
            slots: BTreeMap::new(),
            next_key: 0,
            prev_ref_frame_num: None,
            prev_had_mmco5: false,
            prev_reference_top_foc: 0,
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
        let constrained = sps.constraint_set_flags & 0b10 != 0;
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
        let Event::Slice {
            nal_unit_type,
            nal_ref_idc,
            header,
            slice_data_cursor,
            pps,
            sps,
            ..
        } = event
        else {
            return Ok(());
        };
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
        let new_picture = match self.pending.as_ref() {
            None => true,
            Some(p) => first_slice_of_new_picture(p, nal_unit_type, nal_ref_idc, &header, &sps),
        };
        if new_picture {
            self.finish_picture()?;
            self.begin_picture(nal_unit_type, nal_ref_idc, &header, sps, pps)?;
        }
        let data_bit_offset = 8 + slice_data_cursor.0 as u32 * 8 + u32::from(slice_data_cursor.1);
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
        let poc_sps = PocSps {
            pic_order_cnt_type: sps.pic_order_cnt_type,
            log2_max_frame_num_minus4: sps.log2_max_frame_num_minus4,
            log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
            delta_pic_order_always_zero_flag: sps.delta_pic_order_always_zero_flag,
            offset_for_non_ref_pic: sps.offset_for_non_ref_pic,
            offset_for_top_to_bottom_field: sps.offset_for_top_to_bottom_field,
            num_ref_frames_in_pic_order_cnt_cycle: sps.num_ref_frames_in_pic_order_cnt_cycle,
            offset_for_ref_frame: sps.offset_for_ref_frame.clone(),
            frame_mbs_only_flag: sps.frame_mbs_only_flag,
        };
        let poc_slice = PocSlice {
            is_reference: nal_ref_idc != 0,
            is_idr: idr,
            frame_num: header.frame_num,
            field_pic_flag: false,
            bottom_field_flag: false,
            pic_order_cnt_lsb: header.pic_order_cnt_lsb,
            delta_pic_order_cnt_bottom: header.delta_pic_order_cnt_bottom,
            delta_pic_order_cnt: header.delta_pic_order_cnt,
            prev_had_mmco5: self.prev_had_mmco5,
            prev_reference_top_foc_for_mmco5: self.prev_reference_top_foc,
        };
        let poc = derive_poc(&poc_sps, &poc_slice, &mut self.poc_state)
            .map_err(|e| DecodeError::Bitstream(format!("H.264 picture order count: {e:?}")))?;
        self.pending = Some(InProgress {
            sps,
            pps,
            nal_unit_type,
            nal_ref_idc,
            poc,
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
        let sps = &p.sps;
        let pps = &p.pps;
        let first = &p.slices[0].header;
        let idr = p.nal_unit_type == 5;
        let reference = p.nal_ref_idc != 0;
        let max_frame_num = sps.max_frame_num();

        let session = self.session.as_mut().expect("session ensured");
        let target = session.acquire().ok_or_else(|| {
            DecodeError::Bitstream("more pictures held than the DPB allows".into())
        })?;

        // ReferenceFrames[]: every picture marked for reference.
        let picture_of = |e: &DpbEntry, session: &DecodeSession, slots: &BTreeMap<u32, usize>| {
            let slot = *slots.get(&e.dpb_key)?;
            let long = e.marking == RefMarking::LongTerm;
            Some(VAPictureH264 {
                picture_id: session.surface(slot),
                frame_idx: if long {
                    e.long_term_frame_idx
                } else {
                    e.frame_num
                },
                flags: if long {
                    VA_PICTURE_H264_LONG_TERM_REFERENCE
                } else {
                    VA_PICTURE_H264_SHORT_TERM_REFERENCE
                },
                top_field_order_cnt: e.top_field_order_cnt,
                bottom_field_order_cnt: e.bottom_field_order_cnt,
                va_reserved: [0; 4],
            })
        };
        let mut pic = picture_parameters(sps, pps, first, reference);
        pic.curr_pic = VAPictureH264 {
            picture_id: session.surface(target),
            frame_idx: first.frame_num,
            flags: 0,
            top_field_order_cnt: p.poc.top_field_order_cnt,
            bottom_field_order_cnt: p.poc.bottom_field_order_cnt,
            va_reserved: [0; 4],
        };
        let mut n = 0;
        for e in self.dpb.iter().filter(|e| e.marking != RefMarking::Unused) {
            if let Some(v) = picture_of(e, session, &self.slots)
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
            if h.slice_type.has_list_0() {
                let (mut l0, mut l1) = match h.slice_type {
                    SliceType::B => ref_list::init_ref_pic_lists_b(
                        &self.dpb,
                        p.poc.pic_order_cnt,
                        PicStructure::Frame,
                        false,
                    ),
                    _ => (
                        ref_list::init_ref_pic_list_p(
                            &self.dpb,
                            h.frame_num,
                            max_frame_num,
                            PicStructure::Frame,
                            false,
                        ),
                        Vec::new(),
                    ),
                };
                if l0.is_empty() {
                    session.release(target);
                    return Err(DecodeError::Bitstream(
                        "inter slice without reference pictures".into(),
                    ));
                }
                let ops = |m: &[RefPicListModificationOp]| -> Vec<RplmOp> {
                    m.iter()
                        .map(|op| match *op {
                            RefPicListModificationOp::Subtract(v) => RplmOp::Subtract(v),
                            RefPicListModificationOp::Add(v) => RplmOp::Add(v),
                            RefPicListModificationOp::LongTerm(v) => RplmOp::LongTerm(v),
                        })
                        .collect()
                };
                ref_list::modify_ref_pic_list(
                    &mut l0,
                    &ops(&h.ref_pic_list_modification.modifications_l0),
                    &self.dpb,
                    h.num_ref_idx_l0_active_minus1 + 1,
                    h.frame_num,
                    max_frame_num,
                    false,
                    false,
                );
                if h.slice_type.has_list_1() {
                    ref_list::modify_ref_pic_list(
                        &mut l1,
                        &ops(&h.ref_pic_list_modification.modifications_l1),
                        &self.dpb,
                        h.num_ref_idx_l1_active_minus1 + 1,
                        h.frame_num,
                        max_frame_num,
                        false,
                        false,
                    );
                }
                for (list, out) in [(&l0, &mut sp.ref_pic_list0), (&l1, &mut sp.ref_pic_list1)] {
                    for (i, key) in list.iter().enumerate().take(32) {
                        let entry = self.dpb.iter().find(|e| e.dpb_key == *key);
                        match entry.and_then(|e| picture_of(e, session, &self.slots)) {
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
        let mut current = DpbEntry {
            frame_num: first.frame_num,
            top_field_order_cnt: p.poc.top_field_order_cnt,
            bottom_field_order_cnt: p.poc.bottom_field_order_cnt,
            pic_order_cnt: p.poc.pic_order_cnt,
            structure: PicStructure::Frame,
            marking: if reference {
                RefMarking::ShortTerm
            } else {
                RefMarking::Unused
            },
            long_term_frame_idx: 0,
            dpb_key: self.next_key,
            field_markings: [RefMarking::Unused; 2],
        };
        self.next_key = self.next_key.wrapping_add(1);
        current.sync_field_markings();
        let mut mmco5 = false;
        let mut poc = p.poc.pic_order_cnt;
        if reference {
            let marking = first.dec_ref_pic_marking.as_ref();
            let adaptive: Option<Vec<RefMmco>> = marking
                .and_then(|m| m.adaptive_marking.as_ref())
                .map(|ops| {
                    ops.iter()
                        .map(|op| match *op {
                            SliceMmco::MarkShortTermUnused(v) => RefMmco::MarkShortTermUnused(v),
                            SliceMmco::MarkLongTermUnused(v) => RefMmco::MarkLongTermUnused(v),
                            SliceMmco::AssignLongTerm(d, i) => RefMmco::AssignLongTerm(d, i),
                            SliceMmco::SetMaxLongTermIdx(v) => RefMmco::SetMaxLongTermIdx(v),
                            SliceMmco::MarkAllUnused => RefMmco::MarkAllUnused,
                            SliceMmco::AssignCurrentLongTerm(v) => {
                                RefMmco::AssignCurrentLongTerm(v)
                            }
                        })
                        .collect()
                });
            mmco5 = ref_list::perform_marking(
                &mut self.dpb,
                &mut current,
                sps.max_num_ref_frames,
                idr,
                marking.is_some_and(|m| m.long_term_reference_flag),
                marking.is_some_and(|m| m.no_output_of_prior_pics_flag),
                adaptive.as_deref(),
                first.frame_num,
                max_frame_num,
            );
            if mmco5 {
                // §8.2.1: after MMCO 5 the picture's counts are relative to itself.
                let temp = current.pic_order_cnt;
                current.frame_num = 0;
                current.top_field_order_cnt -= temp;
                current.bottom_field_order_cnt -= temp;
                current.pic_order_cnt = current
                    .top_field_order_cnt
                    .min(current.bottom_field_order_cnt);
                poc = current.pic_order_cnt;
            }
            self.prev_had_mmco5 = mmco5;
            self.prev_reference_top_foc = if mmco5 {
                p.poc.top_field_order_cnt - p.poc.pic_order_cnt
            } else {
                0
            };
            self.prev_ref_frame_num = Some(if mmco5 { 0 } else { first.frame_num });
        }
        // Entries no longer used for reference give their surfaces back.
        let (kept, dropped): (Vec<DpbEntry>, Vec<DpbEntry>) = std::mem::take(&mut self.dpb)
            .into_iter()
            .partition(DpbEntry::is_any_field_ref);
        self.dpb = kept;
        let session = self.session.as_mut().expect("session ensured");
        for e in dropped {
            if let Some(slot) = self.slots.remove(&e.dpb_key) {
                session.release(slot);
            }
        }
        if reference {
            self.slots.insert(current.dpb_key, target);
            self.dpb.push(current);
        } else {
            self.returned_only.push(target);
        }

        let (crop_w, crop_h) = sps.cropped_dimensions();
        let m = sps.frame_crop_margins();
        self.out.push(H264Picture {
            slot: target,
            poc,
            crop: (m.left, m.top, crop_w, crop_h),
            bit_depth: 8,
            idr,
            starts_sequence: idr || mmco5,
        });
        Ok(())
    }
}

/// §7.4.1.2.4: whether a slice begins a new primary coded picture.
fn first_slice_of_new_picture(
    p: &InProgress,
    nal_unit_type: u8,
    nal_ref_idc: u8,
    h: &SliceHeader,
    sps: &Sps,
) -> bool {
    let prev = &p.slices.last().expect("picture has a slice").header;
    let first = &p.slices[0].header;
    prev.frame_num != h.frame_num
        || first.pic_parameter_set_id != h.pic_parameter_set_id
        || prev.field_pic_flag != h.field_pic_flag
        || prev.bottom_field_flag != h.bottom_field_flag
        || (p.nal_ref_idc == 0) != (nal_ref_idc == 0)
        || (sps.pic_order_cnt_type == 0
            && (prev.pic_order_cnt_lsb != h.pic_order_cnt_lsb
                || prev.delta_pic_order_cnt_bottom != h.delta_pic_order_cnt_bottom))
        || (sps.pic_order_cnt_type == 1 && prev.delta_pic_order_cnt != h.delta_pic_order_cnt)
        || (p.nal_unit_type == 5) != (nal_unit_type == 5)
        || (nal_unit_type == 5 && prev.idr_pic_id != h.idr_pic_id)
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
    let transform_8x8 = pps
        .extension
        .as_ref()
        .is_some_and(|e| e.transform_8x8_mode_flag);
    let second_chroma = pps
        .extension
        .as_ref()
        .map_or(pps.chroma_qp_index_offset, |e| {
            e.second_chroma_qp_index_offset
        });
    use seq_fields::*;
    let seq = (sps.chroma_format_idc & 3) << CHROMA_FORMAT_IDC
        | bit(sps.gaps_in_frame_num_value_allowed_flag, GAPS_IN_FRAME_NUM_VALUE_ALLOWED)
        | bit(sps.frame_mbs_only_flag, FRAME_MBS_ONLY)
        | bit(sps.mb_adaptive_frame_field_flag, MB_ADAPTIVE_FRAME_FIELD)
        | bit(sps.direct_8x8_inference_flag, DIRECT_8X8_INFERENCE)
        // Table A-1 / A.3.3: MinLumaBiPredSize 8x8 from level 3.1 in Main and High.
        | bit(sps.level_idc >= 31 && sps.profile_idc != 66, MIN_LUMA_BI_PRED_SIZE_8X8)
        | (sps.log2_max_frame_num_minus4 & 15) << LOG2_MAX_FRAME_NUM_MINUS4
        | (sps.pic_order_cnt_type & 3) << PIC_ORDER_CNT_TYPE
        | (sps.log2_max_pic_order_cnt_lsb_minus4 & 15) << LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4
        | bit(sps.delta_pic_order_always_zero_flag, DELTA_PIC_ORDER_ALWAYS_ZERO);
    use pic_fields::*;
    let fields = bit(pps.entropy_coding_mode_flag, ENTROPY_CODING_MODE)
        | bit(pps.weighted_pred_flag, WEIGHTED_PRED)
        | (pps.weighted_bipred_idc & 3) << WEIGHTED_BIPRED_IDC
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
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8 as u8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8 as u8,
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

/// The effective scaling lists (PPS over SPS, fall-back rules A and B)
/// in the order the bitstream codes them (zig-zag scan), which is what
/// drivers read from `VAIQMatrixBufferH264`.
fn iq_matrix(sps: &Sps, pps: &Pps) -> VAIQMatrixBufferH264 {
    let mut m = VAIQMatrixBufferH264::flat();
    for (i, row) in m.scaling_list_4x4.iter_mut().enumerate() {
        for (d, s) in row.iter_mut().zip(select_scaling_list_4x4(i, sps, pps)) {
            *d = s as u8;
        }
    }
    for (i, row) in m.scaling_list_8x8.iter_mut().enumerate() {
        for (d, s) in row.iter_mut().zip(select_scaling_list_8x8(i, sps, pps)) {
            *d = s as u8;
        }
    }
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
        slice_type: (h.slice_type_raw % 5) as u8,
        direct_spatial_mv_pred_flag: u8::from(h.direct_spatial_mv_pred_flag),
        num_ref_idx_l0_active_minus1: if h.slice_type.has_list_0() {
            h.num_ref_idx_l0_active_minus1 as u8
        } else {
            0
        },
        num_ref_idx_l1_active_minus1: if h.slice_type.has_list_1() {
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
        let luma_default = 1i32 << w.luma_log2_weight_denom;
        let chroma_default = 1i32 << w.chroma_log2_weight_denom;
        let lists = [
            (&w.luma_weights_l0, &w.chroma_weights_l0, true),
            (
                &w.luma_weights_l1,
                &w.chroma_weights_l1,
                h.slice_type.has_list_1(),
            ),
        ];
        for (k, (luma, chroma, used)) in lists.into_iter().enumerate() {
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
                let (wl, ol) = luma.get(i).copied().flatten().unwrap_or((luma_default, 0));
                lw[i] = wl as i16;
                lo[i] = ol as i16;
                let c = chroma
                    .get(i)
                    .copied()
                    .flatten()
                    .unwrap_or([(chroma_default, 0); 2]);
                for j in 0..2 {
                    cw[i][j] = c[j].0 as i16;
                    co[i][j] = c[j].1 as i16;
                }
            }
        }
    }
    sp
}
