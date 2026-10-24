//! AV1 decode through VA-API.
//!
//! OBUs, sequence headers and frame headers come from oxideav-av1's
//! parser, which takes the cross-frame state a header refers to
//! ([`RefInfo`]: reference order hints and sizes, saved loop filter
//! deltas, segmentation features and global motion). This module keeps
//! that state as §7.20 reference_frame_update() leaves it, the film
//! grain parameters each reference slot saves (§7.20, loaded by
//! load_grain_params()), and a surface for every slot; it fills
//! `VADecPictureParameterBufferAV1` and one `VASliceParameterBufferAV1`
//! per tile, and hands the driver each tile group's tile data.
//!
//! Scope: profile 0 (4:2:0 at 8 or 10 bits, not monochrome), the
//! operating point 0 layers, no large-scale tile. A frame with film
//! grain decodes into its reference surface and a second, grained
//! display surface, as VA-API takes it; a later show_existing_frame of
//! it shows that display surface. Anything else is
//! [`DecodeError::Unsupported`] before any submission.
//!
//! One call takes one temporal unit and returns the frame it shows, if
//! any, as soon as the hardware has finished it.

use oxideav_av1::{
    FilmGrainParams, FrameHeader, FrameRestorationType, FrameType, InterpolationFilter,
    NUM_REF_FRAMES, ObuIter, ObuType, RefInfo, SequenceHeader, TxMode, WarpModelType,
    parse_frame_header_with_refs, parse_sequence_header, setup_shear,
};

use crate::display::Display;
use crate::session::{Buffer, DecodeError, DecodeSession, Planes, SurfaceFormat, check_decode};
use crate::sys::av1::*;
use crate::sys::{VA_INVALID_SURFACE, VA_SLICE_DATA_FLAG_ALL, buffer_type, profile};

const REFS: usize = NUM_REF_FRAMES as usize;

/// A shown frame. Its surface stays valid until the next call into the
/// decoder.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Av1Picture {
    /// Surface slot in the decoder's session.
    pub slot: usize,
    /// The upscaled frame size.
    pub width: u32,
    pub height: u32,
    /// 8 or 10.
    pub bit_depth: u8,
    /// `true` when the frame is a key frame shown by this call.
    pub key_frame: bool,
}

/// What a sequence needs from the driver.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Av1Requirements {
    pub va_profile: i32,
    pub format: SurfaceFormat,
    /// The sequence's maximum frame size: every surface has it.
    pub width: u32,
    pub height: u32,
    /// Film grain may be applied: frames need display surfaces too.
    pub film_grain: bool,
}

impl Av1Requirements {
    fn surfaces(&self) -> usize {
        // Eight reference slots and the frame being decoded, a display
        // surface beside each with film grain, and the frame last returned.
        if self.film_grain {
            2 * (REFS + 1) + 1
        } else {
            REFS + 2
        }
    }
}

/// A tile's data: its place in its tile group's buffer and in the frame.
#[derive(Copy, Clone, Debug)]
struct Tile {
    offset: u32,
    size: u32,
    row: u16,
    column: u16,
}

struct TileGroup<'a> {
    data: &'a [u8],
    tiles: Vec<Tile>,
    start: u16,
    end: u16,
}

struct Pending<'a> {
    header: FrameHeader,
    groups: Vec<TileGroup<'a>>,
    next_tile: u32,
}

/// See the module documentation.
pub struct Av1Decoder {
    display: Option<Display>,
    session: Option<DecodeSession>,
    requirements: Option<Av1Requirements>,
    sequence: Option<SequenceHeader>,
    info: RefInfo,
    /// Users of each surface slot: the reference slots and display
    /// entries holding it, plus the frame last returned.
    users: Vec<u8>,
    /// The decoded (grain-free) surface of each reference slot.
    refs: [Option<usize>; REFS],
    /// The surface a show_existing_frame of each slot shows.
    shown: [Option<usize>; REFS],
    /// Each slot's saved film grain parameters.
    grain: [Option<FilmGrainParams>; REFS],
    returned: Option<usize>,
}

impl Av1Decoder {
    pub fn new(display: Display) -> Self {
        Av1Decoder {
            display: Some(display),
            session: None,
            requirements: None,
            sequence: None,
            info: RefInfo::default(),
            users: Vec::new(),
            refs: [None; REFS],
            shown: [None; REFS],
            grain: Default::default(),
            returned: None,
        }
    }

