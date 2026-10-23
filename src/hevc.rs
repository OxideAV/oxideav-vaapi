//! HEVC (H.265) decode through VA-API.
//!
//! Parameter sets, slice segment headers, picture order counts and the
//! reference picture set / DPB marking come from oxideav-h265's own
//! parser and §8.3 driver ([`oxideav_h265::PictureSequenceState`]); this
//! module translates them into `VAPictureParameterBufferHEVC`,
//! `VAIQMatrixBufferHEVC` and per-segment `VASliceParameterBufferHEVC`
//! plus the coded slice data, and keeps a surface for every picture the
//! DPB still references.
//!
//! Scope: Main and Main 10 (4:2:0, 8 or 10 bits, no range / screen
//! content / multi-layer extensions), base layer only. Anything else is
//! [`DecodeError::Unsupported`] before any submission, so a caller can
//! hand the stream to a software decoder.
//!
//! Pictures come back in decode order, one per coded picture, as soon as
//! the hardware has finished them; output ordering is the caller's.

use std::collections::BTreeMap;

use oxideav_h265::dpb::{Marking, RefPicListParams};
use oxideav_h265::hvcc::nal_unit_from_coded;
use oxideav_h265::motion::MotionField;
use oxideav_h265::{
    LongTermEntry, MaterializedShortTermRefPicSet, NalKind, PicParameterSet, Picture,
    PictureHeaderInfo, PictureSequenceState, SeqParameterSet, ShortTermRefPicSet,
    SliceLongTermRefPicSource, SliceRefParams, SliceSegmentHeader, SliceType,
};

use crate::display::Display;
use crate::session::{Buffer, DecodeError, DecodeSession, Planes, SurfaceFormat};
use crate::sys::hevc::*;
use crate::sys::{buffer_type, profile, VA_SLICE_DATA_FLAG_ALL};

const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;
const NAL_EOS: u8 = 36;

/// A decoded picture. Its surface stays valid until the next call into
/// the decoder.
#[derive(Clone, Debug)]
pub struct HevcPicture {
    /// Surface slot in the decoder's session.
    pub slot: usize,
    /// `PicOrderCntVal`.
    pub poc: i32,
    /// `pic_output_flag` (false: decoded for reference only).
    pub output: bool,
    /// The conformance-window rectangle: left, top, width, height.
    pub crop: (u32, u32, u32, u32),
    /// Luma bit depth (8 or 10).
    pub bit_depth: u8,
    /// `true` for an IRAP picture.
    pub irap: bool,
    /// `true` when this picture starts a coded video sequence (an IRAP
    /// with NoRaslOutputFlag): output order restarts here.
    pub starts_sequence: bool,
}

struct Segment {
    nal_type: u8,
    temporal_id: u8,
    /// The whole coded NAL unit, header included.
    coded: Vec<u8>,
    header: SliceSegmentHeader,
    /// Header bytes after the NAL unit header, emulation prevention removed.
    header_bytes: usize,
    /// Emulation prevention bytes inside those header bytes.
    header_emulation: u16,
    /// Bits of the slice's own `st_ref_pic_set()`.
    st_rps_bits: u32,
}

/// What a stream needs from the driver (see [`HevcDecoder::requirements`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HevcRequirements {
    pub va_profile: i32,
    pub format: SurfaceFormat,
    pub coded_width: u32,
    pub coded_height: u32,
}

/// See the module documentation.
pub struct HevcDecoder {
    display: Option<Display>,
    session: Option<DecodeSession>,
    requirements: Option<HevcRequirements>,
    sps: BTreeMap<u8, SeqParameterSet>,
    pps: BTreeMap<u8, PicParameterSet>,
    state: PictureSequenceState,
    /// Surface slot of each DPB entry, index for index; `None` once the
    /// entry is unused for reference.
    dpb_slots: Vec<Option<usize>>,
    /// Slot of the last picture returned that the DPB does not hold.
    returned_only: Option<usize>,
    pending: Vec<Segment>,
    seen_picture: bool,
    /// Skipping the RASL pictures of an IRAP with NoRaslOutputFlag.
    skip_rasl: bool,
    out: Vec<HevcPicture>,
}

impl HevcDecoder {
    pub fn new(display: Display) -> Self {
        HevcDecoder {
            display: Some(display),
            session: None,
            requirements: None,
            sps: BTreeMap::new(),
            pps: BTreeMap::new(),
            state: PictureSequenceState::new(),
            dpb_slots: Vec::new(),
            returned_only: None,
            pending: Vec::new(),
            seen_picture: false,
            skip_rasl: false,
            out: Vec::new(),
        }
    }

    /// The VA profile, surface format and coded size a sequence needs,
    /// or why this bridge cannot decode it.
    pub fn requirements(sps: &SeqParameterSet) -> Result<HevcRequirements, DecodeError> {
        let profile_idc = sps.ptl.general_profile_idc;
        if sps.chroma_format_idc != 1 || sps.separate_colour_plane_flag {
            return Err(DecodeError::Unsupported(format!(
                "chroma_format_idc {}",
                sps.chroma_format_idc
            )));
        }
        if sps.nuh_layer_id != 0 {
            return Err(DecodeError::Unsupported("multi-layer SPS".into()));
        }
        if sps.sps_range_extension.is_some() || sps.sps_scc_extension.is_some() {
            return Err(DecodeError::Unsupported("SPS range / SCC extension".into()));
        }
        let (va_profile, format) = match (sps.bit_depth_luma_minus8, sps.bit_depth_chroma_minus8) {
            (0, 0) if matches!(profile_idc, 1 | 2 | 3) || profile_idc == 0 => {
                (profile::VAProfileHEVCMain, SurfaceFormat::Nv12)
            }
            (0..=2, 0..=2) if matches!(profile_idc, 2) || profile_idc == 0 => {
                (profile::VAProfileHEVCMain10, SurfaceFormat::P010)
            }
            (l, c) => {
                return Err(DecodeError::Unsupported(format!(
                    "profile_idc {profile_idc} at {} / {} bits",
                    l + 8,
                    c + 8
                )))
            }
        };
        Ok(HevcRequirements {
            va_profile,
            format,
            coded_width: sps.pic_width_in_luma_samples,
            coded_height: sps.pic_height_in_luma_samples,
        })
    }

