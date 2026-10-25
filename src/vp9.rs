//! VP9 decode through VA-API.
//!
//! Uncompressed frame headers come from oxideav-bitstream's
//! [`Vp9State`], which also keeps what a header leaves to later frames
//! (reference slot sizes, the colour configuration of the last intra
//! frame, loop filter deltas and segmentation features, §7.2 / §7.4.12
//! "keep the previous value" rules) and derives each segment's loop
//! filter levels (§8.8.1) and quantizers (§8.6.1). This module keeps a
//! surface per reference slot and hands the whole frame to the driver,
//! which reads the compressed header and tiles itself.
//!
//! Scope: profiles 0 and 2 (4:2:0 at 8 or 10 bits). A frame whose size
//! differs from the sequence's outside a key frame (reference scaling)
//! is [`DecodeError::Unsupported`], so a caller can continue the stream
//! in software from its last key frame.
//!
//! One call takes one sample (a frame or a superframe) and returns the
//! frame it shows, if any, as soon as the hardware has finished it.

use oxideav_bitstream::vp9::frame::{
    MAX_LOOP_FILTER, SEG_LVL_ALT_L, SEG_LVL_REF_FRAME, SEG_LVL_SKIP, Vp9ColorConfig as ColorConfig,
    Vp9FrameHeader, Vp9FrameType, Vp9InterpolationFilter, Vp9State, split_superframe,
};

use crate::display::Display;
use crate::session::{Buffer, DecodeError, DecodeSession, Planes, SurfaceFormat, check_decode};
use crate::sys::vp9::*;
use crate::sys::{VA_INVALID_SURFACE, VA_SLICE_DATA_FLAG_ALL, buffer_type, profile};

/// Eight reference slots, the frame being decoded and the frame last
/// returned.
const SURFACES: usize = 10;

/// A shown frame. Its surface stays valid until the next call into the
/// decoder.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
#[non_exhaustive]
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
    /// Header state across frames.
    state: Vp9State,
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
            state: Vp9State::new(),
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
        let h = self
            .state
            .parse_frame(frame)
            .map_err(|e| DecodeError::Bitstream(format!("VP9 frame header: {e:?}")))?;

        if h.show_existing_frame {
            let index = usize::from(h.frame_to_show_map_idx & 7);
            let slot = self.refs[index]
                .ok_or_else(|| DecodeError::Bitstream("shown reference slot is empty".into()))?;
            let (width, height) = self.state.ref_size[index];
            let bit_depth = self.state.ref_color[index].map_or(0, |c| c.bit_depth);
            self.retain(slot);
            return Ok(Some(Vp9Picture {
                slot,
                width,
                height,
                bit_depth,
                color: self.state.color.unwrap_or(h.color),
                key_frame: false,
            }));
        }

        let key = h.frame_type == Vp9FrameType::Key;
        let bit_depth = h.color.bit_depth;
        if !h.color.subsampling_x || !h.color.subsampling_y {
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

        let pic = self.picture_parameters(&h)?;
        let slice = slice_parameters(&h, frame.len());

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

        // Reference slot sizes, deltas and features the frame leaves.
        self.state.commit(&h);
        for i in 0..8 {
            if h.refresh_frame_flags & (1 << i) != 0 {
                self.retain(target);
                if let Some(old) = self.refs[i].replace(target) {
                    self.unuse(old);
                }
            }
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
            color: h.color,
            key_frame: key,
        }))
    }

    fn picture_parameters(
        &self,
        h: &Vp9FrameHeader,
    ) -> Result<VADecPictureParameterBufferVP9, DecodeError> {
        let seg = &h.segmentation;
        let intra = h.is_intra();
        // §7.2.7 numbering (EIGHTTAP 0, EIGHTTAP_SMOOTH 1, EIGHTTAP_SHARP 2,
        // BILINEAR 3, SWITCHABLE 4); not coded in intra frames.
        let interpolation_filter = match h.interpolation_filter {
            _ if intra => 0u8,
            Vp9InterpolationFilter::EightTapSmooth => 1,
            Vp9InterpolationFilter::EightTapSharp => 2,
            Vp9InterpolationFilter::Bilinear => 3,
            Vp9InterpolationFilter::Switchable => 4,
            _ => 0,
        };
        let session = self.session.as_ref().expect("session ensured");
        let mut reference_frames = [VA_INVALID_SURFACE; 8];
        for (i, slot) in self.refs.iter().enumerate() {
            if let Some(s) = slot {
                reference_frames[i] = session.surface(*s);
            }
        }
        let b = |flag: bool, at: u32| u32::from(flag) << at;
        use pic_fields::*;
        let mut fields = b(h.color.subsampling_x, SUBSAMPLING_X)
            | b(h.color.subsampling_y, SUBSAMPLING_Y)
            | b(h.frame_type != Vp9FrameType::Key, FRAME_TYPE)
            | b(h.show_frame, SHOW_FRAME)
            | b(h.error_resilient_mode, ERROR_RESILIENT_MODE)
            | b(h.intra_only, INTRA_ONLY)
            | b(h.allow_high_precision_mv, ALLOW_HIGH_PRECISION_MV)
            | (u32::from(interpolation_filter) << MCOMP_FILTER_TYPE)
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
        if !intra {
            let idx = h.ref_frame_idx;
            for (k, (at, bias_at)) in [
                (LAST_REF_FRAME, LAST_REF_FRAME_SIGN_BIAS),
                (GOLDEN_REF_FRAME, GOLDEN_REF_FRAME_SIGN_BIAS),
                (ALT_REF_FRAME, ALT_REF_FRAME_SIGN_BIAS),
            ]
            .into_iter()
            .enumerate()
            {
                let i = usize::from(idx[k] & 7);
                if self.refs[i].is_none() {
                    return Err(DecodeError::Bitstream(
                        "inter frame refers to an empty slot".into(),
                    ));
                }
                fields |= (u32::from(idx[k] & 7) << at) | b(h.ref_frame_sign_bias[k], bias_at);
            }
        }
        let header_len = u8::try_from(h.uncompressed_header_size).map_err(|_| {
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
            // 255 where the frame does not code them.
            mb_segment_tree_probs: seg.tree_probs,
            segment_pred_probs: seg.pred_probs,
            profile: h.profile,
            bit_depth: h.color.bit_depth,
            va_reserved: [0; 8],
        })
    }
}