    /// The VA profile, surface format and size a sequence needs, or why
    /// this bridge cannot decode it.
    pub fn requirements(seq: &SequenceHeader) -> Result<Av1Requirements, DecodeError> {
        let c = &seq.color_config;
        if seq.seq_profile != 0 || c.mono_chrome || !c.subsampling_x || !c.subsampling_y {
            return Err(DecodeError::Unsupported(format!(
                "AV1 profile {} (monochrome {}, subsampling {} {})",
                seq.seq_profile, c.mono_chrome, c.subsampling_x, c.subsampling_y
            )));
        }
        let format = match c.bit_depth {
            8 => SurfaceFormat::Nv12,
            10 => SurfaceFormat::P010,
            d => return Err(DecodeError::Unsupported(format!("AV1 at {d} bits"))),
        };
        Ok(Av1Requirements {
            va_profile: profile::VAProfileAV1Profile0,
            format,
            width: seq.max_frame_width_minus_1 + 1,
            height: seq.max_frame_height_minus_1 + 1,
            film_grain: seq.film_grain_params_present,
        })
    }

    /// Whether `display` decodes such a sequence.
    pub fn supports(display: &Display, r: &Av1Requirements) -> Result<(), DecodeError> {
        check_decode(display, r.va_profile, r.format, r.width, r.height)
    }

    /// The sequence header OBU among `obus` (the configuration OBUs of an
    /// `av1C` record, or a temporal unit).
    pub fn find_sequence_header(obus: &[u8]) -> Result<Option<SequenceHeader>, DecodeError> {
        for obu in ObuIter::new(obus) {
            let obu = obu.map_err(|e| DecodeError::Bitstream(format!("AV1 OBU: {e:?}")))?;
            if obu.obu_type == ObuType::SequenceHeader {
                return parse_sequence_header(obu.payload)
                    .map(Some)
                    .map_err(|e| DecodeError::Bitstream(format!("AV1 sequence header: {e:?}")));
            }
        }
        Ok(None)
    }

    /// Take a sequence ahead of its first temporal unit and create its
    /// session, so a driver that refuses it is found before any sample.
    pub fn prepare(&mut self, seq: SequenceHeader) -> Result<(), DecodeError> {
        let r = Self::requirements(&seq)?;
        self.ensure_session(r)?;
        self.sequence = Some(seq);
        Ok(())
    }

    pub fn session(&self) -> Option<&DecodeSession> {
        self.session.as_ref()
    }