    /// Whether the driver behind `display` has what `sps` needs, without
    /// creating anything.
    pub fn supports(display: &Display, sps: &SeqParameterSet) -> Result<(), DecodeError> {
        let r = Self::requirements(sps)?;
        let max = crate::config::get_attribute(
            display.raw(),
            r.va_profile,
            crate::sys::entrypoint::VAEntrypointVLD,
            crate::sys::attrib::VAConfigAttribMaxPictureWidth,
        );
        if let Ok(Some(w)) = max {
            if r.coded_width > w {
                return Err(DecodeError::Unsupported(format!("width {} > {w}", r.coded_width)));
            }
        }
        let max = crate::config::get_attribute(
            display.raw(),
            r.va_profile,
            crate::sys::entrypoint::VAEntrypointVLD,
            crate::sys::attrib::VAConfigAttribMaxPictureHeight,
        );
        if let Ok(Some(h)) = max {
            if r.coded_height > h {
                return Err(DecodeError::Unsupported(format!("height {} > {h}", r.coded_height)));
            }
        }
        if !display.is_supported(crate::display::VaProfile(r.va_profile), crate::sys::entrypoint::VAEntrypointVLD) {
            return Err(DecodeError::Unsupported(format!("no VLD for VA profile {}", r.va_profile)));
        }
        Ok(())
    }

    /// Parse an SPS NAL unit (whole, header included).
    pub fn parse_sps(coded: &[u8]) -> Result<SeqParameterSet, DecodeError> {
        let nal = nal_unit_from_coded(coded).map_err(|e| DecodeError::Bitstream(format!("{e:?}")))?;
        if nal.header.nal_unit_type != NAL_SPS {
            return Err(DecodeError::Bitstream("not an SPS".into()));
        }
        SeqParameterSet::parse(&nal.rbsp).map_err(|e| DecodeError::Bitstream(format!("SPS: {e:?}")))
    }

    /// Create the decode session for the sequence parameter set seen
    /// last, before any picture, so a caller learns now whether the
    /// driver takes the stream.
    pub fn prepare(&mut self) -> Result<(), DecodeError> {
        let sps = self
            .sps
            .values()
            .next_back()
            .cloned()
            .ok_or_else(|| DecodeError::Bitstream("no sequence parameter set".into()))?;
        self.ensure_session(&sps, true)
    }

    pub fn session(&self) -> Option<&DecodeSession> {
        self.session.as_ref()
    }

    /// Decode coded NAL units (each whole, NAL unit header included, no
    /// start code or length prefix). Every picture they complete is
    /// returned; a picture is complete when the next one starts, when a
    /// parameter set or end of sequence follows it, or at [`Self::flush`].
    pub fn decode<'a>(
        &mut self,
        nals: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Vec<HevcPicture>, DecodeError> {
        self.out.clear();
        if let (Some(slot), Some(s)) = (self.returned_only.take(), self.session.as_mut()) {
            s.release(slot);
        }
        for coded in nals {
            self.push(coded)?;
        }
        Ok(std::mem::take(&mut self.out))
    }

    /// Finish the pending picture (end of an access unit or of the stream).
    pub fn flush(&mut self) -> Result<Vec<HevcPicture>, DecodeError> {
        self.out.clear();
        self.finish_picture()?;
        Ok(std::mem::take(&mut self.out))
    }

    /// Copy a returned picture's conformance window into `planes`.
    pub fn read(&mut self, picture: &HevcPicture, planes: &mut Planes<'_>) -> Result<(), DecodeError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| DecodeError::Bitstream("no session".into()))?;
        let (x, y, w, h) = picture.crop;
        session.read(picture.slot, x, y, w, h, planes)
    }

    fn push(&mut self, coded: &[u8]) -> Result<(), DecodeError> {
        if coded.len() < 2 {
            return Err(DecodeError::Bitstream("NAL unit shorter than its header".into()));
        }
        let nal = nal_unit_from_coded(coded).map_err(|e| DecodeError::Bitstream(format!("{e:?}")))?;
        let h = nal.header;
        if h.nuh_layer_id != 0 {
            return Ok(());
        }
        if h.is_vcl() {
            if h.nal_unit_type > 21 {
                return Ok(()); // reserved VCL types
            }
            let first = nal.rbsp.first().is_some_and(|b| b & 0x80 != 0);
            if first {
                self.finish_picture()?;
            }
            let pps_id = peek_pps_id(&nal.rbsp, h.nal_unit_type)?;
            let pps = self
                .pps
                .get(&pps_id)
                .ok_or_else(|| DecodeError::Bitstream(format!("slice refers to missing PPS {pps_id}")))?;
            let sps = self
                .sps
                .get(&pps.sps_id)
                .ok_or_else(|| DecodeError::Bitstream(format!("PPS refers to missing SPS {}", pps.sps_id)))?;
            let header = SliceSegmentHeader::parse(&nal.rbsp, h.nal_unit_type, sps, pps)
                .map_err(|e| DecodeError::Bitstream(format!("slice header: {e:?}")))?;
            let header_bytes = header
                .byte_offset_to_slice_data
                .ok_or_else(|| DecodeError::Bitstream("slice header not parsed to its end".into()))?;
            let header_emulation = emulation_bytes(&nal.escaped, header_bytes);
            let st_rps_bits = if header.short_term_ref_pic_set_sps_flag == Some(false) {
                header
                    .inline_short_term_ref_pic_set
                    .as_ref()
                    .map_or(0, |s| st_rps_bits(s, sps.num_short_term_ref_pic_sets))
            } else {
                0
            };
            self.pending.push(Segment {
                nal_type: h.nal_unit_type,
                temporal_id: h.temporal_id,
                coded: coded.to_vec(),
                header,
                header_bytes,
                header_emulation,
                st_rps_bits,
            });
            return Ok(());
        }
        match h.nal_unit_type {
            NAL_VPS => self.finish_picture()?,
            NAL_SPS => {
                self.finish_picture()?;
                let sps = SeqParameterSet::parse(&nal.rbsp)
                    .map_err(|e| DecodeError::Bitstream(format!("SPS: {e:?}")))?;
                self.sps.insert(sps.sps_id, sps);
            }
            NAL_PPS => {
                self.finish_picture()?;
                let pps = PicParameterSet::parse(&nal.rbsp)
                    .map_err(|e| DecodeError::Bitstream(format!("PPS: {e:?}")))?;
                self.pps.insert(pps.pps_id, pps);
            }
            NAL_EOS => {
                self.finish_picture()?;
                // §8.1.3: the picture after an end of sequence has NoRaslOutputFlag.
                self.seen_picture = false;
            }
            _ => {}
        }
        Ok(())
    }

