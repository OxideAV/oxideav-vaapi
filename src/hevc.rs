//! HEVC (H.265) decode through VA-API.
//!
//! Parameter sets and complete slice segment headers come from
//! oxideav-bitstream's stream front end
//! ([`oxideav_bitstream::hevc::stream::HevcParser`]); picture order counts
//! and the reference picture set / DPB marking from its [`HevcDpb`]
//! (§8.3). This module translates them into
//! `VAPictureParameterBufferHEVC`, `VAIQMatrixBufferHEVC` and per-segment
//! `VASliceParameterBufferHEVC` plus the coded slice data, and keeps a
//! surface for every picture the DPB still references.
//!
//! Scope: Main and Main 10 (4:2:0, 8 or 10 bits, no range / screen
//! content / multi-layer extensions), base layer only. Anything else is
//! [`DecodeError::Unsupported`] before any submission, so a caller can
//! hand the stream to a software decoder.
//!
//! Pictures come back in decode order, one per coded picture, as soon as
//! the hardware has finished them; output ordering is the caller's.

use std::collections::BTreeMap;

use oxideav_bitstream::BitstreamError;
use oxideav_bitstream::hevc::dpb::{HevcDpb, HevcMarking, HevcRefPicLists};
use oxideav_bitstream::hevc::scaling::HevcScalingLists;
use oxideav_bitstream::hevc::slice::{
    HevcPredWeight, HevcSliceType as SliceType, SliceSegmentHeader, tile_sizes_in_ctbs,
};
use oxideav_bitstream::hevc::stream::{HevcEvent, HevcParser};
use oxideav_bitstream::hevc::{HevcPps as PicParameterSet, HevcSps as SeqParameterSet};

use crate::display::Display;
use crate::session::{Buffer, DecodeError, DecodeSession, Planes, SurfaceFormat};
use crate::sys::hevc::*;
use crate::sys::{VA_SLICE_DATA_FLAG_ALL, buffer_type, profile};

const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;
const NAL_EOS: u8 = 36;

/// A decoded picture. Its surface stays valid until the next call into
/// the decoder.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
    /// The whole coded NAL unit, header included.
    coded: Vec<u8>,
    header: SliceSegmentHeader,
}

/// What a stream needs from the driver (see [`HevcDecoder::requirements`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
    parser: HevcParser,
    dpb: HevcDpb,
    /// Surface slot of each picture the DPB holds, by key.
    slots: BTreeMap<u32, usize>,
    /// Slot of the last picture returned that the DPB does not hold.
    returned_only: Option<usize>,
    pending: Vec<Segment>,
    seen_picture: bool,
    /// Skipping the RASL pictures of an IRAP with NoRaslOutputFlag.
    skip_rasl: bool,
    out: Vec<HevcPicture>,
}

fn bitstream(what: &str) -> impl Fn(BitstreamError) -> DecodeError + '_ {
    move |e| DecodeError::Bitstream(format!("{what}: {e:?}"))
}

impl HevcDecoder {
    pub fn new(display: Display) -> Self {
        HevcDecoder {
            display: Some(display),
            session: None,
            requirements: None,
            parser: HevcParser::new(),
            dpb: HevcDpb::new(),
            slots: BTreeMap::new(),
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
        let profile_idc = sps.profile_tier_level.general_profile_idc;
        if sps.chroma_format_idc != 1 || sps.separate_colour_plane_flag {
            return Err(DecodeError::Unsupported(format!(
                "chroma_format_idc {}",
                sps.chroma_format_idc
            )));
        }
        if sps.sps_multilayer_extension_flag || sps.sps_3d_extension_flag {
            return Err(DecodeError::Unsupported("multi-layer SPS".into()));
        }
        if sps.sps_range_extension_flag || sps.sps_scc_extension_flag {
            return Err(DecodeError::Unsupported("SPS range / SCC extension".into()));
        }
        let (va_profile, format) = match (sps.bit_depth_luma_minus8, sps.bit_depth_chroma_minus8) {
            (0, 0) if matches!(profile_idc, 1..=3) || profile_idc == 0 => {
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
                )));
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
        if let Ok(Some(w)) = max
            && r.coded_width > w
        {
            return Err(DecodeError::Unsupported(format!(
                "width {} > {w}",
                r.coded_width
            )));
        }
        let max = crate::config::get_attribute(
            display.raw(),
            r.va_profile,
            crate::sys::entrypoint::VAEntrypointVLD,
            crate::sys::attrib::VAConfigAttribMaxPictureHeight,
        );
        if let Ok(Some(h)) = max
            && r.coded_height > h
        {
            return Err(DecodeError::Unsupported(format!(
                "height {} > {h}",
                r.coded_height
            )));
        }
        if !display.is_supported(
            crate::display::VaProfile(r.va_profile),
            crate::sys::entrypoint::VAEntrypointVLD,
        ) {
            return Err(DecodeError::Unsupported(format!(
                "no VLD for VA profile {}",
                r.va_profile
            )));
        }
        Ok(())
    }

