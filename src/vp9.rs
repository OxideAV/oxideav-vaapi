//! VP9 decode through VA-API.
//!
//! Uncompressed frame headers come from oxideav-vp9's parser; this
//! module keeps the state a header leaves to earlier frames (reference
//! slots and sizes, the colour configuration of the last intra frame,
//! loop filter deltas and segmentation features, §7.2 / §7.4.12 "keep
//! the previous value" rules), derives each segment's loop filter
//! levels (§8.8.1) and quantizers (§8.6.1), and hands the whole frame
//! to the driver, which reads the compressed header and tiles itself.
//!
//! Scope: profiles 0 and 2 (4:2:0 at 8 or 10 bits). A frame whose size
//! differs from the sequence's outside a key frame (reference scaling)
//! is [`DecodeError::Unsupported`], so a caller can continue the stream
//! in software from its last key frame.
//!
//! One call takes one sample (a frame or a superframe) and returns the
//! frame it shows, if any, as soon as the hardware has finished it.

use oxideav_vp9::{
    ColorConfig, FrameType, MAX_SEGMENTS, RefFrameState, SEG_LVL_MAX, SegmentationParams,
    Vp9FrameHeader, get_ac_quant, get_dc_quant, loop_filter_frame_init,
    parse_uncompressed_header_with_refs, split_superframe,
};

use crate::display::Display;
use crate::session::{Buffer, DecodeError, DecodeSession, Planes, SurfaceFormat, check_decode};
use crate::sys::vp9::*;
use crate::sys::{VA_INVALID_SURFACE, VA_SLICE_DATA_FLAG_ALL, buffer_type, profile};

/// §6.4.9 feature indices.
const SEG_LVL_REF_FRAME: usize = 2;
const SEG_LVL_SKIP: usize = 3;
/// Eight reference slots, the frame being decoded and the frame last
/// returned.
const SURFACES: usize = 10;

/// A shown frame. Its surface stays valid until the next call into the
/// decoder.
#[derive(Clone, Debug)]
pub struct Vp9Picture {
    /// Surface slot in the decoder's session.
    pub slot: usize,
    pub width: u32,
    pub height: u32,
    /// 8 or 10.
    pub bit_depth: u8,
    /// The colour configuration in force for the frame.
    pub color: ColorConfig,
    /// `true` when the shown frame is a key frame decoded by this call:
    /// decoding can start here.
    pub key_frame: bool,
}

/// What a stream needs from the driver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Vp9Requirements {
    pub va_profile: i32,
    pub format: SurfaceFormat,
    pub width: u32,
    pub height: u32,
}

/// See the module documentation.
pub struct Vp9Decoder {
    display: Option<Display>,
    session: Option<DecodeSession>,
    requirements: Option<Vp9Requirements>,
    /// Users of each surface slot: reference slots holding it, plus the
    /// frame last returned.
    users: [u8; SURFACES],
    refs: [Option<usize>; 8],
    ref_dims: [(u32, u32); 8],
    ref_depth: [u8; 8],
    color: Option<ColorConfig>,
    lf_ref_deltas: [i8; 4],
    lf_mode_deltas: [i8; 2],
    seg_abs_or_delta: bool,
    seg_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    seg_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
    returned: Option<usize>,
}

impl Vp9Decoder {
    pub fn new(display: Display) -> Self {
        Vp9Decoder {
            display: Some(display),
            session: None,
            requirements: None,
            users: [0; SURFACES],
            refs: [None; 8],
            ref_dims: [(0, 0); 8],
            ref_depth: [0; 8],
            color: None,
            lf_ref_deltas: [1, 0, -1, -1],
            lf_mode_deltas: [0, 0],
            seg_abs_or_delta: false,
            seg_enabled: [[false; SEG_LVL_MAX]; MAX_SEGMENTS],
            seg_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
            returned: None,
        }
    }

    /// The VA profile, surface format and size a sequence needs, or why
    /// this bridge cannot decode it.
    pub fn requirements(
        profile_number: u8,
        bit_depth: u8,
        width: u32,
        height: u32,
    ) -> Result<Vp9Requirements, DecodeError> {
        let (va_profile, format) = match (profile_number, bit_depth) {
            (0, 8) => (profile::VAProfileVP9Profile0, SurfaceFormat::Nv12),
            (2, 10) => (profile::VAProfileVP9Profile2, SurfaceFormat::P010),
            _ => {
                return Err(DecodeError::Unsupported(format!(
                    "VP9 profile {profile_number} at {bit_depth} bits"
                )));
            }
        };
        if width == 0 || height == 0 || width > 65536 || height > 65536 {
            return Err(DecodeError::Unsupported(format!("size {width}x{height}")));
        }
        Ok(Vp9Requirements {
            va_profile,
            format,
            width,
            height,
        })
    }