    /// Copy a returned picture's samples into `planes` (NV12 / P010).
    pub fn read(
        &mut self,
        picture: &Av1Picture,
        planes: &mut Planes<'_>,
    ) -> Result<(), DecodeError> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| DecodeError::Bitstream("no session".into()))?;
        session.read(picture.slot, 0, 0, picture.width, picture.height, planes)
    }

    fn ensure_session(&mut self, r: Av1Requirements) -> Result<(), DecodeError> {
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
        self.refs = [None; REFS];
        self.shown = [None; REFS];
        self.grain = Default::default();
        self.info = RefInfo::default();
        self.returned = None;
        self.requirements = None;
        self.users = vec![0; r.surfaces()];
        match DecodeSession::new(
            display,
            r.va_profile,
            r.format,
            r.width,
            r.height,
            r.surfaces(),
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

    /// Release a slot just acquired if nothing came to use it.
    fn settle(&mut self, slot: usize) {
        if self.users[slot] == 0
            && let Some(s) = self.session.as_mut()
        {
            s.release(slot);
        }
    }

    /// Decode one temporal unit; returns the frame it shows.
    pub fn decode(&mut self, sample: &[u8]) -> Result<Option<Av1Picture>, DecodeError> {
        if let Some(slot) = self.returned.take() {
            self.unuse(slot);
        }
        let mut shown: Option<Av1Picture> = None;
        let mut pending: Option<Pending<'_>> = None;
        for obu in ObuIter::new(sample) {
            let obu = obu.map_err(|e| DecodeError::Bitstream(format!("AV1 OBU: {e:?}")))?;
            if !self.in_operating_point(obu.extension_flag, obu.temporal_id, obu.spatial_id) {
                continue;
            }
            match obu.obu_type {
                ObuType::SequenceHeader => {
                    let seq = parse_sequence_header(obu.payload).map_err(|e| {
                        DecodeError::Bitstream(format!("AV1 sequence header: {e:?}"))
                    })?;
                    let r = Self::requirements(&seq)?;
                    if self.requirements != Some(r) {
                        self.ensure_session(r)?;
                    }
                    self.sequence = Some(seq);
                }
                ObuType::FrameHeader | ObuType::Frame => {
                    if pending.is_some() {
                        return Err(DecodeError::Bitstream(
                            "frame header before the last frame's tiles".into(),
                        ));
                    }
                    let seq = self.sequence.as_ref().ok_or_else(|| {
                        DecodeError::Bitstream("frame before a sequence header".into())
                    })?;
                    let header = parse_frame_header_with_refs(obu.payload, seq, &self.info)
                        .map_err(|e| DecodeError::Bitstream(format!("AV1 frame header: {e:?}")))?;
                    if header.show_existing_frame {
                        let p = self.show_existing(&header)?;
                        if let Some(old) = shown.replace(p) {
                            self.unuse(old.slot);
                        }
                        continue;
                    }
                    let mut p = Pending {
                        header,
                        groups: Vec::new(),
                        next_tile: 0,
                    };
                    if obu.obu_type == ObuType::Frame {
                        let at = p.header.bits_consumed.div_ceil(8);
                        let body = obu.payload.get(at..).ok_or_else(|| {
                            DecodeError::Bitstream("frame OBU shorter than its header".into())
                        })?;
                        let group = tile_group(&p.header, body)?;
                        if group.start != 0 {
                            return Err(DecodeError::Bitstream(
                                "frame OBU tile group does not start at tile 0".into(),
                            ));
                        }
                        p.next_tile = u32::from(group.end) + 1;
                        p.groups.push(group);
                    }
                    pending = Some(p);
                }
                ObuType::TileGroup => {
                    let p = pending.as_mut().ok_or_else(|| {
                        DecodeError::Bitstream("tile group without a frame header".into())
                    })?;
                    let group = tile_group(&p.header, obu.payload)?;
                    if u32::from(group.start) != p.next_tile {
                        return Err(DecodeError::Bitstream("tile groups out of order".into()));
                    }
                    p.next_tile = u32::from(group.end) + 1;
                    p.groups.push(group);
                }
                _ => {}
            }
            let complete = pending.as_ref().is_some_and(|p| {
                let ti = p.header.tile_info.as_ref();
                ti.is_some_and(|t| p.next_tile == t.tile_cols * t.tile_rows)
            });
            if complete {
                let p = pending.take().expect("pending frame");
                if let Some(picture) = self.decode_frame(&p)?
                    && let Some(old) = shown.replace(picture)
                {
                    self.unuse(old.slot);
                }
            }
        }
        if pending.is_some() {
            return Err(DecodeError::Bitstream(
                "temporal unit ends inside a frame".into(),
            ));
        }
        if let Some(p) = &shown {
            self.returned = Some(p.slot);
        }
        Ok(shown)
    }

    fn in_operating_point(&self, extension: bool, temporal_id: u8, spatial_id: u8) -> bool {
        let Some(seq) = self.sequence.as_ref() else {
            return true;
        };
        let idc = seq
            .operating_points
            .first()
            .map_or(0, |o| o.operating_point_idc);
        if idc == 0 || !extension {
            return true;
        }
        (idc >> temporal_id) & 1 != 0 && (idc >> (u32::from(spatial_id) + 8)) & 1 != 0
    }

    fn show_existing(&mut self, h: &FrameHeader) -> Result<Av1Picture, DecodeError> {
        let idx = usize::from(h.frame_to_show_map_idx.unwrap_or(0)) % REFS;
        let slot = self.shown[idx]
            .ok_or_else(|| DecodeError::Bitstream("shown reference slot is empty".into()))?;
        let bit_depth = self
            .sequence
            .as_ref()
            .map_or(8, |s| s.color_config.bit_depth);
        let picture = Av1Picture {
            slot,
            width: self.info.upscaled_width[idx],
            height: self.info.frame_height[idx],
            bit_depth,
            key_frame: h.frame_type == FrameType::Key,
        };
        self.retain(slot);
        // §7.21: a shown key frame is loaded and stored into every slot.
        if h.frame_type == FrameType::Key {
            let (decoded, display) = (self.refs[idx], self.shown[idx]);
            let grain = self.grain[idx].clone();
            for i in 0..REFS {
                if h.refresh_frame_flags & (1 << i) == 0 || i == idx {
                    continue;
                }
                for s in [decoded, display].into_iter().flatten() {
                    self.retain(s);
                }
                for s in [self.refs[i].take(), self.shown[i].take()]
                    .into_iter()
                    .flatten()
                {
                    self.unuse(s);
                }
                self.refs[i] = decoded;
                self.shown[i] = display;
                self.grain[i] = grain.clone();
                copy_ref_info(&mut self.info, idx, i);
            }
        }
        Ok(picture)
    }

    fn decode_frame(&mut self, p: &Pending<'_>) -> Result<Option<Av1Picture>, DecodeError> {
        let h = &p.header;
        let seq = self.sequence.clone().expect("sequence present");
        let fs = h
            .frame_size
            .as_ref()
            .ok_or_else(|| DecodeError::Bitstream("frame without a size".into()))?;
        let r = self
            .requirements
            .ok_or_else(|| DecodeError::Bitstream("no session".into()))?;
        if fs.upscaled_width > r.width || fs.frame_height > r.height {
            return Err(DecodeError::Bitstream(
                "frame larger than the sequence maximum".into(),
            ));
        }
        if !h.frame_is_intra {
            let refs = h
                .inter_refs
                .as_ref()
                .ok_or_else(|| DecodeError::Bitstream("inter frame without references".into()))?;
            if refs
                .ref_frame_idx
                .iter()
                .any(|&i| self.refs[usize::from(i) % REFS].is_none())
            {
                return Err(DecodeError::Bitstream(
                    "inter frame refers to an empty slot".into(),
                ));
            }
        }
        // §5.9.30: load_grain_params() for a predicted grain set.
        let grain = match h.film_grain_params.as_ref() {
            Some(g) if g.apply_grain && g.predicted => self.grain
                [usize::from(g.film_grain_params_ref_idx) % REFS]
                .clone()
                .map(|mut saved| {
                    saved.grain_seed = g.grain_seed;
                    saved.apply_grain = true;
                    saved
                }),
            Some(g) if g.apply_grain => Some(g.clone()),
            _ => None,
        };

        let session = self.session.as_mut().expect("session ensured");
        let target = session
            .acquire()
            .ok_or_else(|| DecodeError::Bitstream("no free surface".into()))?;
        let display = if grain.is_some() {
            match session.acquire() {
                Some(d) => Some(d),
                None => {
                    session.release(target);
                    return Err(DecodeError::Bitstream("no free display surface".into()));
                }
            }
        } else {
            None
        };
        let mut pic = picture_parameters(&seq, h, grain.as_ref());
        pic.current_frame = session.surface(target);
        pic.current_display_picture = session.surface(display.unwrap_or(target));
        for (i, slot) in self.refs.iter().enumerate() {
            pic.ref_frame_map[i] = slot.map_or(VA_INVALID_SURFACE, |s| session.surface(s));
        }

        let mut params: Vec<Vec<VASliceParameterBufferAV1>> = Vec::with_capacity(p.groups.len());
        for g in &p.groups {
            params.push(
                g.tiles
                    .iter()
                    .map(|t| VASliceParameterBufferAV1 {
                        slice_data_size: t.size,
                        slice_data_offset: t.offset,
                        slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
                        tile_row: t.row,
                        tile_column: t.column,
                        tg_start: g.start,
                        tg_end: g.end,
                        ..Default::default()
                    })
                    .collect(),
            );
        }
        let mut buffers = vec![Buffer::of(buffer_type::VAPictureParameterBufferType, &pic)];
        for (g, list) in p.groups.iter().zip(&params) {
            buffers.push(Buffer::array(buffer_type::VASliceParameterBufferType, list));
            buffers.push(Buffer::bytes(buffer_type::VASliceDataBufferType, g.data));
        }
        let result = session.submit(target, &buffers).and_then(|()| {
            session.sync(target)?;
            if let Some(d) = display {
                session.sync(d)?;
            }
            Ok(())
        });
        if let Err(e) = result {
            session.release(target);
            if let Some(d) = display {
                session.release(d);
            }
            return Err(e);
        }
        self.users[target] = 0;
        if let Some(d) = display {
            self.users[d] = 0;
        }
        let shown_slot = display.unwrap_or(target);

        // §7.20 reference_frame_update().
        for i in 0..REFS {
            if h.refresh_frame_flags & (1 << i) == 0 {
                continue;
            }
            self.retain(target);
            if display.is_some() {
                self.retain(shown_slot);
            } else {
                self.retain(target);
            }
            for s in [self.refs[i].take(), self.shown[i].take()]
                .into_iter()
                .flatten()
            {
                self.unuse(s);
            }
            self.refs[i] = Some(target);
            self.shown[i] = Some(shown_slot);
            self.grain[i] = grain.clone();
            store_ref_info(&mut self.info, i, h);
        }
        let picture = if h.show_frame {
            self.retain(shown_slot);
            Some(Av1Picture {
                slot: shown_slot,
                width: fs.upscaled_width,
                height: fs.frame_height,
                bit_depth: seq.color_config.bit_depth,
                key_frame: h.frame_type == FrameType::Key,
            })
        } else {
            None
        };
        self.settle(target);
        if let Some(d) = display {
            self.settle(d);
        }
        Ok(picture)
    }
}

/// §7.20 for slot `i`: the frame's order hint, identifiers, sizes and the
/// parameters later headers load from it.
fn store_ref_info(info: &mut RefInfo, i: usize, h: &FrameHeader) {
    let fs = h.frame_size.as_ref().expect("decoded frame has a size");
    info.valid[i] = true;
    info.order_hint[i] = h.order_hint;
    info.frame_id[i] = h.current_frame_id;
    info.upscaled_width[i] = fs.upscaled_width;
    info.frame_height[i] = fs.frame_height;
    info.render_width[i] = fs.render_width;
    info.render_height[i] = fs.render_height;
    info.frame_type_is_key[i] = h.frame_type == FrameType::Key;
    info.saved_gm_params[i] = h
        .global_motion_params
        .as_ref()
        .map_or(oxideav_av1::prev_gm_params_default(), |g| g.gm_params);
    let (refs, modes) = h.loop_filter_params.as_ref().map_or(
        (oxideav_av1::LOOP_FILTER_REF_DELTAS_DEFAULT, [0i8; 2]),
        |lf| (lf.loop_filter_ref_deltas, lf.loop_filter_mode_deltas),
    );
    info.saved_lf_ref_deltas[i] = refs;
    info.saved_lf_mode_deltas[i] = modes;
    match h.segmentation_params.as_ref() {
        Some(s) => {
            info.saved_seg_feature_active[i] = s.segment_feature_active;
            info.saved_seg_feature_data[i] = s.segment_feature_data;
        }
        None => {
            info.saved_seg_feature_active[i] = Default::default();
            info.saved_seg_feature_data[i] = Default::default();
        }
    }
}

fn copy_ref_info(info: &mut RefInfo, from: usize, to: usize) {
    info.valid[to] = info.valid[from];
    info.order_hint[to] = info.order_hint[from];
    info.frame_id[to] = info.frame_id[from];
    info.upscaled_width[to] = info.upscaled_width[from];
    info.frame_height[to] = info.frame_height[from];
    info.render_width[to] = info.render_width[from];
    info.render_height[to] = info.render_height[from];
    info.frame_type_is_key[to] = info.frame_type_is_key[from];
    info.saved_gm_params[to] = info.saved_gm_params[from];
    info.saved_lf_ref_deltas[to] = info.saved_lf_ref_deltas[from];
    info.saved_lf_mode_deltas[to] = info.saved_lf_mode_deltas[from];
    info.saved_seg_feature_active[to] = info.saved_seg_feature_active[from];
    info.saved_seg_feature_data[to] = info.saved_seg_feature_data[from];
}

/// §5.11.1 tile_group_obu(): the group's tile range and each tile's data.
fn tile_group<'a>(h: &FrameHeader, body: &'a [u8]) -> Result<TileGroup<'a>, DecodeError> {
    let ti = h
        .tile_info
        .as_ref()
        .ok_or_else(|| DecodeError::Bitstream("frame header without tile info".into()))?;
    let num_tiles = ti.tile_cols * ti.tile_rows;
    let bad = || DecodeError::Bitstream("malformed tile group".into());
    let mut bits = BitCursor { data: body, at: 0 };
    let (mut start, mut end) = (0u32, num_tiles - 1);
    if num_tiles > 1 && bits.read(1).ok_or_else(bad)? == 1 {
        let n = ti.tile_cols_log2 + ti.tile_rows_log2;
        start = bits.read(n).ok_or_else(bad)?;
        end = bits.read(n).ok_or_else(bad)?;
    }
    if start > end || end >= num_tiles {
        return Err(bad());
    }
    let mut at = bits.at.div_ceil(8);
    let size_bytes = usize::from(ti.tile_size_bytes);
    let mut tiles = Vec::with_capacity((end - start + 1) as usize);
    for n in start..=end {
        let size = if n == end {
            body.len().checked_sub(at).ok_or_else(bad)?
        } else {
            let field = body.get(at..at + size_bytes).ok_or_else(bad)?;
            at += size_bytes;
            field
                .iter()
                .rev()
                .fold(0usize, |v, &b| (v << 8) | usize::from(b))
                + 1
        };
        if size == 0 || at + size > body.len() {
            return Err(bad());
        }
        tiles.push(Tile {
            offset: at as u32,
            size: size as u32,
            row: (n / ti.tile_cols) as u16,
            column: (n % ti.tile_cols) as u16,
        });
        at += size;
    }
    Ok(TileGroup {
        data: body,
        tiles,
        start: start as u16,
        end: end as u16,
    })
}