    /// Parse an SPS NAL unit (whole, header included).
    pub fn parse_sps(coded: &[u8]) -> Result<SeqParameterSet, DecodeError> {
        if coded.len() < 2 || (coded[0] >> 1) & 0x3F != NAL_SPS {
            return Err(DecodeError::Bitstream("not an SPS".into()));
        }
        oxideav_bitstream::hevc::parse_sps_nal(coded).map_err(bitstream("SPS"))
    }

    /// Create the decode session for the sequence parameter set seen
    /// last, before any picture, so a caller learns now whether the
    /// driver takes the stream.
    pub fn prepare(&mut self) -> Result<(), DecodeError> {
        let sps = (0..16)
            .rev()
            .find_map(|id| self.parser.sps(id).cloned())
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
    pub fn read(
        &mut self,
        picture: &HevcPicture,
        planes: &mut Planes<'_>,
    ) -> Result<(), DecodeError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| DecodeError::Bitstream("no session".into()))?;
        let (x, y, w, h) = picture.crop;
        session.read(picture.slot, x, y, w, h, planes)
    }

    fn push(&mut self, coded: &[u8]) -> Result<(), DecodeError> {
        if coded.len() < 2 {
            return Err(DecodeError::Bitstream(
                "NAL unit shorter than its header".into(),
            ));
        }
        let nal_type = (coded[0] >> 1) & 0x3F;
        let layer = (u16::from(coded[0] & 1) << 5) | u16::from(coded[1] >> 3);
        if layer != 0 {
            return Ok(());
        }
        // The picture in progress ends before the parser takes in a unit
        // that starts the next one (its parameter sets stay in force).
        let starts_picture = match nal_type {
            0..=21 => coded.get(2).is_some_and(|b| b & 0x80 != 0),
            NAL_VPS | NAL_SPS | NAL_PPS | NAL_EOS => true,
            _ => false,
        };
        if starts_picture {
            self.finish_picture()?;
        }
        match self
            .parser
            .process_nal(coded)
            .map_err(bitstream("NAL unit"))?
        {
            HevcEvent::Slice(slice) => self.pending.push(Segment {
                coded: coded.to_vec(),
                header: slice.header,
            }),
            HevcEvent::EndOfSequence => {
                // §8.1.3: the picture after an end of sequence has NoRaslOutputFlag.
                self.seen_picture = false;
                self.dpb.end_of_sequence();
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
    fn ensure_session(
        &mut self,
        sps: &SeqParameterSet,
        irap_restart: bool,
    ) -> Result<(), DecodeError> {
        let r = Self::requirements(sps)?;
        let dpb = sps.sps_max_dec_pic_buffering_minus1 as usize + 1;
        let count = (dpb + 2).min(18);
        if self.requirements == Some(r)
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.surface_count() >= count)
        {
            return Ok(());
        }
        if self.session.is_some() && !irap_restart {
            return Err(DecodeError::Bitstream(
                "sequence change without an IRAP picture".into(),
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
        self.returned_only = None;
        self.requirements = None;
        match DecodeSession::new(
            display,
            r.va_profile,
            r.format,
            r.coded_width,
            r.coded_height,
            count,
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

    fn decode_picture(&mut self, segs: &[Segment]) -> Result<(), DecodeError> {
        let indep = &segs[0].header;
        if indep.dependent_slice_segment_flag {
            return Err(DecodeError::Bitstream(
                "picture starts with a dependent slice segment".into(),
            ));
        }
        let pps = self
            .parser
            .pps(indep.slice_pic_parameter_set_id)
            .cloned()
            .ok_or_else(|| DecodeError::Bitstream("missing PPS".into()))?;
        let sps = self
            .parser
            .sps(pps.pps_seq_parameter_set_id)
            .cloned()
            .ok_or_else(|| DecodeError::Bitstream("missing SPS".into()))?;
        if pps.pps_range_extension_flag
            || pps.pps_scc_extension_flag
            || pps.pps_multilayer_extension_flag
            || pps.pps_3d_extension_flag
        {
            return Err(DecodeError::Unsupported(
                "PPS range / SCC / multi-layer extension".into(),
            ));
        }
        let t = indep.nal_unit_type;
        let (irap, idr) = (indep.is_irap(), indep.is_idr());
        let no_rasl_output = idr || (16..=18).contains(&t) || (irap && !self.seen_picture);
        if irap {
            self.skip_rasl = no_rasl_output;
        }
        if matches!(t, 8 | 9) && self.skip_rasl {
            // §8.1.3: not decoded, not output; references before the IRAP are gone.
            return Ok(());
        }
        if !irap && self.session.is_none() {
            return Err(DecodeError::Bitstream(
                "stream does not start with an IRAP picture".into(),
            ));
        }
        self.ensure_session(&sps, irap && no_rasl_output)?;
        self.seen_picture = true;

        // §8.3.1 / §8.3.2: POC and the RPS; pictures it leaves out are gone.
        let current = self
            .dpb
            .begin_picture(&sps, indep)
            .map_err(bitstream("reference picture set"))?;
        for key in &current.released {
            if let (Some(slot), Some(s)) = (self.slots.remove(key), self.session.as_mut()) {
                s.release(slot);
            }
        }
        if current.skip {
            return Ok(());
        }
        let rps = &current.rps;
        for list in [&rps.st_curr_before, &rps.st_curr_after, &rps.lt_curr] {
            if list.iter().any(Option::is_none) {
                return Err(DecodeError::Bitstream(
                    "reference picture missing from the DPB".into(),
                ));
            }
        }

        let session = self.session.as_mut().expect("session ensured");
        let target = session.acquire().ok_or_else(|| {
            DecodeError::Bitstream("more pictures held than the DPB allows".into())
        })?;
        let poc = current.poc;

        // ReferenceFrames[]: every picture still marked for reference.
        let mut pic = match picture_parameters(&sps, &pps, segs) {
            Ok(p) => p,
            Err(e) => {
                session.release(target);
                return Err(e);
            }
        };
        pic.curr_pic = VAPictureHEVC {
            picture_id: session.surface(target),
            pic_order_cnt: poc,
            flags: 0,
            va_reserved: [0; 4],
        };
        // ReferenceFrames index of each held picture, by key.
        let mut frame_of_key = BTreeMap::new();
        let mut n = 0usize;
        for e in self.dpb.references() {
            let Some(&slot) = self.slots.get(&e.key) else {
                continue;
            };
            if n == 15 {
                session.release(target);
                return Err(DecodeError::Bitstream(
                    "more than 15 reference pictures".into(),
                ));
            }
            let mut flags = 0;
            if e.marking == HevcMarking::LongTerm {
                flags |= VA_PICTURE_HEVC_LONG_TERM_REFERENCE;
            }
            let key = Some(e.key);
            if rps.st_curr_before.contains(&key) {
                flags |= VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
            } else if rps.st_curr_after.contains(&key) {
                flags |= VA_PICTURE_HEVC_RPS_ST_CURR_AFTER;
            } else if rps.lt_curr.contains(&key) {
                flags |= VA_PICTURE_HEVC_RPS_LT_CURR;
            }
            pic.reference_frames[n] = VAPictureHEVC {
                picture_id: session.surface(slot),
                pic_order_cnt: e.poc,
                flags,
                va_reserved: [0; 4],
            };
            frame_of_key.insert(e.key, n as u8);
            n += 1;
        }

        let iq = iq_matrix(&sps, &pps);
        let mut slices = Vec::with_capacity(segs.len());
        let mut last_indep: Option<VASliceParameterBufferHEVC> = None;
        for (k, seg) in segs.iter().enumerate() {
            let h = &seg.header;
            let built = if h.dependent_slice_segment_flag {
                last_indep
                    .ok_or_else(|| DecodeError::Bitstream("dependent slice segment first".into()))
                    .map(|mut base| {
                        base.long_slice_flags |= 1 << long_slice_flags::DEPENDENT_SLICE_SEGMENT;
                        base.long_slice_flags &= !(1 << long_slice_flags::LAST_SLICE_OF_PIC);
                        base.num_entry_point_offsets = 0;
                        base
                    })
            } else {
                let lists = if h.slice_type.is_inter() {
                    self.dpb
                        .ref_pic_lists(h)
                        .map(Some)
                        .map_err(bitstream("reference picture lists"))
                } else {
                    Ok(None)
                };
                lists
                    .and_then(|lists| slice_parameters(h, lists.as_ref(), &frame_of_key))
                    .inspect(|sp| last_indep = Some(*sp))
            };
            let mut sp = match built {
                Ok(sp) => sp,
                Err(e) => {
                    session.release(target);
                    return Err(e);
                }
            };
            sp.slice_data_size = seg.coded.len() as u32;
            sp.slice_data_offset = 0;
            sp.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
            sp.slice_data_byte_offset = (2 + h.header_bytes) as u32;
            sp.slice_data_num_emu_prevn_bytes = h.header_emulation_bytes as u16;
            sp.slice_segment_address = h.slice_segment_address;
            sp.num_entry_point_offsets = h.num_entry_point_offsets as u16;
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
            buffers.push(Buffer::bytes(
                buffer_type::VASliceDataBufferType,
                &seg.coded,
            ));
        }
        let submitted = session
            .submit(target, &buffers)
            .and_then(|()| session.sync(target));
        if let Err(e) = submitted {
            session.release(target);
            return Err(e);
        }

        // The picture joins the DPB as a short-term reference unless the
        // DPB holds one picture only (§C.5.2.2: it is emptied first).
        let finished = match self.dpb.finish_picture() {
            Ok(f) => f,
            Err(e) => {
                session.release(target);
                return Err(bitstream("reference picture set")(e));
            }
        };
        for key in &finished.released {
            if let Some(slot) = self.slots.remove(key) {
                session.release(slot);
            }
        }
        if finished.stored {
            self.slots.insert(finished.key, target);
        } else {
            self.returned_only = Some(target);
        }
        let cw = sps.conformance_window.unwrap_or_default();
        let (sub_w, sub_h) = (2u32, 2u32);
        let left = sub_w * cw.left;
        let top = sub_h * cw.top;
        let width = sps.pic_width_in_luma_samples - left - sub_w * cw.right;
        let height = sps.pic_height_in_luma_samples - top - sub_h * cw.bottom;
        self.out.push(HevcPicture {
            slot: target,
            poc,
            output: indep.pic_output_flag,
            crop: (left, top, width, height),
            bit_depth: sps.bit_depth_luma_minus8 + 8,
            irap,
            starts_sequence: irap && no_rasl_output,
        });
        Ok(())
    }
}

fn bit(flag: bool, at: u32) -> u32 {
    u32::from(flag) << at
}

fn picture_parameters(
    sps: &SeqParameterSet,
    pps: &PicParameterSet,
    segs: &[Segment],
) -> Result<VAPictureParameterBufferHEVC, DecodeError> {
    let indep = &segs[0].header;
    // The PCM fields are only coded with pcm_enabled_flag.
    let pcm = sps.pcm_enabled_flag;
    let pcm_or_0 = |v: u8| if pcm { v } else { 0 };
    // A dependent segment repeats its independent segment's type.
    let intra = segs
        .iter()
        .all(|s| s.header.dependent_slice_segment_flag || s.header.slice_type == SliceType::I);
    let no_b = segs
        .iter()
        .all(|s| s.header.dependent_slice_segment_flag || s.header.slice_type != SliceType::B);
    use pic_fields as pf;
    use slice_parsing_fields as sf;
    let pic_fields = u32::from(sps.chroma_format_idc) << pf::CHROMA_FORMAT_IDC
        | bit(sps.separate_colour_plane_flag, pf::SEPARATE_COLOUR_PLANE)
        | bit(sps.pcm_enabled_flag, pf::PCM_ENABLED)
        | bit(sps.scaling_list_enabled_flag, pf::SCALING_LIST_ENABLED)
        | bit(pps.transform_skip_enabled_flag, pf::TRANSFORM_SKIP_ENABLED)
        | bit(sps.amp_enabled_flag, pf::AMP_ENABLED)
        | bit(
            sps.strong_intra_smoothing_enabled_flag,
            pf::STRONG_INTRA_SMOOTHING_ENABLED,
        )
        | bit(
            pps.sign_data_hiding_enabled_flag,
            pf::SIGN_DATA_HIDING_ENABLED,
        )
        | bit(pps.constrained_intra_pred_flag, pf::CONSTRAINED_INTRA_PRED)
        | bit(pps.cu_qp_delta_enabled_flag, pf::CU_QP_DELTA_ENABLED)
        | bit(pps.weighted_pred_flag, pf::WEIGHTED_PRED)
        | bit(pps.weighted_bipred_flag, pf::WEIGHTED_BIPRED)
        | bit(
            pps.transquant_bypass_enabled_flag,
            pf::TRANSQUANT_BYPASS_ENABLED,
        )
        | bit(pps.tiles_enabled_flag, pf::TILES_ENABLED)
        | bit(
            pps.entropy_coding_sync_enabled_flag,
            pf::ENTROPY_CODING_SYNC_ENABLED,
        )
        | bit(
            pps.pps_loop_filter_across_slices_enabled_flag,
            pf::PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED,
        )
        | bit(
            // §7.4.3.3: inferred to be 1 without tiles.
            pps.tiles
                .as_ref()
                .is_none_or(|t| t.loop_filter_across_tiles_enabled_flag),
            pf::LOOP_FILTER_ACROSS_TILES_ENABLED,
        )
        | bit(
            pcm && sps.pcm_loop_filter_disabled_flag,
            pf::PCM_LOOP_FILTER_DISABLED,
        )
        | bit(sps.sps_max_num_reorder_pics == 0, pf::NO_PIC_REORDERING)
        | bit(no_b, pf::NO_BI_PRED);
    let slice_parsing_fields = bit(
        pps.lists_modification_present_flag,
        sf::LISTS_MODIFICATION_PRESENT,
    ) | bit(
        sps.long_term_ref_pics_present_flag,
        sf::LONG_TERM_REF_PICS_PRESENT,
    ) | bit(
        sps.sps_temporal_mvp_enabled_flag,
        sf::SPS_TEMPORAL_MVP_ENABLED,
    ) | bit(pps.cabac_init_present_flag, sf::CABAC_INIT_PRESENT)
        | bit(pps.output_flag_present_flag, sf::OUTPUT_FLAG_PRESENT)
        | bit(
            pps.dependent_slice_segments_enabled_flag,
            sf::DEPENDENT_SLICE_SEGMENTS_ENABLED,
        )
        | bit(
            pps.pps_slice_chroma_qp_offsets_present_flag,
            sf::PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT,
        )
        | bit(
            sps.sample_adaptive_offset_enabled_flag,
            sf::SAMPLE_ADAPTIVE_OFFSET_ENABLED,
        )
        | bit(
            pps.deblocking_filter_override_enabled_flag,
            sf::DEBLOCKING_FILTER_OVERRIDE_ENABLED,
        )
        | bit(
            pps.pps_deblocking_filter_disabled_flag,
            sf::PPS_DISABLE_DEBLOCKING_FILTER,
        )
        | bit(
            pps.slice_segment_header_extension_present_flag,
            sf::SLICE_SEGMENT_HEADER_EXTENSION_PRESENT,
        )
        | bit(indep.is_irap(), sf::RAP_PIC)
        | bit(indep.is_idr(), sf::IDR_PIC)
        | bit(intra, sf::INTRA_PIC);
    let (cols, rows) = pps
        .tiles
        .as_ref()
        .filter(|_| pps.tiles_enabled_flag)
        .map_or((0, 0), |t| {
            (t.num_tile_columns_minus1, t.num_tile_rows_minus1)
        });
    if cols > 19 || rows > 21 {
        return Err(DecodeError::Bitstream("too many tiles".into()));
    }
    let mut column_width_minus1 = [0u16; 19];
    let mut row_height_minus1 = [0u16; 21];
    if cols > 0 || rows > 0 {
        // §6.5.1: every tile but the last of its row / column.
        let (widths, heights) = tile_sizes_in_ctbs(sps, pps).map_err(bitstream("tiles"))?;
        for (d, w) in column_width_minus1.iter_mut().zip(&widths[..cols as usize]) {
            *d = (w - 1) as u16;
        }
        for (d, h) in row_height_minus1.iter_mut().zip(&heights[..rows as usize]) {
            *d = (h - 1) as u16;
        }
    }
    Ok(VAPictureParameterBufferHEVC {
        curr_pic: VAPictureHEVC::invalid(),
        reference_frames: [VAPictureHEVC::invalid(); 15],
        pic_width_in_luma_samples: sps.pic_width_in_luma_samples as u16,
        pic_height_in_luma_samples: sps.pic_height_in_luma_samples as u16,
        pic_fields,
        sps_max_dec_pic_buffering_minus1: sps.sps_max_dec_pic_buffering_minus1 as u8,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        pcm_sample_bit_depth_luma_minus1: pcm_or_0(sps.pcm_sample_bit_depth_luma_minus1),
        pcm_sample_bit_depth_chroma_minus1: pcm_or_0(sps.pcm_sample_bit_depth_chroma_minus1),
        log2_min_luma_coding_block_size_minus3: sps.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: sps.log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2: sps.log2_min_luma_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size: sps.log2_diff_max_min_luma_transform_block_size,
        log2_min_pcm_luma_coding_block_size_minus3: pcm_or_0(
            sps.log2_min_pcm_luma_coding_block_size_minus3,
        ),
        log2_diff_max_min_pcm_luma_coding_block_size: pcm_or_0(
            sps.log2_diff_max_min_pcm_luma_coding_block_size,
        ),
        max_transform_hierarchy_depth_intra: sps.max_transform_hierarchy_depth_intra,
        max_transform_hierarchy_depth_inter: sps.max_transform_hierarchy_depth_inter,
        init_qp_minus26: pps.init_qp_minus26 as i8,
        diff_cu_qp_delta_depth: pps.diff_cu_qp_delta_depth as u8,
        pps_cb_qp_offset: pps.pps_cb_qp_offset as i8,
        pps_cr_qp_offset: pps.pps_cr_qp_offset as i8,
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
        pps_beta_offset_div2: pps.pps_beta_offset_div2 as i8,
        pps_tc_offset_div2: pps.pps_tc_offset_div2 as i8,
        num_extra_slice_header_bits: pps.num_extra_slice_header_bits,
        // Bits of the inline st_ref_pic_set() (0 when SPS-indexed).
        st_rps_bits: indep.st_rps_bits,
        va_reserved: [0; 8],
    })
}

/// The scaling lists in effect, coefficients in raster order.
fn iq_matrix(sps: &SeqParameterSet, pps: &PicParameterSet) -> Option<VAIQMatrixBufferHEVC> {
    let lists = HevcScalingLists::in_effect(sps, pps)?;
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
        m.scaling_list_4x4[id].copy_from_slice(&lists.raster(0, id));
        m.scaling_list_8x8[id].copy_from_slice(&lists.raster(1, id));
        m.scaling_list_16x16[id].copy_from_slice(&lists.raster(2, id));
        m.scaling_list_dc_16x16[id] = lists.dc[0][id];
    }
    for (k, id) in [0usize, 3].into_iter().enumerate() {
        m.scaling_list_32x32[k].copy_from_slice(&lists.raster(3, id));
        m.scaling_list_dc_32x32[k] = lists.dc[1][id];
    }
    Some(m)
}

fn slice_parameters(
    h: &SliceSegmentHeader,
    lists: Option<&HevcRefPicLists>,
    frame_of_key: &BTreeMap<u32, u8>,
) -> Result<VASliceParameterBufferHEVC, DecodeError> {
    use long_slice_flags as lf;
    let slice_type = h.slice_type;
    let mut ref_pic_list = [[0xFFu8; 15]; 2];
    if let Some(lists) = lists {
        let fill = |dst: &mut [u8; 15], src: &[Option<_>]| -> Result<(), DecodeError> {
            for (i, e) in src.iter().take(15).enumerate() {
                let entry: &Option<oxideav_bitstream::hevc::dpb::HevcListEntry> = e;
                dst[i] = match entry {
                    Some(e) => *frame_of_key.get(&e.key).ok_or_else(|| {
                        DecodeError::Bitstream(
                            "reference list names a picture outside the DPB".into(),
                        )
                    })?,
                    None => {
                        return Err(DecodeError::Bitstream(
                            "reference list entry missing".into(),
                        ));
                    }
                };
            }
            Ok(())
        };
        fill(&mut ref_pic_list[0], &lists.list0)?;
        fill(&mut ref_pic_list[1], &lists.list1)?;
    }
    let type_code = u32::from(slice_type.code());
    let flags = type_code << lf::SLICE_TYPE
        | u32::from(h.colour_plane_id) << lf::COLOR_PLANE_ID
        | bit(h.slice_sao_luma_flag, lf::SLICE_SAO_LUMA)
        | bit(h.slice_sao_chroma_flag, lf::SLICE_SAO_CHROMA)
        | bit(h.mvd_l1_zero_flag, lf::MVD_L1_ZERO)
        | bit(h.cabac_init_flag, lf::CABAC_INIT)
        | bit(
            h.slice_temporal_mvp_enabled_flag,
            lf::SLICE_TEMPORAL_MVP_ENABLED,
        )
        | bit(
            h.slice_deblocking_filter_disabled_flag,
            lf::SLICE_DEBLOCKING_FILTER_DISABLED,
        )
        | bit(h.collocated_from_l0_flag, lf::COLLOCATED_FROM_L0)
        | bit(
            h.slice_loop_filter_across_slices_enabled_flag,
            lf::SLICE_LOOP_FILTER_ACROSS_SLICES_ENABLED,
        );
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
            h.collocated_ref_idx as u8
        } else {
            0xFF
        },
        // In force (the PPS defaults unless overridden); 0 where unused.
        num_ref_idx_l0_active_minus1: h.num_ref_idx_l0_active_minus1 as u8,
        num_ref_idx_l1_active_minus1: h.num_ref_idx_l1_active_minus1 as u8,
        slice_qp_delta: h.slice_qp_delta as i8,
        slice_cb_qp_offset: h.slice_cb_qp_offset as i8,
        slice_cr_qp_offset: h.slice_cr_qp_offset as i8,
        slice_beta_offset_div2: h.slice_beta_offset_div2 as i8,
        slice_tc_offset_div2: h.slice_tc_offset_div2 as i8,
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
        five_minus_max_num_merge_cand: h.five_minus_max_num_merge_cand as u8,
        num_entry_point_offsets: 0,
        entry_offset_to_subset_array: 0,
        slice_data_num_emu_prevn_bytes: 0,
        va_reserved: [0; 2],
    };
    if let Some(pwt) = &h.pred_weight_table {
        sp.luma_log2_weight_denom = pwt.luma_log2_weight_denom as u8;
        sp.delta_chroma_log2_weight_denom = pwt.delta_chroma_log2_weight_denom as i8;
        // The coded deltas, and ChromaOffsetLX (§7.4.7.3, eq. 7-56).
        let fill = |entries: &[HevcPredWeight],
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
                        co[i][j] = e.chroma_offset[j] as i8;
                    }
                }
            }
        };
        fill(
            &pwt.l0,
            &mut sp.delta_luma_weight_l0,
            &mut sp.luma_offset_l0,
            &mut sp.delta_chroma_weight_l0,
            &mut sp.chroma_offset_l0,
        );
        fill(
            &pwt.l1,
            &mut sp.delta_luma_weight_l1,
            &mut sp.luma_offset_l1,
            &mut sp.delta_chroma_weight_l1,
            &mut sp.chroma_offset_l1,
        );
    }
    Ok(sp)
}