    /// Whether `display` decodes such a sequence.
    pub fn supports(display: &Display, r: &Vp9Requirements) -> Result<(), DecodeError> {
        check_decode(display, r.va_profile, r.format, r.width, r.height)
    }

    /// Create the session for a sequence ahead of its first key frame,
    /// so a driver that refuses it is found before any sample.
    pub fn prepare(&mut self, r: Vp9Requirements) -> Result<(), DecodeError> {
        self.ensure_session(r)
    }

    pub fn session(&self) -> Option<&DecodeSession> {
        self.session.as_ref()
    }

    /// Copy a returned picture's samples into `planes` (NV12 / P010).
    pub fn read(
        &mut self,
        picture: &Vp9Picture,
        planes: &mut Planes<'_>,
    ) -> Result<(), DecodeError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| DecodeError::Bitstream("no session".into()))?;
        session.read(picture.slot, 0, 0, picture.width, picture.height, planes)
    }

    /// Decode one sample (a frame or a superframe); returns the frame it
    /// shows.
    pub fn decode(&mut self, sample: &[u8]) -> Result<Option<Vp9Picture>, DecodeError> {
        if let Some(slot) = self.returned.take() {
            self.unuse(slot);
        }
        let frames = split_superframe(sample);
        if frames.is_empty() {
            return Err(DecodeError::Bitstream("empty sample".into()));
        }
        let mut shown: Option<Vp9Picture> = None;
        for frame in frames {
            if let Some(p) = self.decode_frame(frame)?
                && let Some(old) = shown.replace(p)
            {
                self.unuse(old.slot);
            }
        }
        if let Some(p) = &shown {
            self.returned = Some(p.slot);
        }
        Ok(shown)
    }

    fn ensure_session(&mut self, r: Vp9Requirements) -> Result<(), DecodeError> {
        if self.requirements == Some(r) && self.session.is_some() {
            return Ok(());
        }
        let display = match self.session.take() {
            Some(s) => s.into_display(),
            None => self
                .display
                .take()
                .ok_or_else(|| DecodeError::Bitstream("decoder lost its display".into()))?,
        };
        self.users = [0; SURFACES];
        self.refs = [None; 8];
        self.returned = None;
        self.requirements = None;
        match DecodeSession::new(display, r.va_profile, r.format, r.width, r.height, SURFACES) {
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

    fn retain(&mut self, slot: usize) {
        self.users[slot] += 1;
    }

    fn unuse(&mut self, slot: usize) {
        self.users[slot] = self.users[slot].saturating_sub(1);
        if self.users[slot] == 0
            && let Some(s) = self.session.as_mut()
        {
            s.release(slot);
        }
    }

    fn decode_frame(&mut self, frame: &[u8]) -> Result<Option<Vp9Picture>, DecodeError> {
        let refs = self.color.map(|color_config| RefFrameState {
            ref_dims: &self.ref_dims,
            color_config,
        });
        let h = parse_uncompressed_header_with_refs(frame, refs)
            .map_err(|e| DecodeError::Bitstream(format!("VP9 frame header: {e:?}")))?;

        if h.show_existing_frame {
            let index = usize::from(h.frame_to_show_map_idx.unwrap_or(0) & 7);
            let slot = self.refs[index]
                .ok_or_else(|| DecodeError::Bitstream("shown reference slot is empty".into()))?;
            let (width, height) = self.ref_dims[index];
            self.retain(slot);
            return Ok(Some(Vp9Picture {
                slot,
                width,
                height,
                bit_depth: self.ref_depth[index],
                color: self.color.unwrap_or(h.color_config),
                key_frame: false,
            }));
        }

        let key = h.frame_type == FrameType::KeyFrame;
        let intra = key || h.intra_only;
        let bit_depth = h.color_config.bit_depth;
        if !h.color_config.subsampling_x || !h.color_config.subsampling_y {
            return Err(DecodeError::Unsupported(
                "VP9 chroma other than 4:2:0".into(),
            ));
        }
        let r = Self::requirements(h.profile, bit_depth, h.frame_width, h.frame_height)?;
        if key {
            if self.requirements != Some(r) {
                self.ensure_session(r)?;
            }
        } else if self.session.is_none() {
            return Err(DecodeError::Bitstream(
                "stream does not start with a key frame".into(),
            ));
        } else if self.requirements != Some(r) {
            return Err(DecodeError::Unsupported(format!(
                "VP9 frame of {}x{} at {} bits in a {:?} sequence",
                h.frame_width, h.frame_height, bit_depth, self.requirements
            )));
        }

        // §7.2: setup_past_independence() for intra and error-resilient frames.
        if intra || h.error_resilient_mode {
            self.lf_ref_deltas = [1, 0, -1, -1];
            self.lf_mode_deltas = [0, 0];
            self.seg_abs_or_delta = false;
            self.seg_enabled = [[false; SEG_LVL_MAX]; MAX_SEGMENTS];
            self.seg_data = [[0; SEG_LVL_MAX]; MAX_SEGMENTS];
        }
        for (slot, d) in self.lf_ref_deltas.iter_mut().zip(h.loop_filter.ref_deltas) {
            if let Some(d) = d {
                *slot = d;
            }
        }
        for (slot, d) in self
            .lf_mode_deltas
            .iter_mut()
            .zip(h.loop_filter.mode_deltas)
        {
            if let Some(d) = d {
                *slot = d;
            }
        }
        if h.segmentation.enabled && h.segmentation.update_data {
            self.seg_abs_or_delta = h.segmentation.abs_or_delta_update;
            self.seg_enabled = h.segmentation.feature_enabled;
            self.seg_data = h.segmentation.feature_data;
        }
        let seg = SegmentationParams {
            abs_or_delta_update: self.seg_abs_or_delta,
            feature_enabled: self.seg_enabled,
            feature_data: self.seg_data,
            ..h.segmentation
        };

        let pic = self.picture_parameters(&h, &seg)?;
        let slice = slice_parameters(
            &h,
            &seg,
            frame.len(),
            self.lf_ref_deltas,
            self.lf_mode_deltas,
        );

        let session = self.session.as_mut().expect("session ensured");
        let target = session
            .acquire()
            .ok_or_else(|| DecodeError::Bitstream("no free surface".into()))?;
        let buffers = [
            Buffer::of(buffer_type::VAPictureParameterBufferType, &pic),
            Buffer::of(buffer_type::VASliceParameterBufferType, &slice),
            Buffer::bytes(buffer_type::VASliceDataBufferType, frame),
        ];
        if let Err(e) = session
            .submit(target, &buffers)
            .and_then(|()| session.sync(target))
        {
            session.release(target);
            return Err(e);
        }
        self.users[target] = 0;

        let refresh = if key { 0xFF } else { h.refresh_frame_flags };
        for i in 0..8 {
            if refresh & (1 << i) != 0 {
                self.retain(target);
                if let Some(old) = self.refs[i].replace(target) {
                    self.unuse(old);
                }
                self.ref_dims[i] = (h.frame_width, h.frame_height);
                self.ref_depth[i] = bit_depth;
            }
        }
        if intra {
            self.color = Some(h.color_config);
        }
        if !h.show_frame {
            if self.users[target] == 0
                && let Some(s) = self.session.as_mut()
            {
                s.release(target);
            }
            return Ok(None);
        }
        self.retain(target);
        Ok(Some(Vp9Picture {
            slot: target,
            width: h.frame_width,
            height: h.frame_height,
            bit_depth,
            color: h.color_config,
            key_frame: key,
        }))
    }

    fn picture_parameters(
        &self,
        h: &Vp9FrameHeader,
        seg: &SegmentationParams,
    ) -> Result<VADecPictureParameterBufferVP9, DecodeError> {
        let session = self.session.as_ref().expect("session ensured");
        let mut reference_frames = [VA_INVALID_SURFACE; 8];
        for (i, slot) in self.refs.iter().enumerate() {
            if let Some(s) = slot {
                reference_frames[i] = session.surface(*s);
            }
        }
        let b = |flag: bool, at: u32| u32::from(flag) << at;
        use pic_fields::*;
        let mut fields = b(h.color_config.subsampling_x, SUBSAMPLING_X)
            | b(h.color_config.subsampling_y, SUBSAMPLING_Y)
            | b(h.frame_type != FrameType::KeyFrame, FRAME_TYPE)
            | b(h.show_frame, SHOW_FRAME)
            | b(h.error_resilient_mode, ERROR_RESILIENT_MODE)
            | b(h.intra_only, INTRA_ONLY)
            | b(h.allow_high_precision_mv, ALLOW_HIGH_PRECISION_MV)
            | (u32::from(h.interpolation_filter & 7) << MCOMP_FILTER_TYPE)
            | b(h.frame_parallel_decoding_mode, FRAME_PARALLEL_DECODING_MODE)
            | (u32::from(h.reset_frame_context & 3) << RESET_FRAME_CONTEXT)
            | b(h.refresh_frame_context, REFRESH_FRAME_CONTEXT)
            | (u32::from(h.frame_context_idx & 3) << FRAME_CONTEXT_IDX)
            | b(seg.enabled, SEGMENTATION_ENABLED)
            | b(
                seg.enabled && seg.temporal_update,
                SEGMENTATION_TEMPORAL_UPDATE,
            )
            | b(seg.enabled && seg.update_map, SEGMENTATION_UPDATE_MAP)
            | b(h.quantization.lossless, LOSSLESS_FLAG);
        if let Some(idx) = h.ref_frame_idx {
            for (k, (at, bias_at)) in [
                (LAST_REF_FRAME, LAST_REF_FRAME_SIGN_BIAS),
                (GOLDEN_REF_FRAME, GOLDEN_REF_FRAME_SIGN_BIAS),
                (ALT_REF_FRAME, ALT_REF_FRAME_SIGN_BIAS),
            ]
            .into_iter()
            .enumerate()
            {
                let i = usize::from(idx[k] & 7);
                if self.refs[i].is_none() && h.frame_type != FrameType::KeyFrame && !h.intra_only {
                    return Err(DecodeError::Bitstream(
                        "inter frame refers to an empty slot".into(),
                    ));
                }
                fields |= (u32::from(idx[k] & 7) << at) | b(h.ref_frame_sign_bias[k], bias_at);
            }
        }
        let header_len = u8::try_from(h.uncompressed_header_size_bytes).map_err(|_| {
            DecodeError::Bitstream("uncompressed header longer than 255 bytes".into())
        })?;
        Ok(VADecPictureParameterBufferVP9 {
            frame_width: u16::try_from(h.frame_width)
                .map_err(|_| DecodeError::Unsupported("VP9 width above 65535".into()))?,
            frame_height: u16::try_from(h.frame_height)
                .map_err(|_| DecodeError::Unsupported("VP9 height above 65535".into()))?,
            reference_frames,
            pic_fields: fields,
            filter_level: h.loop_filter.level,
            sharpness_level: h.loop_filter.sharpness,
            log2_tile_rows: h.tile_info.tile_rows_log2,
            log2_tile_columns: h.tile_info.tile_cols_log2,
            frame_header_length_in_bytes: header_len,
            first_partition_size: h.header_size_in_bytes,
            mb_segment_tree_probs: seg.tree_probs.unwrap_or([255; 7]),
            segment_pred_probs: seg.pred_prob.unwrap_or([255; 3]),
            profile: h.profile,
            bit_depth: h.color_config.bit_depth,
            va_reserved: [0; 8],
        })
    }
}

/// The frame's segment parameters: §8.8.1 filter levels per reference
/// frame and mode class, §8.6.1 quantizers, and the reference / skip
/// features. The whole frame is the slice data.
fn slice_parameters(
    h: &Vp9FrameHeader,
    seg: &SegmentationParams,
    frame_len: usize,
    ref_deltas: [i8; 4],
    mode_deltas: [i8; 2],
) -> VASliceParameterBufferVP9 {
    let levels = loop_filter_frame_init(&h.loop_filter, seg, ref_deltas, mode_deltas);
    let depth = h.color_config.bit_depth;
    let q = &h.quantization;
    let mut out = VASliceParameterBufferVP9 {
        slice_data_size: frame_len as u32,
        slice_data_offset: 0,
        slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
        ..Default::default()
    };
    for (s, p) in out.seg_param.iter_mut().enumerate() {
        let active = |feature: usize| seg.enabled && seg.feature_enabled[s][feature];
        use segment_flags::*;
        p.segment_flags = (u16::from(active(SEG_LVL_REF_FRAME)) << SEGMENT_REFERENCE_ENABLED)
            | (((seg.feature_data[s][SEG_LVL_REF_FRAME] as u16) & 3) << SEGMENT_REFERENCE)
            | (u16::from(active(SEG_LVL_SKIP)) << SEGMENT_REFERENCE_SKIPPED);
        for r in 0..4 {
            for m in 0..2 {
                p.filter_level[r][m] = levels.levels[s][r][m];
            }
        }
        p.luma_dc_quant_scale = get_dc_quant(0, seg, q, s, depth) as i16;
        p.luma_ac_quant_scale = get_ac_quant(0, seg, q, s, depth) as i16;
        p.chroma_dc_quant_scale = get_dc_quant(1, seg, q, s, depth) as i16;
        p.chroma_ac_quant_scale = get_ac_quant(1, seg, q, s, depth) as i16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_requirements() {
        let r = Vp9Decoder::requirements(0, 8, 1920, 1080).unwrap();
        assert_eq!(
            (r.va_profile, r.format),
            (profile::VAProfileVP9Profile0, SurfaceFormat::Nv12)
        );
        let r = Vp9Decoder::requirements(2, 10, 1920, 1080).unwrap();
        assert_eq!(
            (r.va_profile, r.format),
            (profile::VAProfileVP9Profile2, SurfaceFormat::P010)
        );
        assert!(matches!(
            Vp9Decoder::requirements(1, 8, 64, 64),
            Err(DecodeError::Unsupported(_))
        ));
        assert!(matches!(
            Vp9Decoder::requirements(2, 12, 64, 64),
            Err(DecodeError::Unsupported(_))
        ));
    }
}