/// The frame's segment parameters: §8.8.1 filter levels per reference
/// frame and mode class, §8.6.1 quantizers, and the reference / skip
/// features. The whole frame is the slice data.
fn slice_parameters(h: &Vp9FrameHeader, frame_len: usize) -> VASliceParameterBufferVP9 {
    let levels = h.loop_filter_levels();
    let seg = &h.segmentation;
    let mut out = VASliceParameterBufferVP9 {
        slice_data_size: frame_len as u32,
        slice_data_offset: 0,
        slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
        ..Default::default()
    };
    for (s, p) in out.seg_param.iter_mut().enumerate() {
        use segment_flags::*;
        p.segment_flags = (u16::from(seg.feature_active(s, SEG_LVL_REF_FRAME))
            << SEGMENT_REFERENCE_ENABLED)
            | (((seg.feature_data[s][SEG_LVL_REF_FRAME] as u16) & 3) << SEGMENT_REFERENCE)
            | (u16::from(seg.feature_active(s, SEG_LVL_SKIP)) << SEGMENT_REFERENCE_SKIPPED);
        p.filter_level = levels[s];
        // INTRA_FRAME has no mode classes, so drivers read [0][0] only;
        // [0][1] carries the segment's level unless the frame updates
        // the deltas, as this bridge has always sent it.
        let lf = &h.loop_filter;
        p.filter_level[0][1] = if lf.delta_update {
            0
        } else if seg.feature_active(s, SEG_LVL_ALT_L) {
            let d = i32::from(seg.feature_data[s][SEG_LVL_ALT_L]);
            let l = if seg.abs_or_delta_update {
                d
            } else {
                d + i32::from(lf.level)
            };
            l.clamp(0, MAX_LOOP_FILTER) as u8
        } else {
            lf.level
        };
        p.luma_dc_quant_scale = h.dc_quant(0, s) as i16;
        p.luma_ac_quant_scale = h.ac_quant(0, s) as i16;
        p.chroma_dc_quant_scale = h.dc_quant(1, s) as i16;
        p.chroma_ac_quant_scale = h.ac_quant(1, s) as i16;
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