    fn finish_picture(&mut self) -> Result<(), DecodeError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let segs = std::mem::take(&mut self.pending);
        self.decode_picture(&segs)
    }

    /// Make sure the session matches `sps`; a new size, format or
    /// profile starts over (only at an IRAP with NoRaslOutputFlag).
    fn ensure_session(&mut self, sps: &SeqParameterSet, irap_restart: bool) -> Result<(), DecodeError> {
        let r = Self::requirements(sps)?;
        let dpb = sps.sub_layer_ordering_info[usize::from(sps.max_sub_layers_minus1)
            .min(sps.sub_layer_ordering_info.len() - 1)]
        .max_dec_pic_buffering_minus1 as usize
            + 1;
        let count = (dpb + 2).min(18);
        if self.requirements == Some(r) && self.session.as_ref().is_some_and(|s| s.surface_count() >= count) {
            return Ok(());
        }
        if self.session.is_some() && !irap_restart {
            return Err(DecodeError::Bitstream("sequence change without an IRAP picture".into()));
        }
        let display = match self.session.take() {
            Some(s) => s.into_display(),
            None => self.display.take().ok_or_else(|| DecodeError::Bitstream("decoder lost its display".into()))?,
        };
        self.state = PictureSequenceState::new();
        self.dpb_slots.clear();
        self.returned_only = None;
        self.requirements = None;
        match DecodeSession::new(display, r.va_profile, r.format, r.coded_width, r.coded_height, count) {
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

    fn decode_picture(&mut self, segs: &[Segment]) -> Result<(), DecodeError> {
        let indep = &segs[0];
        if indep.header.dependent_slice_segment_flag {
            return Err(DecodeError::Bitstream("picture starts with a dependent slice segment".into()));
        }
        let pps = self
            .pps
            .get(&indep.header.slice_pic_parameter_set_id)
            .cloned()
            .ok_or_else(|| DecodeError::Bitstream("missing PPS".into()))?;
        let sps = self
            .sps
            .get(&pps.sps_id)
            .cloned()
            .ok_or_else(|| DecodeError::Bitstream("missing SPS".into()))?;
        if pps.pps_range_extension.is_some() || pps.pps_scc_extension.is_some() || pps.pps_multilayer_extension.is_some() {
            return Err(DecodeError::Unsupported("PPS range / SCC / multi-layer extension".into()));
        }
        let nal_kind = NalKind::new(indep.nal_type);
        let no_rasl_output = nal_kind.is_idr() || nal_kind.is_bla() || (nal_kind.is_irap() && !self.seen_picture);
        if nal_kind.is_irap() {
            self.skip_rasl = no_rasl_output;
        }
        if nal_kind.is_rasl() && self.skip_rasl {
            // §8.1.3: not decoded, not output; references before the IRAP are gone.
            return Ok(());
        }
        if !nal_kind.is_irap() && self.session.is_none() {
            return Err(DecodeError::Bitstream("stream does not start with an IRAP picture".into()));
        }
        self.ensure_session(&sps, nal_kind.is_irap() && no_rasl_output)?;
        self.seen_picture = true;

        // §C.5.2.2: entries the previous picture left unused leave the DPB.
        let entries = self.state.dpb().entries();
        debug_assert_eq!(entries.len(), self.dpb_slots.len());
        let mut kept = Vec::with_capacity(self.dpb_slots.len());
        for (e, slot) in entries.iter().zip(self.dpb_slots.drain(..)) {
            if e.marking != Marking::Unused {
                kept.push(slot);
            } else if let (Some(slot), Some(s)) = (slot, self.session.as_mut()) {
                s.release(slot);
            }
        }
        self.dpb_slots = kept;
        self.state.dpb_mut().evict_unused();

        let slice_type = indep
            .header
            .slice_type
            .ok_or_else(|| DecodeError::Bitstream("independent slice without a type".into()))?;
        let info = header_info(indep, &sps, nal_kind, no_rasl_output)?;
        let first_ref = slice_ref_params(&indep.header, &pps, slice_type, &info);
        let ref_state = self.state.begin_picture(&info, &first_ref);
        let rps = ref_state.rps.clone();
        for list in [&rps.st_curr_before, &rps.st_curr_after, &rps.lt_curr] {
            if list.iter().any(Option::is_none) {
                return Err(DecodeError::Bitstream("reference picture missing from the DPB".into()));
            }
        }
        // Entries the RPS just marked unused are not referenced by this
        // picture or any later one: their surfaces go back now.
        for (e, slot) in self.state.dpb().entries().iter().zip(self.dpb_slots.iter_mut()) {
            if e.marking == Marking::Unused {
                if let (Some(s), Some(session)) = (slot.take(), self.session.as_mut()) {
                    session.release(s);
                }
            }
        }

        let session = self.session.as_mut().expect("session ensured");
        let target = session
            .acquire()
            .ok_or_else(|| DecodeError::Bitstream("more pictures held than the DPB allows".into()))?;
        let poc = ref_state.poc.val;

        // ReferenceFrames[]: every picture still marked for reference.
        let mut pic = picture_parameters(&sps, &pps, segs, nal_kind)?;
        pic.curr_pic = VAPictureHEVC {
            picture_id: session.surface(target),
            pic_order_cnt: poc,
            flags: 0,
            va_reserved: [0; 4],
        };
        let mut frame_of_entry = vec![0xFFu8; self.state.dpb().len()];
        let mut n = 0usize;
        for (i, e) in self.state.dpb().entries().iter().enumerate() {
            if e.marking == Marking::Unused {
                continue;
            }
            let Some(slot) = self.dpb_slots[i] else { continue };
            if n == 15 {
                session.release(target);
                return Err(DecodeError::Bitstream("more than 15 reference pictures".into()));
            }
            let mut flags = 0;
            if e.marking == Marking::LongTerm {
                flags |= VA_PICTURE_HEVC_LONG_TERM_REFERENCE;
            }
            if rps.st_curr_before.contains(&Some(i)) {
                flags |= VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
            } else if rps.st_curr_after.contains(&Some(i)) {
                flags |= VA_PICTURE_HEVC_RPS_ST_CURR_AFTER;
            } else if rps.lt_curr.contains(&Some(i)) {
                flags |= VA_PICTURE_HEVC_RPS_LT_CURR;
            }
            pic.reference_frames[n] = VAPictureHEVC {
                picture_id: session.surface(slot),
                pic_order_cnt: e.poc,
                flags,
                va_reserved: [0; 4],
            };
            frame_of_entry[i] = n as u8;
            n += 1;
        }

        let iq = iq_matrix(&sps, &pps);
        let mut slices = Vec::with_capacity(segs.len());
        let mut last_indep: Option<VASliceParameterBufferHEVC> = None;
        for (k, seg) in segs.iter().enumerate() {
            let mut sp = if seg.header.dependent_slice_segment_flag {
                let mut base = last_indep
                    .ok_or_else(|| DecodeError::Bitstream("dependent slice segment first".into()))?;
                base.long_slice_flags |= 1 << long_slice_flags::DEPENDENT_SLICE_SEGMENT;
                base.long_slice_flags &= !(1 << long_slice_flags::LAST_SLICE_OF_PIC);
                base.num_entry_point_offsets = 0;
                base
            } else {
                let st = seg
                    .header
                    .slice_type
                    .ok_or_else(|| DecodeError::Bitstream("independent slice without a type".into()))?;
                let refs = slice_ref_params(&seg.header, &pps, st, &info);
                let lists = refs.is_inter.then(|| {
                    self.state.dpb().build_ref_pic_lists(
                        &rps,
                        &RefPicListParams {
                            num_ref_idx_l0_active_minus1: refs.num_ref_idx_l0_active_minus1,
                            num_ref_idx_l1_active_minus1: refs.num_ref_idx_l1_active_minus1,
                            num_pic_total_curr: refs.num_pic_total_curr,
                            is_b: refs.is_b,
                            list_entry_l0: refs.list_entry_l0.as_deref(),
                            list_entry_l1: refs.list_entry_l1.as_deref(),
                            curr_pic_ref_enabled: false,
                            inter_layer0: &[],
                            inter_layer1: &[],
                        },
                    )
                });
                let sp = slice_parameters(&seg.header, &pps, st, lists.as_ref(), &frame_of_entry)?;
                last_indep = Some(sp);
                sp
            };
            sp.slice_data_size = seg.coded.len() as u32;
            sp.slice_data_offset = 0;
            sp.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
            sp.slice_data_byte_offset = (2 + seg.header_bytes) as u32;
            sp.slice_data_num_emu_prevn_bytes = seg.header_emulation;
            sp.slice_segment_address = seg.header.slice_segment_address;
            sp.num_entry_point_offsets = seg
                .header
                .entry_point_offsets
                .as_ref()
                .map_or(0, |e| e.num_entry_point_offsets as u16);
            if k + 1 == segs.len() {
                sp.long_slice_flags |= 1 << long_slice_flags::LAST_SLICE_OF_PIC;
            }
            slices.push(sp);
        }

        let mut buffers = vec![Buffer::of(buffer_type::VAPictureParameterBufferType, &pic)];
        if let Some(iq) = iq.as_ref() {
            buffers.push(Buffer::of(buffer_type::VAIQMatrixBufferType, iq));
        }
        for (sp, seg) in slices.iter().zip(segs) {
            buffers.push(Buffer::of(buffer_type::VASliceParameterBufferType, sp));
            buffers.push(Buffer::bytes(buffer_type::VASliceDataBufferType, &seg.coded));
        }
        let submitted = session.submit(target, &buffers).and_then(|()| session.sync(target));
        if let Err(e) = submitted {
            session.release(target);
            return Err(e);
        }

        // The picture joins the DPB as a short-term reference unless the
        // DPB holds one picture only (§C.5.2.2: it is emptied first).
        let referenceable = sps.sub_layer_ordering_info
            [usize::from(sps.max_sub_layers_minus1).min(sps.sub_layer_ordering_info.len() - 1)]
        .max_dec_pic_buffering_minus1
            > 0;
        if referenceable {
            self.state
                .store_picture(ref_state.poc, 0, Picture::new(8, 8, 1, 8, 8), MotionField::new(8, 8));
            self.dpb_slots.push(Some(target));
        } else {
            self.returned_only = Some(target);
        }
        let cw = &sps.conformance_window;
        let (sub_w, sub_h) = (2u32, 2u32);
        let left = sub_w * cw.left_offset;
        let top = sub_h * cw.top_offset;
        let width = sps.pic_width_in_luma_samples - left - sub_w * cw.right_offset;
        let height = sps.pic_height_in_luma_samples - top - sub_h * cw.bottom_offset;
        self.out.push(HevcPicture {
            slot: target,
            poc,
            output: indep.header.pic_output_flag,
            crop: (left, top, width, height),
            bit_depth: sps.bit_depth_luma_minus8 + 8,
            irap: nal_kind.is_irap(),
            starts_sequence: nal_kind.is_irap() && no_rasl_output,
        });
        Ok(())
    }
}

/// `slice_pic_parameter_set_id` without parsing the whole header.
fn peek_pps_id(rbsp: &[u8], nal_unit_type: u8) -> Result<u8, DecodeError> {
    let mut r = oxideav_h265::BitReader::new(rbsp);
    let bad = |_| DecodeError::Bitstream("truncated slice header".into());
    r.u1().map_err(bad)?;
    if (16..=23).contains(&nal_unit_type) {
        r.u1().map_err(bad)?;
    }
    let id = r.ue().map_err(bad)?;
    if id > 63 {
        return Err(DecodeError::Bitstream("slice_pic_parameter_set_id > 63".into()));
    }
    Ok(id as u8)
}

/// Emulation prevention bytes among the coded bytes that hold the first
/// `rbsp_len` RBSP bytes of a payload.
fn emulation_bytes(escaped: &[u8], rbsp_len: usize) -> u16 {
    let (mut zeros, mut produced, mut removed) = (0usize, 0usize, 0u16);
    for &b in escaped {
        if produced >= rbsp_len {
            break;
        }
        if zeros >= 2 && b == 3 {
            removed += 1;
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        produced += 1;
    }
    removed
}

fn ue_bits(v: u32) -> u32 {
    let v = u64::from(v) + 1;
    2 * (63 - v.leading_zeros()) + 1
}

/// Bits taken by the slice header's own `st_ref_pic_set(num_short_term_ref_pic_sets)`.
fn st_rps_bits(s: &ShortTermRefPicSet, num_sets: u32) -> u32 {
    let mut bits = 0;
    if num_sets != 0 {
        bits += 1; // inter_ref_pic_set_prediction_flag
    }
    if s.inter_ref_pic_set_prediction_flag {
        bits += ue_bits(s.delta_idx_minus1) + 1 + ue_bits(s.abs_delta_rps_minus1);
        for &used in &s.used_by_curr_pic_flag {
            bits += if used { 1 } else { 2 };
        }
    } else {
        bits += ue_bits(s.num_negative_pics) + ue_bits(s.num_positive_pics);
        bits += s.delta_poc_s0_minus1.iter().map(|&d| ue_bits(d) + 1).sum::<u32>();
        bits += s.delta_poc_s1_minus1.iter().map(|&d| ue_bits(d) + 1).sum::<u32>();
    }
    bits
}

fn materialize_rps(header: &SliceSegmentHeader, sps: &SeqParameterSet) -> Result<MaterializedShortTermRefPicSet, DecodeError> {
    let bad = |m: &str| DecodeError::Bitstream(m.into());
    let Some(sps_flag) = header.short_term_ref_pic_set_sps_flag else {
        return Ok(MaterializedShortTermRefPicSet {
            delta_poc_s0: Vec::new(),
            used_by_curr_pic_s0: Vec::new(),
            delta_poc_s1: Vec::new(),
            used_by_curr_pic_s1: Vec::new(),
        });
    };
    let mut chain: Vec<MaterializedShortTermRefPicSet> = Vec::with_capacity(sps.short_term_ref_pic_sets.len());
    for (idx, set) in sps.short_term_ref_pic_sets.iter().enumerate() {
        let source = if set.inter_ref_pic_set_prediction_flag {
            let r = idx.checked_sub(set.delta_idx_minus1 as usize + 1).ok_or_else(|| bad("RefRpsIdx underflow"))?;
            Some(&chain[r])
        } else {
            None
        };
        chain.push(set.materialize(source).map_err(|e| DecodeError::Bitstream(format!("{e:?}")))?);
    }
    if sps_flag {
        let idx = header.short_term_ref_pic_set_idx.unwrap_or(0) as usize;
        chain.into_iter().nth(idx).ok_or_else(|| bad("short_term_ref_pic_set_idx out of range"))
    } else {
        let set = header.inline_short_term_ref_pic_set.as_ref().ok_or_else(|| bad("missing st_ref_pic_set"))?;
        let source = if set.inter_ref_pic_set_prediction_flag {
            let r = chain.len().checked_sub(set.delta_idx_minus1 as usize + 1).ok_or_else(|| bad("RefRpsIdx underflow"))?;
            Some(&chain[r])
        } else {
            None
        };
        set.materialize(source).map_err(|e| DecodeError::Bitstream(format!("{e:?}")))
    }
}

fn header_info(seg: &Segment, sps: &SeqParameterSet, nal_kind: NalKind, no_rasl_output: bool) -> Result<PictureHeaderInfo, DecodeError> {
    let max_poc_lsb = 1u32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
    let short_term_rps = materialize_rps(&seg.header, sps)?;
    let mut long_term = Vec::new();
    let mut acc: u32 = 0;
    let num_lt_sps = seg.header.num_long_term_sps.unwrap_or(0) as usize;
    for (i, lt) in seg.header.long_term_ref_pics.iter().enumerate() {
        // §7.4.7.1 eq. 7-52
        acc = if i == 0 || i == num_lt_sps {
            lt.delta_poc_msb_cycle_lt
        } else {
            acc.checked_add(lt.delta_poc_msb_cycle_lt)
                .ok_or_else(|| DecodeError::Bitstream("DeltaPocMsbCycleLt overflow".into()))?
        };
        let poc_lsb_lt = match lt.source {
            SliceLongTermRefPicSource::Sps { lt_idx_sps } => {
                sps.long_term_ref_pics
                    .get(lt_idx_sps as usize)
                    .ok_or_else(|| DecodeError::Bitstream("lt_idx_sps out of range".into()))?
                    .poc_lsb
            }
            SliceLongTermRefPicSource::InSlice { poc_lsb_lt, .. } => poc_lsb_lt,
        };
        let used = lt
            .used_by_curr_pic_lt(sps)
            .ok_or_else(|| DecodeError::Bitstream("lt_idx_sps out of range".into()))?;
        long_term.push(LongTermEntry {
            poc_lsb_lt,
            used_by_curr_pic_lt: used,
            delta_poc_msb_present: lt.delta_poc_msb_present_flag,
            delta_poc_msb_cycle_lt: acc,
        });
    }
    Ok(PictureHeaderInfo {
        nal_kind,
        temporal_id: seg.temporal_id,
        layer_id: 0,
        no_rasl_output,
        poc_lsb: seg.header.slice_pic_order_cnt_lsb.unwrap_or(0),
        max_poc_lsb,
        short_term_rps,
        long_term,
    })
}

fn slice_ref_params(header: &SliceSegmentHeader, pps: &PicParameterSet, slice_type: SliceType, info: &PictureHeaderInfo) -> SliceRefParams {
    let st = info
        .short_term_rps
        .used_by_curr_pic_s0
        .iter()
        .chain(info.short_term_rps.used_by_curr_pic_s1.iter())
        .filter(|&&u| u)
        .count();
    let lt = info.long_term.iter().filter(|e| e.used_by_curr_pic_lt).count();
    SliceRefParams {
        is_inter: slice_type != SliceType::I,
        is_b: slice_type == SliceType::B,
        num_ref_idx_l0_active_minus1: u32::from(
            header.num_ref_idx_l0_active_minus1.unwrap_or(pps.num_ref_idx_l0_default_active_minus1),
        ),
        num_ref_idx_l1_active_minus1: u32::from(
            header.num_ref_idx_l1_active_minus1.unwrap_or(pps.num_ref_idx_l1_default_active_minus1),
        ),
        num_pic_total_curr: (st + lt) as u32,
        temporal_mvp_enabled: header.slice_temporal_mvp_enabled_flag,
        collocated_from_l0_flag: header.collocated_from_l0_flag.unwrap_or(true),
        collocated_ref_idx: header.collocated_ref_idx.unwrap_or(0),
        curr_pic_ref_enabled: false,
        list_entry_l0: header
            .ref_pic_lists_modification
            .as_ref()
            .filter(|m| m.ref_pic_list_modification_flag_l0)
            .map(|m| m.list_entry_l0.clone()),
        list_entry_l1: header
            .ref_pic_lists_modification
            .as_ref()
            .filter(|m| m.ref_pic_list_modification_flag_l1 == Some(true))
            .map(|m| m.list_entry_l1.clone()),
    }
}

/// Tile column widths / row heights in CTBs (§6.5.1), all but the last.
fn tile_sizes(count_minus1: u32, uniform: bool, explicit: &[u32], pic_in_ctbs: u32) -> Vec<u16> {
    let n = count_minus1 + 1;
    (0..count_minus1)
        .map(|i| {
            if uniform {
                (((i + 1) * pic_in_ctbs) / n - (i * pic_in_ctbs) / n - 1) as u16
            } else {
                explicit.get(i as usize).copied().unwrap_or(0) as u16
            }
        })
        .collect()
}

fn bit(flag: bool, at: u32) -> u32 {
    u32::from(flag) << at
}

fn picture_parameters(
    sps: &SeqParameterSet,
    pps: &PicParameterSet,
    segs: &[Segment],
    nal_kind: NalKind,
) -> Result<VAPictureParameterBufferHEVC, DecodeError> {
    let sub = &sps.sub_layer_ordering_info
        [usize::from(sps.max_sub_layers_minus1).min(sps.sub_layer_ordering_info.len() - 1)];
    let pcm = sps.pcm;
    let intra = segs
        .iter()
        .all(|s| s.header.dependent_slice_segment_flag || s.header.slice_type == Some(SliceType::I));
    let no_b = segs.iter().all(|s| s.header.slice_type != Some(SliceType::B));
    use pic_fields as pf;
    use slice_parsing_fields as sf;
    let pic_fields = u32::from(sps.chroma_format_idc) << pf::CHROMA_FORMAT_IDC
        | bit(sps.separate_colour_plane_flag, pf::SEPARATE_COLOUR_PLANE)
        | bit(sps.pcm_enabled_flag, pf::PCM_ENABLED)
        | bit(sps.scaling_list_enabled_flag, pf::SCALING_LIST_ENABLED)
        | bit(pps.transform_skip_enabled_flag, pf::TRANSFORM_SKIP_ENABLED)
        | bit(sps.amp_enabled_flag, pf::AMP_ENABLED)
        | bit(sps.strong_intra_smoothing_enabled_flag, pf::STRONG_INTRA_SMOOTHING_ENABLED)
        | bit(pps.sign_data_hiding_enabled_flag, pf::SIGN_DATA_HIDING_ENABLED)
        | bit(pps.constrained_intra_pred_flag, pf::CONSTRAINED_INTRA_PRED)
        | bit(pps.cu_qp_delta_enabled_flag, pf::CU_QP_DELTA_ENABLED)
        | bit(pps.weighted_pred_flag, pf::WEIGHTED_PRED)
        | bit(pps.weighted_bipred_flag, pf::WEIGHTED_BIPRED)
        | bit(pps.transquant_bypass_enabled_flag, pf::TRANSQUANT_BYPASS_ENABLED)
        | bit(pps.tiles_enabled_flag, pf::TILES_ENABLED)
        | bit(pps.entropy_coding_sync_enabled_flag, pf::ENTROPY_CODING_SYNC_ENABLED)
        | bit(pps.pps_loop_filter_across_slices_enabled_flag, pf::PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED)
        | bit(pps.loop_filter_across_tiles_enabled_flag, pf::LOOP_FILTER_ACROSS_TILES_ENABLED)
        | bit(pcm.is_some_and(|p| p.loop_filter_disabled_flag), pf::PCM_LOOP_FILTER_DISABLED)
        | bit(sub.max_num_reorder_pics == 0, pf::NO_PIC_REORDERING)
        | bit(no_b, pf::NO_BI_PRED);
    let slice_parsing_fields = bit(pps.lists_modification_present_flag, sf::LISTS_MODIFICATION_PRESENT)
        | bit(sps.long_term_ref_pics_present_flag, sf::LONG_TERM_REF_PICS_PRESENT)
        | bit(sps.sps_temporal_mvp_enabled_flag, sf::SPS_TEMPORAL_MVP_ENABLED)
        | bit(pps.cabac_init_present_flag, sf::CABAC_INIT_PRESENT)
        | bit(pps.output_flag_present_flag, sf::OUTPUT_FLAG_PRESENT)
        | bit(pps.dependent_slice_segments_enabled_flag, sf::DEPENDENT_SLICE_SEGMENTS_ENABLED)
        | bit(pps.pps_slice_chroma_qp_offsets_present_flag, sf::PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT)
        | bit(sps.sample_adaptive_offset_enabled_flag, sf::SAMPLE_ADAPTIVE_OFFSET_ENABLED)
        | bit(pps.deblocking.override_enabled_flag, sf::DEBLOCKING_FILTER_OVERRIDE_ENABLED)
        | bit(pps.deblocking.disabled_flag, sf::PPS_DISABLE_DEBLOCKING_FILTER)
        | bit(pps.slice_segment_header_extension_present_flag, sf::SLICE_SEGMENT_HEADER_EXTENSION_PRESENT)
        | bit(nal_kind.is_irap(), sf::RAP_PIC)
        | bit(nal_kind.is_idr(), sf::IDR_PIC)
        | bit(intra, sf::INTRA_PIC);
    let ctb = 1u32 << (sps.log2_min_luma_coding_block_size_minus3 + 3 + sps.log2_diff_max_min_luma_coding_block_size);
    let w_ctbs = sps.pic_width_in_luma_samples.div_ceil(ctb);
    let h_ctbs = sps.pic_height_in_luma_samples.div_ceil(ctb);
    let (cols, rows) = if pps.tiles_enabled_flag {
        (pps.tiles.num_tile_columns_minus1, pps.tiles.num_tile_rows_minus1)
    } else {
        (0, 0)
    };
    if cols > 19 || rows > 21 {
        return Err(DecodeError::Bitstream("too many tiles".into()));
    }
    let mut column_width_minus1 = [0u16; 19];
    let mut row_height_minus1 = [0u16; 21];
    for (i, v) in tile_sizes(cols, pps.tiles.uniform_spacing_flag, &pps.tiles.column_width_minus1, w_ctbs).into_iter().enumerate() {
        column_width_minus1[i] = v;
    }
    for (i, v) in tile_sizes(rows, pps.tiles.uniform_spacing_flag, &pps.tiles.row_height_minus1, h_ctbs).into_iter().enumerate() {
        row_height_minus1[i] = v;
    }
    Ok(VAPictureParameterBufferHEVC {
        curr_pic: VAPictureHEVC::invalid(),
        reference_frames: [VAPictureHEVC::invalid(); 15],
        pic_width_in_luma_samples: sps.pic_width_in_luma_samples as u16,
        pic_height_in_luma_samples: sps.pic_height_in_luma_samples as u16,
        pic_fields,
        sps_max_dec_pic_buffering_minus1: sub.max_dec_pic_buffering_minus1 as u8,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        pcm_sample_bit_depth_luma_minus1: pcm.map_or(0, |p| p.bit_depth_luma_minus1),
        pcm_sample_bit_depth_chroma_minus1: pcm.map_or(0, |p| p.bit_depth_chroma_minus1),
        log2_min_luma_coding_block_size_minus3: sps.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: sps.log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2: sps.log2_min_luma_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size: sps.log2_diff_max_min_luma_transform_block_size,
        log2_min_pcm_luma_coding_block_size_minus3: pcm.map_or(0, |p| p.log2_min_pcm_luma_coding_block_size_minus3),
        log2_diff_max_min_pcm_luma_coding_block_size: pcm.map_or(0, |p| p.log2_diff_max_min_pcm_luma_coding_block_size),
        max_transform_hierarchy_depth_intra: sps.max_transform_hierarchy_depth_intra,
        max_transform_hierarchy_depth_inter: sps.max_transform_hierarchy_depth_inter,
        init_qp_minus26: pps.init_qp_minus26 as i8,
        diff_cu_qp_delta_depth: pps.diff_cu_qp_delta_depth as u8,
        pps_cb_qp_offset: pps.pps_cb_qp_offset,
        pps_cr_qp_offset: pps.pps_cr_qp_offset,
        log2_parallel_merge_level_minus2: pps.log2_parallel_merge_level_minus2 as u8,
        num_tile_columns_minus1: cols as u8,
        num_tile_rows_minus1: rows as u8,
        column_width_minus1,
        row_height_minus1,
        slice_parsing_fields,
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
        num_short_term_ref_pic_sets: sps.num_short_term_ref_pic_sets as u8,
        num_long_term_ref_pic_sps: sps.num_long_term_ref_pics_sps as u8,
        num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        pps_beta_offset_div2: pps.deblocking.beta_offset_div2,
        pps_tc_offset_div2: pps.deblocking.tc_offset_div2,
        num_extra_slice_header_bits: pps.num_extra_slice_header_bits,
        st_rps_bits: segs[0].st_rps_bits,
        va_reserved: [0; 8],
    })
}

/// §6.5.3 up-right diagonal scan: (x, y) of each scan position.
fn diagonal_scan(n: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(n * n);
    let (mut x, mut y) = (0i32, 0i32);
    while out.len() < n * n {
        while y >= 0 {
            if (x as usize) < n && (y as usize) < n {
                out.push((x as usize, y as usize));
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
    }
    out
}

/// The scaling lists in effect, coefficients in raster order.
fn iq_matrix(sps: &SeqParameterSet, pps: &PicParameterSet) -> Option<VAIQMatrixBufferHEVC> {
    if !sps.scaling_list_enabled_flag {
        return None;
    }
    let default;
    let data = if pps.pps_scaling_list_data_present_flag {
        pps.scaling_list_data.as_ref()?
    } else if sps.sps_scaling_list_data_present_flag {
        sps.scaling_list_data.as_ref()?
    } else {
        default = oxideav_h265::scaling_list::ScalingListData::all_default();
        &default
    };
    let scan4 = diagonal_scan(4);
    let scan8 = diagonal_scan(8);
    let raster = |coef: &[u16], scan: &[(usize, usize)], n: usize, out: &mut [u8]| {
        for (i, &(x, y)) in scan.iter().enumerate() {
            out[y * n + x] = coef.get(i).copied().unwrap_or(16) as u8;
        }
    };
    let mut m = VAIQMatrixBufferHEVC {
        scaling_list_4x4: [[0; 16]; 6],
        scaling_list_8x8: [[0; 64]; 6],
        scaling_list_16x16: [[0; 64]; 6],
        scaling_list_32x32: [[0; 64]; 2],
        scaling_list_dc_16x16: [0; 6],
        scaling_list_dc_32x32: [0; 2],
        va_reserved: [0; 4],
    };
    for id in 0..6 {
        raster(&data.lists[0][id].coef, &scan4, 4, &mut m.scaling_list_4x4[id]);
        raster(&data.lists[1][id].coef, &scan8, 8, &mut m.scaling_list_8x8[id]);
        raster(&data.lists[2][id].coef, &scan8, 8, &mut m.scaling_list_16x16[id]);
        m.scaling_list_dc_16x16[id] = data.lists[2][id].dc_coef as u8;
    }
    for (k, id) in [0usize, 3].into_iter().enumerate() {
        raster(&data.lists[3][id].coef, &scan8, 8, &mut m.scaling_list_32x32[k]);
        m.scaling_list_dc_32x32[k] = data.lists[3][id].dc_coef as u8;
    }
    Some(m)
}

fn slice_parameters(
    h: &SliceSegmentHeader,
    pps: &PicParameterSet,
    slice_type: SliceType,
    lists: Option<&oxideav_h265::dpb::RefPicLists>,
    frame_of_entry: &[u8],
) -> Result<VASliceParameterBufferHEVC, DecodeError> {
    use long_slice_flags as lf;
    let mut ref_pic_list = [[0xFFu8; 15]; 2];
    if let Some(lists) = lists {
        let fill = |dst: &mut [u8; 15], src: &[Option<usize>]| -> Result<(), DecodeError> {
            for (i, e) in src.iter().take(15).enumerate() {
                dst[i] = match e {
                    Some(idx) => *frame_of_entry
                        .get(*idx)
                        .filter(|&&f| f != 0xFF)
                        .ok_or_else(|| DecodeError::Bitstream("reference list names a picture outside the DPB".into()))?,
                    None => return Err(DecodeError::Bitstream("reference list entry missing".into())),
                };
            }
            Ok(())
        };
        fill(&mut ref_pic_list[0], &lists.list0)?;
        if let Some(l1) = &lists.list1 {
            fill(&mut ref_pic_list[1], l1)?;
        }
    }
    let type_code = match slice_type {
        SliceType::B => 0u32,
        SliceType::P => 1,
        SliceType::I => 2,
    };
    let deblocking_disabled = h.deblocking.as_ref().map_or(pps.deblocking.disabled_flag, |d| d.disabled_flag);
    let (beta, tc) = h
        .deblocking
        .as_ref()
        .map_or((pps.deblocking.beta_offset_div2, pps.deblocking.tc_offset_div2), |d| {
            (d.beta_offset_div2, d.tc_offset_div2)
        });
    let across = h
        .slice_loop_filter_across_slices_enabled_flag
        .unwrap_or(pps.pps_loop_filter_across_slices_enabled_flag);
    let flags = type_code << lf::SLICE_TYPE
        | u32::from(h.colour_plane_id.unwrap_or(0)) << lf::COLOR_PLANE_ID
        | bit(h.slice_sao_luma_flag, lf::SLICE_SAO_LUMA)
        | bit(h.slice_sao_chroma_flag, lf::SLICE_SAO_CHROMA)
        | bit(h.mvd_l1_zero_flag.unwrap_or(false), lf::MVD_L1_ZERO)
        | bit(h.cabac_init_flag.unwrap_or(false), lf::CABAC_INIT)
        | bit(h.slice_temporal_mvp_enabled_flag, lf::SLICE_TEMPORAL_MVP_ENABLED)
        | bit(deblocking_disabled, lf::SLICE_DEBLOCKING_FILTER_DISABLED)
        | bit(h.collocated_from_l0_flag.unwrap_or(true), lf::COLLOCATED_FROM_L0)
        | bit(across, lf::SLICE_LOOP_FILTER_ACROSS_SLICES_ENABLED);
    let inter = slice_type != SliceType::I;
    let mut sp = VASliceParameterBufferHEVC {
        slice_data_size: 0,
        slice_data_offset: 0,
        slice_data_flag: 0,
        slice_data_byte_offset: 0,
        slice_segment_address: 0,
        ref_pic_list,
        long_slice_flags: flags,
        collocated_ref_idx: if h.slice_temporal_mvp_enabled_flag && inter {
            h.collocated_ref_idx.unwrap_or(0) as u8
        } else {
            0xFF
        },
        num_ref_idx_l0_active_minus1: if inter {
            h.num_ref_idx_l0_active_minus1.unwrap_or(pps.num_ref_idx_l0_default_active_minus1)
        } else {
            0
        },
        num_ref_idx_l1_active_minus1: if slice_type == SliceType::B {
            h.num_ref_idx_l1_active_minus1.unwrap_or(pps.num_ref_idx_l1_default_active_minus1)
        } else {
            0
        },
        slice_qp_delta: h.slice_qp_delta.unwrap_or(0) as i8,
        slice_cb_qp_offset: h.slice_cb_qp_offset,
        slice_cr_qp_offset: h.slice_cr_qp_offset,
        slice_beta_offset_div2: beta,
        slice_tc_offset_div2: tc,
        luma_log2_weight_denom: 0,
        delta_chroma_log2_weight_denom: 0,
        delta_luma_weight_l0: [0; 15],
        luma_offset_l0: [0; 15],
        delta_chroma_weight_l0: [[0; 2]; 15],
        chroma_offset_l0: [[0; 2]; 15],
        delta_luma_weight_l1: [0; 15],
        luma_offset_l1: [0; 15],
        delta_chroma_weight_l1: [[0; 2]; 15],
        chroma_offset_l1: [[0; 2]; 15],
        five_minus_max_num_merge_cand: h.five_minus_max_num_merge_cand.unwrap_or(0) as u8,
        num_entry_point_offsets: 0,
        entry_offset_to_subset_array: 0,
        slice_data_num_emu_prevn_bytes: 0,
        va_reserved: [0; 2],
    };
    if let Some(pwt) = &h.pred_weight_table {
        let luma_denom = u32::from(pwt.luma_log2_weight_denom);
        let chroma_denom = (luma_denom as i32 + pwt.delta_chroma_log2_weight_denom).clamp(0, 7) as u32;
        sp.luma_log2_weight_denom = pwt.luma_log2_weight_denom;
        sp.delta_chroma_log2_weight_denom = pwt.delta_chroma_log2_weight_denom as i8;
        // §7.4.7.3, eq. 7-56 (high_precision_offsets_enabled_flag is 0 here).
        let half = 1i32 << 7;
        let fill = |entries: &[oxideav_h265::PredWeightEntry],
                    dlw: &mut [i8; 15],
                    lo: &mut [i8; 15],
                    dcw: &mut [[i8; 2]; 15],
                    co: &mut [[i8; 2]; 15]| {
            for (i, e) in entries.iter().take(15).enumerate() {
                if e.luma_weight_flag {
                    dlw[i] = e.delta_luma_weight as i8;
                    lo[i] = e.luma_offset as i8;
                }
                if e.chroma_weight_flag {
                    for j in 0..2 {
                        dcw[i][j] = e.delta_chroma_weight[j] as i8;
                        let weight = (1i32 << chroma_denom) + e.delta_chroma_weight[j];
                        let offset = (half - ((half * weight) >> chroma_denom) + e.delta_chroma_offset[j]).clamp(-half, half - 1);
                        co[i][j] = offset as i8;
                    }
                }
            }
        };
        fill(&pwt.entries_l0, &mut sp.delta_luma_weight_l0, &mut sp.luma_offset_l0, &mut sp.delta_chroma_weight_l0, &mut sp.chroma_offset_l0);
        fill(&pwt.entries_l1, &mut sp.delta_luma_weight_l1, &mut sp.luma_offset_l1, &mut sp.delta_chroma_weight_l1, &mut sp.chroma_offset_l1);
    }
    Ok(sp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagonal_scan_4x4_matches_table_6_5() {
        let s = diagonal_scan(4);
        assert_eq!(&s[..6], &[(0, 0), (0, 1), (1, 0), (0, 2), (1, 1), (2, 0)]);
        assert_eq!(s[15], (3, 3));
    }

    #[test]
    fn emulation_prevention_counted_inside_the_header_only() {
        // 00 00 03 01 | 00 00 03 02: one EP byte in the first 3 RBSP bytes.
        let escaped = [0, 0, 3, 1, 0, 0, 3, 2];
        assert_eq!(emulation_bytes(&escaped, 3), 1);
        assert_eq!(emulation_bytes(&escaped, 6), 2);
    }

    #[test]
    fn uniform_tile_columns() {
        // 4 columns over 30 CTBs: 7, 8, 7, (8).
        assert_eq!(tile_sizes(3, true, &[], 30), vec![6, 7, 6]);
    }

    #[test]
    fn inline_rps_bits() {
        let s = ShortTermRefPicSet {
            num_negative_pics: 1,
            delta_poc_s0_minus1: vec![0],
            used_by_curr_pic_s0_flag: vec![true],
            ..Default::default()
        };
        // flag(1) + ue(1)=3 + ue(0)=1 + ue(0)=1 + used(1)
        assert_eq!(st_rps_bits(&s, 4), 7);
    }
}