struct BitCursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl BitCursor<'_> {
    fn read(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(v)
    }
}

fn bit(flag: bool, at: u32) -> u32 {
    u32::from(flag) << at
}

/// The picture parameters a header and its sequence give; the caller
/// fills in the surfaces.
fn picture_parameters(
    seq: &SequenceHeader,
    h: &FrameHeader,
    grain: Option<&FilmGrainParams>,
) -> VADecPictureParameterBufferAV1 {
    let c = &seq.color_config;
    let fs = h.frame_size.as_ref().expect("decoded frame has a size");
    let mut p = VADecPictureParameterBufferAV1::zeroed();
    p.profile = seq.seq_profile;
    p.order_hint_bits_minus_1 = seq.order_hint_bits.saturating_sub(1);
    p.bit_depth_idx = (c.bit_depth - 8) / 2;
    p.matrix_coefficients = c.matrix_coefficients;
    {
        use seq_info::*;
        p.seq_info_fields = bit(seq.still_picture, STILL_PICTURE)
            | bit(seq.use_128x128_superblock, USE_128X128_SUPERBLOCK)
            | bit(seq.enable_filter_intra, ENABLE_FILTER_INTRA)
            | bit(seq.enable_intra_edge_filter, ENABLE_INTRA_EDGE_FILTER)
            | bit(seq.enable_interintra_compound, ENABLE_INTERINTRA_COMPOUND)
            | bit(seq.enable_masked_compound, ENABLE_MASKED_COMPOUND)
            | bit(seq.enable_dual_filter, ENABLE_DUAL_FILTER)
            | bit(seq.enable_order_hint, ENABLE_ORDER_HINT)
            | bit(seq.enable_jnt_comp, ENABLE_JNT_COMP)
            | bit(seq.enable_cdef, ENABLE_CDEF)
            | bit(c.mono_chrome, MONO_CHROME)
            | bit(c.color_range, COLOR_RANGE)
            | bit(c.subsampling_x, SUBSAMPLING_X)
            | bit(c.subsampling_y, SUBSAMPLING_Y)
            | bit(c.chroma_sample_position != 0, CHROMA_SAMPLE_POSITION)
            | bit(seq.film_grain_params_present, FILM_GRAIN_PARAMS_PRESENT);
    }
    p.frame_width_minus1 = (fs.upscaled_width - 1) as u16;
    p.frame_height_minus1 = (fs.frame_height - 1) as u16;
    if let Some(r) = h.inter_refs.as_ref() {
        p.ref_frame_idx = r.ref_frame_idx;
    }
    p.primary_ref_frame = h.primary_ref_frame;
    p.order_hint = h.order_hint as u8;

    if let Some(s) = h.segmentation_params.as_ref() {
        use segment_info::*;
        p.seg_info.segment_info_fields = bit(s.enabled, ENABLED)
            | bit(s.update_map, UPDATE_MAP)
            | bit(s.temporal_update, TEMPORAL_UPDATE)
            | bit(s.update_data, UPDATE_DATA);
        for i in 0..8 {
            let mut mask = 0u8;
            for j in 0..8 {
                p.seg_info.feature_data[i][j] = s.segment_feature_data[i][j];
                mask |= u8::from(s.segment_feature_active[i][j]) << j;
            }
            p.seg_info.feature_mask[i] = mask;
        }
    }
    if let Some(g) = grain {
        let f = &mut p.film_grain_info;
        use film_grain_info::*;
        f.film_grain_info_fields = bit(g.apply_grain, APPLY_GRAIN)
            | bit(g.chroma_scaling_from_luma, CHROMA_SCALING_FROM_LUMA)
            | (u32::from(g.grain_scaling.saturating_sub(8) & 3) << GRAIN_SCALING_MINUS_8)
            | (u32::from(g.ar_coeff_lag & 3) << AR_COEFF_LAG)
            | (u32::from(g.ar_coeff_shift.saturating_sub(6) & 3) << AR_COEFF_SHIFT_MINUS_6)
            | (u32::from(g.grain_scale_shift & 3) << GRAIN_SCALE_SHIFT)
            | bit(g.overlap_flag, OVERLAP_FLAG)
            | bit(g.clip_to_restricted_range, CLIP_TO_RESTRICTED_RANGE);
        f.grain_seed = g.grain_seed;
        f.num_y_points = g.num_y_points;
        f.point_y_value = g.point_y_value;
        f.point_y_scaling = g.point_y_scaling;
        f.num_cb_points = g.num_cb_points;
        f.point_cb_value = g.point_cb_value;
        f.point_cb_scaling = g.point_cb_scaling;
        f.num_cr_points = g.num_cr_points;
        f.point_cr_value = g.point_cr_value;
        f.point_cr_scaling = g.point_cr_scaling;
        for (d, s) in f.ar_coeffs_y.iter_mut().zip(g.ar_coeffs_y_plus_128.iter()) {
            *d = (i16::from(*s) - 128) as i8;
        }
        for (d, s) in f
            .ar_coeffs_cb
            .iter_mut()
            .zip(g.ar_coeffs_cb_plus_128.iter())
        {
            *d = (i16::from(*s) - 128) as i8;
        }
        for (d, s) in f
            .ar_coeffs_cr
            .iter_mut()
            .zip(g.ar_coeffs_cr_plus_128.iter())
        {
            *d = (i16::from(*s) - 128) as i8;
        }
        f.cb_mult = g.cb_mult;
        f.cb_luma_mult = g.cb_luma_mult;
        f.cb_offset = g.cb_offset;
        f.cr_mult = g.cr_mult;
        f.cr_luma_mult = g.cr_luma_mult;
        f.cr_offset = g.cr_offset;
    }

    if let Some(ti) = h.tile_info.as_ref() {
        p.tile_cols = ti.tile_cols as u8;
        p.tile_rows = ti.tile_rows as u8;
        let shift = if seq.use_128x128_superblock { 5 } else { 4 };
        for i in 0..(ti.tile_cols as usize).min(63) {
            let sbs = (ti.mi_col_starts[i + 1] - ti.mi_col_starts[i] + (1 << shift) - 1) >> shift;
            p.width_in_sbs_minus_1[i] = (sbs - 1) as u16;
        }
        for i in 0..(ti.tile_rows as usize).min(63) {
            let sbs = (ti.mi_row_starts[i + 1] - ti.mi_row_starts[i] + (1 << shift) - 1) >> shift;
            p.height_in_sbs_minus_1[i] = (sbs - 1) as u16;
        }
        p.context_update_tile_id = ti.context_update_tile_id as u16;
    }
    let inter = h.inter_refs.as_ref();
    {
        use pic_info::*;
        p.pic_info_fields = (u32::from(h.frame_type.as_raw()) << FRAME_TYPE)
            | bit(h.show_frame, SHOW_FRAME)
            | bit(h.showable_frame, SHOWABLE_FRAME)
            | bit(h.error_resilient_mode, ERROR_RESILIENT_MODE)
            | bit(h.disable_cdf_update, DISABLE_CDF_UPDATE)
            | bit(h.allow_screen_content_tools, ALLOW_SCREEN_CONTENT_TOOLS)
            | bit(h.force_integer_mv, FORCE_INTEGER_MV)
            | bit(h.allow_intrabc, ALLOW_INTRABC)
            | bit(fs.use_superres, USE_SUPERRES)
            | bit(
                inter.is_some_and(|r| r.allow_high_precision_mv),
                ALLOW_HIGH_PRECISION_MV,
            )
            | bit(
                inter.is_some_and(|r| r.is_motion_mode_switchable),
                IS_MOTION_MODE_SWITCHABLE,
            )
            | bit(
                inter.is_some_and(|r| r.use_ref_frame_mvs),
                USE_REF_FRAME_MVS,
            )
            | bit(h.disable_frame_end_update_cdf, DISABLE_FRAME_END_UPDATE_CDF)
            | bit(
                h.tile_info
                    .as_ref()
                    .is_some_and(|t| t.uniform_tile_spacing_flag),
                UNIFORM_TILE_SPACING_FLAG,
            )
            | bit(h.allow_warped_motion.unwrap_or(false), ALLOW_WARPED_MOTION);
    }
    p.superres_scale_denominator = fs.superres_denom as u8;
    p.interp_filter = match inter.map(|r| r.interpolation_filter) {
        Some(InterpolationFilter::EighttapSmooth) => 1,
        Some(InterpolationFilter::EighttapSharp) => 2,
        Some(InterpolationFilter::Bilinear) => 3,
        Some(InterpolationFilter::Switchable) => 4,
        _ => 0,
    };
    if let Some(lf) = h.loop_filter_params.as_ref() {
        p.filter_level = [lf.loop_filter_level[0], lf.loop_filter_level[1]];
        p.filter_level_u = lf.loop_filter_level[2];
        p.filter_level_v = lf.loop_filter_level[3];
        use loop_filter_info::*;
        p.loop_filter_info_fields = ((lf.loop_filter_sharpness & 7) << SHARPNESS_LEVEL)
            | (u8::from(lf.loop_filter_delta_enabled) << MODE_REF_DELTA_ENABLED)
            | (u8::from(lf.loop_filter_delta_update) << MODE_REF_DELTA_UPDATE);
        p.ref_deltas = lf.loop_filter_ref_deltas;
        p.mode_deltas = lf.loop_filter_mode_deltas;
    } else {
        p.ref_deltas = oxideav_av1::LOOP_FILTER_REF_DELTAS_DEFAULT;
    }
    if let Some(q) = h.quantization_params.as_ref() {
        p.base_qindex = q.base_q_idx;
        p.y_dc_delta_q = q.delta_q_y_dc;
        p.u_dc_delta_q = q.delta_q_u_dc;
        p.u_ac_delta_q = q.delta_q_u_ac;
        p.v_dc_delta_q = q.delta_q_v_dc;
        p.v_ac_delta_q = q.delta_q_v_ac;
        use qmatrix::*;
        p.qmatrix_fields = (u16::from(q.using_qmatrix) << USING_QMATRIX)
            | (u16::from(q.qm_y & 15) << QM_Y)
            | (u16::from(q.qm_u & 15) << QM_U)
            | (u16::from(q.qm_v & 15) << QM_V);
    }
    {
        use mode_control::*;
        let dq = h.delta_q_params.as_ref();
        let dlf = h.delta_lf_params.as_ref();
        let tx = match h.tx_mode {
            Some(TxMode::Only4x4) | None => 0,
            Some(TxMode::TxModeLargest) => 1,
            Some(TxMode::TxModeSelect) => 2,
        };
        p.mode_control_fields = bit(dq.is_some_and(|d| d.delta_q_present), DELTA_Q_PRESENT_FLAG)
            | (u32::from(dq.map_or(0, |d| d.delta_q_res) & 3) << LOG2_DELTA_Q_RES)
            | bit(
                dlf.is_some_and(|d| d.delta_lf_present),
                DELTA_LF_PRESENT_FLAG,
            )
            | (u32::from(dlf.map_or(0, |d| d.delta_lf_res) & 3) << LOG2_DELTA_LF_RES)
            | bit(dlf.is_some_and(|d| d.delta_lf_multi), DELTA_LF_MULTI)
            | (tx << TX_MODE)
            | bit(h.reference_select.unwrap_or(false), REFERENCE_SELECT)
            | bit(h.reduced_tx_set.unwrap_or(false), REDUCED_TX_SET_USED)
            | bit(h.skip_mode_present.unwrap_or(false), SKIP_MODE_PRESENT);
    }
    if let Some(cd) = h.cdef_params.as_ref() {
        p.cdef_damping_minus_3 = cd.cdef_damping.saturating_sub(3);
        p.cdef_bits = cd.cdef_bits;
        // The coded secondary strength (the §5.9.19 3 => 4 step undone).
        let coded = |s: u8| if s == 4 { 3 } else { s & 3 };
        for i in 0..8 {
            p.cdef_y_strengths[i] =
                (cd.cdef_y_pri_strength[i] << 2) | coded(cd.cdef_y_sec_strength[i]);
            p.cdef_uv_strengths[i] =
                (cd.cdef_uv_pri_strength[i] << 2) | coded(cd.cdef_uv_sec_strength[i]);
        }
    }
    if let Some(lr) = h.lr_params.as_ref() {
        use loop_restoration::*;
        let t = |r: FrameRestorationType| r as u16 & 3;
        p.loop_restoration_fields = (t(lr.frame_restoration_type[0]) << YFRAME_RESTORATION_TYPE)
            | (t(lr.frame_restoration_type[1]) << CBFRAME_RESTORATION_TYPE)
            | (t(lr.frame_restoration_type[2]) << CRFRAME_RESTORATION_TYPE)
            | (u16::from(lr.lr_unit_shift & 3) << LR_UNIT_SHIFT)
            | (u16::from(lr.lr_uv_shift & 1) << LR_UV_SHIFT);
    }
    for (k, wm) in p.wm.iter_mut().enumerate() {
        wm.wmmat[2] = 1 << 16;
        wm.wmmat[5] = 1 << 16;
        let Some(g) = h.global_motion_params.as_ref() else {
            continue;
        };
        let r = k + 1;
        let ty = g.gm_type[r];
        wm.wmtype = ty as u32;
        wm.wmmat[..6].copy_from_slice(&g.gm_params[r]);
        wm.invalid = match ty {
            WarpModelType::RotZoom | WarpModelType::Affine => {
                u8::from(!setup_shear(g.gm_params[r]).is_some_and(|s| s.warp_valid))
            }
            _ => 0,
        };
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdef_strengths_pack_the_coded_secondary() {
        let coded = |s: u8| if s == 4 { 3 } else { s & 3 };
        assert_eq!([0, 1, 2, 4].map(coded), [0, 1, 2, 3]);
    }

    #[test]
    fn tile_sizes_are_little_endian_minus_one() {
        let mut bits = BitCursor {
            data: &[0b1010_0000],
            at: 0,
        };
        assert_eq!(bits.read(3), Some(0b101));
        assert_eq!(bits.at, 3);
    }
}
