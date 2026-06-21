# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.3](https://github.com/OxideAV/oxideav-vaapi/compare/v0.0.2...v0.0.3) - 2026-06-21

### Fixed

- correct VA-API test assumptions (surface-id sentinel + VAProfileNone)

### Other

- refresh to current status, drop per-round changelog cruft
- round 10: codec_id_for_profile — reverse lookup over the family table
- round 9: drop unused EntrypointMatrix import in test
- round 9: EntrypointMatrix — one-shot (profile, [entrypoints]) snapshot
- lift codec id → VAProfile family map into public module

### Added — Round 10 (reverse lookup: `codec_id_for_profile`)

Complements the round-8 forward map (`codec_profiles(id) -> &[i32]`)
with the reverse direction: given a raw `VAProfile` value (or a typed
[`VaProfile`]), return the codec id of the family it belongs to.

This is the primitive callers need when iterating an
[`EntrypointMatrix`]'s advertised profile list and bucketing each
profile by codec — e.g. building a `(codec, [profile])` index for the
HEVC / VP9 / AV1 adapters planned in the README roadmap. Without it,
each row has to re-scan `KNOWN_CODECS` by hand; the helper lifts the
scan into the table module so callers stay single-line.

- New `profiles::codec_id_for_profile(raw: i32) -> Option<&'static str>`
  — O(KNOWN_CODECS · max_family_len) scan; returns the codec id of
  the row that contains `raw`, or `None` for unmapped values
  (`VAProfileNone`, future/vendor-specific values).
- New `profiles::codec_id_for_va_profile(VaProfile) -> Option<&'static str>`
  — typed wrapper around the raw variant; same semantics, drops a
  `.raw()` at the call site.
- Both helpers re-exported at the crate root.
- 6 new unit tests in `src/profiles.rs` covering: H.264 / HEVC / VP9
  / AV1 family resolution, `None` for `VAProfileNone` and out-of-range
  values, typed-vs-raw parity across every table entry, and the
  forward-reverse round-trip (every profile in the table maps back to
  a codec id whose `codec_profiles` slice contains it).
- New `tests/round10_codec_id_lookup.rs` (6 tests, skip-friendly on
  hosts with no render node):
  - `reverse_lookup_covers_every_table_entry` — pure table sweep.
  - `reverse_lookup_returns_none_for_va_profile_none` /
    `_for_unknown_value` — sentinel + range guards.
  - `typed_and_raw_variants_agree_for_every_table_entry`.
  - `matrix_advertised_profiles_bucket_by_codec_without_panic` /
    `matrix_codec_id_lookup_matches_table_for_recognised_profiles`
    — driver-touching parity checks against a real
    `EntrypointMatrix`.
  - `reverse_lookup_zero_collision_across_families` — guard against
    two rows accidentally claiming the same profile.

### Added — Round 9 (`EntrypointMatrix` — one-shot `(profile, [entrypoints])` snapshot)

Lifts the `(profile, entrypoint)` membership check out of
`vaQueryConfigEntrypoints` FFI traffic and into an in-memory snapshot
that callers consult freely.  The naive shape — repeatedly calling
`Display::is_supported(profile, entrypoint)` for each codec family
and each entrypoint we care about — issued one
`vaQueryConfigEntrypoints` per pair.  On a 25-profile driver with
seven codec families this came out to roughly fifty round-trips per
`engine_info()` invocation.  With the matrix it's roughly twenty-five
(one per advertised profile) plus zero per query.

- New `display::EntrypointMatrix` type — a `Vec<(VaProfile,
  Vec<i32>)>` wrapped behind a small public API:
  - `profiles() -> impl Iterator<Item = VaProfile>` — advertised
    profile list matching `Display::profiles()` exactly.
  - `len()` / `is_empty()` — trivial accessors.
  - `entrypoints_for(profile) -> &[i32]` — entrypoints advertised for
    a single profile; empty slice if the profile isn't in the matrix.
  - `is_supported(profile, entrypoint) -> bool` — O(rows) membership
    check, equivalent to `Display::is_supported` but no FFI.
  - `profiles_with_entrypoint(entrypoint) -> Vec<VaProfile>` —
    equivalent to `Display::profiles_with_entrypoint`, again FFI-free.
  - `any_supports(&[i32], entrypoint) -> bool` — "any profile in this
    list advertises this entrypoint" — the codec-family check shared
    between `engine::collect_codecs` and `profiles::host_supports_codec_decode`.
- New `Display::entrypoint_matrix() -> Result<EntrypointMatrix, VaError>`
  — single-call constructor that issues `vaQueryConfigProfiles` once,
  then `vaQueryConfigEntrypoints` once per advertised profile.
- `engine::probe_node` now builds the matrix once per device and
  threads it through `collect_codecs` + `max_dims_across`. Both
  functions drop their `Display::is_supported` calls in favour of
  matrix lookups; `max_dims_across` keeps its `vaGetConfigAttributes`
  calls because those answer a different question (per-pair max
  dimensions, not "is the pair advertised").
- `profiles::host_supports_codec_decode` builds and consults the
  matrix internally — saves N-1 FFI calls on multi-profile families
  (h264 = 4, hevc = 6).  New `profiles::host_entrypoint_matrix() ->
  Option<EntrypointMatrix>` factors out the "open `/dev/dri/renderD128`
  + build matrix" plumbing; new `profiles::codec_decode_supported`
  and `profiles::codec_encode_supported` take a `&EntrypointMatrix`
  + codec id so multi-codec pre-flights share one matrix.
- New `tests/round9_entrypoint_matrix.rs` (9 tests, skip-friendly on
  hosts with no render node):
  - Round-trip checks: matrix `profiles()` == `Display::profiles()`
    and matrix `is_supported(VLD)` == `Display::is_supported(VLD)`
    for every advertised profile.
  - Bulk-filter parity: matrix `profiles_with_entrypoint(VLD)` ==
    `Display::profiles_with_entrypoint(VLD)`.
  - Negative paths: `any_supports` over a bogus profile list returns
    false, `entrypoints_for` on an unadvertised profile returns an
    empty slice, `codec_*_supported` over an unknown codec id returns
    false without touching the matrix.
  - Convergence: `codec_decode_supported(&matrix, "h264")` agrees with
    `host_supports_codec_decode("h264")` — the two spellings of the
    same question.
  - Skip-friendly contract: `host_entrypoint_matrix` returns `None`
    on no-render-node hosts without panicking.

### Added — Round 8 (codec-id → VA-API profile family map, shared by `engine.rs` + `register()`)

Lifts the private `CODEC_FAMILIES` table that used to live inside
`engine.rs` into a public `profiles` module so codec adapters (today
just H.264, tomorrow HEVC and VP9) can consult one source of truth
for "which `VAProfile` values does codec id X refer to?".

- New `pub mod profiles` (always compiled — no `registry` gate)
  exposing:
  - `pub const KNOWN_CODECS: &[CodecFamily]` — the codec id →
    `VAProfile` family table. 8 entries: `h264` / `hevc` / `av1` /
    `vp8` / `vp9` / `mpeg2` / `vc1` / `jpeg`. Ordering is ascending
    capability — the last entry is the "headline" profile used for
    `vaGetConfigAttributes(MaxPictureWidth/Height)` queries.
  - `pub fn codec_profiles(codec_id) -> Option<&'static [i32]>` —
    look up the family by id.
  - `pub fn headline_profile(codec_id) -> Option<VaProfile>` — last
    entry of the family list.
  - `pub fn host_supports_codec_decode(codec_id) -> bool` — opens
    `/dev/dri/renderD128`, checks `VAEntrypointVLD` against every
    family profile, returns `false` for unknown ids / sandbox CI
    hosts / drivers that don't accelerate the codec.
- `register()` in `lib.rs` now calls
  `host_supports_codec_decode("h264")` instead of the private
  `host_supports_h264_decode()` helper (which is removed). Same
  semantics; any future codec adapter that needs the same
  pre-flight gets the helper for free.
- `engine.rs::collect_codecs` consumes `KNOWN_CODECS` directly —
  the private `CodecFamily` struct + table are removed in favour
  of the shared one in `profiles`. Net delta in `engine.rs`:
  −60 LOC.
- New `tests/round8_codec_profiles.rs` (6 tests) covers the codec
  id round-trip (`KNOWN_CODECS` ↔ `codec_profiles`), the headline
  profile choice for H.264 / HEVC, the unknown-codec fallback, the
  skip-friendly no-libva path, and (crucially) that every codec id
  surfaced by `engine_info()` is present in `KNOWN_CODECS` — i.e.
  the refactor doesn't drift between the two consumers.
- New `jpeg` row in `KNOWN_CODECS` (one entry:
  `VAProfileJPEGBaseline`) — `engine_info()` will now surface a
  `jpeg` `HwCodecCaps` block on hosts where the driver advertises
  the JPEG baseline profile (e.g. Intel iHD's MFX JPEG decoder).

## [0.0.2](https://github.com/OxideAV/oxideav-vaapi/compare/v0.0.1...v0.0.2) - 2026-05-06

### Other

- dlopen_succeeds + va_get_display_drm_returns_non_null skip on no-libva CI
- apply rustfmt layout + fix always-zero op + identical-if-block lints
- skip frameworks_load + vtable_resolves on hosts without the driver
- honour CodecParameters::device_index — pick matching DRI render node
- cache SPS/PPS across packets — works under bench's slice-only feed
- query vaGetConfigAttributes per device for max width/height
- implement engine_info() — enumerate DRI render nodes + per-codec caps

### Added — Round 7 (`CodecParameters::device_index` plumbing)

`H264VaCodecDecoder::make` (the registered factory) now honours
[`oxideav_core::CodecParameters::device_index`]. Indexing matches
[`engine::engine_info`]'s walk order so `info h264` device-block
indices and `--device` selectors line up — passing
`with_device_index(1)` opens the second device that
`engine_info()` reports, not just "the second `renderD*` node on
disk" (which may not be the same thing on hosts where libva
refuses to bind to one of the nodes).

- New `pub fn engine::device_path_for_index(index: u32) -> Result<PathBuf, VaError>`
  (also re-exported from the crate root). Walks
  `/dev/dri/renderD128..renderD191`, attempts
  [`Display::open_drm`] on each existing path, counts only those
  that initialise cleanly, and returns the path at position
  `index` in that filtered list. Errors with a descriptive
  [`VaError::Init`] when `index` is out of range.
- New `H264VaCodecDecoder::new_with_device_index(codec_id, index)`
  constructor; the existing
  [`H264VaCodecDecoder::new(codec_id)`] now delegates to it with
  `index = 0` for source-compatibility.
- Decoder factory `decoder::h264_decoder_factory` reads
  `params.device_index.unwrap_or(0)` and threads the index
  through. Out-of-range values return
  [`oxideav_core::Error::Unsupported`] rather than silently
  falling back to device 0 — silent fallback would mask
  configuration mistakes.
- New `tests/round7_device_index.rs` (3 tests):
  - `device_index_none_opens_first_working_device` — default
    `CodecParameters` has `device_index = None`; the factory
    accepts it and `device_path_for_index(0)` matches the
    `dri_node` extra entry on `engine_info()[0]`.
  - `device_index_one_opens_second_device` — skips when
    `engine_info()` reports fewer than 2 working devices; on
    multi-device hosts (the dev box now: NVIDIA renderD128 +
    Intel iHD renderD129) confirms the second-position
    device_index resolves to the second device's path and the
    factory builds successfully.
  - `device_index_out_of_range_errors` — runs everywhere;
    `device_index = 99` errors at both the helper and the
    factory layer. Always-on so CI hosts without a working
    VA-API stack still cover the negative path.

### Added — Round 6 (engine probe — DRI render-node enumeration + per-codec caps)

Wires up Phase 1's [`oxideav_core::engine::EngineProbeFn`] contract so
`oxideav list` and any other consumer can ask the VA-API bridge "which
engines do you see, and what can each do?" without spinning up a
decoder.

- New `engine` module gated behind the default-on `registry` feature
  (the [`HwDeviceInfo`] / [`HwCodecCaps`] return types live in
  `oxideav-core`):
  - `pub fn engine_info() -> Vec<HwDeviceInfo>` walks the standard
    DRM render-node range `/dev/dri/renderD128`..`renderD191`,
    skips nodes that don't exist, opens each existing one with
    [`Display::open_drm`], and silently skips nodes whose libva
    driver refuses to load (the standard "node exists but the GPU
    driver doesn't bind" case — on this dev box renderD129 is the
    second NVIDIA GPU's render node where libva falls through to
    iHD/i965 and bails).
  - Per-device entry:
    - `name`: vendor string + render-node basename, so multi-GPU
      hosts producing the same vendor string still get unique
      device names (e.g. `"VA-API NVDEC driver [direct backend]
      (renderD128)"`).
    - `api_version`: `"VA-API <major>.<minor>"` from
      [`Display::api_version`].
    - `extra`: includes `("dri_node", "/dev/dri/renderD128")`.
    - `codecs`: one [`HwCodecCaps`] per codec family (h264 / hevc /
      av1 / vp8 / vp9 / mpeg2 / vc1) with at least one advertised
      profile. Decode/encode flags are any-of-family; max
      width/height come from `vaGetConfigAttributes` on the highest
      advertised profile (fallback `None` when the driver returns
      `VA_ATTRIB_NOT_SUPPORTED` or a sentinel `0`); `profiles`
      lists the [`VaProfile::name`] string for every advertised
      family profile.
- `register()` now chains `.with_engine_id("vaapi")` and
  `.with_engine_probe(engine_info)` onto the H.264 [`CodecInfo`] so
  the CLI can group all VA-API codec entries by engine and call the
  probe at most once per pass.
- New `tests/round6_engine_info.rs`:
  - `engine_info_finds_render_node_or_skips` — dumps the first
    device, asserts non-empty vendor + advertised api_version + an
    h264 entry with `decode = true`. Skip-friendly when no render
    node is present.
  - `engine_info_does_not_panic_when_called_twice` — confirms the
    probe is callable repeatedly (consumers may dedupe by engine id
    and call once per pass, but the contract is "idempotent + safe
    to call multiple times").

### Findings — Round 6

On the dev box (RTX 5080 + nvidia-vaapi-driver 0.0.16 + libva 1.22):

- 2 render nodes exist (`/dev/dri/renderD128`, `renderD129`);
  `engine_info()` reports 1 working device because the nvidia
  driver shim binds to renderD128 only — opening renderD129
  through libva falls through to the iHD/i965 fallbacks and
  returns `VaError::Init`.
- The single device exposes decode-only caps (`encode = false`)
  for every codec family, matching the Round 4 finding that
  nvidia-vaapi-driver is NVDEC-only.
- `MaxPictureWidth` / `MaxPictureHeight` come back as `0` from this
  driver — the engine module treats `0` as `None` (the alternative
  `Some(0)` would be a misleading "max dimension is zero" report
  to consumers like the CLI).

### Added — Round 5 (H.264 decode wall RESOLVED + first registered factory)

The Round 3 H.264 decode silent-fail wall is **gone**. Retrying the
same end-to-end IDR submission flow with the shared, unit-tested
[`oxideav-bitstream`] parser populating the parameter buffers gave a
**pixel-perfect match** against ffmpeg's reference render
(`mean abs diff = 0/255` on the bundled 320×240 fixture, all three
planes). The Round 3 wall was a parser bug, not a driver issue — the
NVDEC backend was always willing to decode; the silent-fail signature
came from at least one mis-packed bitfield in the in-tree parser's
parameter-buffer construction.

- New `oxideav-bitstream = "0.0"` dependency (matching the workspace
  shape used by `oxideav-vdpau`).
- New `decoder` module:
  - `H264VaDecoder<'d>` — single-IDR helper that owns a `Config` +
    `Context` (bound by lifetime to a `Display`) and drives the
    submission flow `vaCreateBuffer × 4 → vaBeginPicture →
    vaRenderPicture → vaEndPicture → vaSyncSurface → vaCreateImage(NV12)
    → vaGetImage`. Returns the decoded frame in I420 layout.
  - `compute_slice_data_bit_offset` — full IDR-I-slice header
    bit-counter that extends the `oxideav-bitstream`'s minimal slice
    header parse with ref_pic_list_modification, dec_ref_pic_marking
    (IDR-flavoured: no_output_of_prior_pics + long_term_reference),
    slice_qp_delta and the deblocking-filter triplet — far enough to
    locate the start of `slice_data()` accurately in the bitstream.
    Required because libva's `slice_data_bit_offset` is the post-EBSP
    bit count from the NAL header byte through the end of
    slice_header() and the `oxideav-bitstream` minimal parser stops at
    `pic_order_cnt_lsb`.
  - `H264VaCodecDecoder` — `oxideav_core::Decoder` implementation,
    `Send` via `unsafe impl` (libva's "not thread-safe" rule is about
    concurrent access; serialized owner moves are sound). Opens its
    own DRM render node, lazily constructs the inner `H264VaDecoder`
    on the first packet, decodes the IDR access unit and emits an
    `oxideav_core::VideoFrame` cropped to display dimensions.
- New `VAIQMatrixBufferH264` struct in `sys.rs` plus a `flat()`
  constructor that returns the all-16 default scaling lists. Verified
  240 bytes via a one-shot `sizeof()` C check against
  `/usr/include/va/va.h`.
- `register()` now wires the H.264 decode factory into the codec
  registry at priority 10 with `hardware_accelerated = true`. On hosts
  where libva loads but no driver `.so` is available for the GPU, the
  pre-flight `host_supports_h264_decode()` check returns false and
  registration is skipped — the SW codec stays the only candidate.
- New `tests/round5_decode.rs` (3 tests):
  - `h264_high_decoder_constructs` — `H264VaDecoder::new` succeeds
    and reports the SPS-derived 320×240 dimensions.
  - `h264_high_idr_decode_succeeds_or_documents_silent_fail` —
    cross-validates against an ffmpeg reference rendered via
    `ffmpeg -f h264 -i pipe:0 -f rawvideo -pix_fmt yuv420p pipe:1`.
    On the dev box this lands the `mad_total = 0.000` success
    branch; the test code retains the silent-fail-asserting branch
    (constant 0x80 luma + 0x80/0x80 chroma) so the same source
    expresses both possible truths and passes against either.
  - `registry_decoder_roundtrips_idr_packet` — feeds the fixture as a
    single `oxideav_core::Packet` to the registry-shaped
    `H264VaCodecDecoder`, pulls a `Frame::Video` back, asserts the
    three-plane shape and per-plane sizes (320×240 Y, 160×120 U/V).
- New `tests/fixtures/h264_high_320x240_1frame.h264` — same
  ffmpeg-generated single-IDR Annex-B as the `oxideav-vdpau` Round 3
  fixture (~6.6 KB).

### Findings — Round 5

The Round 3 wall held only against the *parser* used during that
round. Three concrete shape differences between the in-tree parser
and `oxideav-bitstream`'s parser most likely caused the silent-fail —
without keeping the in-tree parser around for diff we can only point
at categories — but the `seq_fields` / `pic_fields` packing is a
known minefield, the `slice_data_bit_offset` is sensitive to whether
emulation-prevention bytes are stripped before counting bits, and the
`MinLumaBiPredSize8x8 : 1` slot in `seq_fields` is silently load-
bearing on NVDEC even for I-slices. The new code documents the bit
positions explicitly against `va.h`.

The lesson for the wider OxideAV project is the architectural one:
**bitstream parsing belongs in `oxideav-bitstream`** (a tested, shared
crate) and not in each hardware-bridge crate. The Round 3 attempt
predated that crate, and the in-tree parser was never unit-tested
against a reference. With the same parser now driving both `oxideav-
vdpau` and `oxideav-vaapi` end-to-end, both backends now decode the
same fixture bit-exactly.

### Added — Round 4 (capability probing API + driver-reality findings)

- New `Display` capability methods built on `vaQueryConfigEntrypoints`:
  - `Display::entrypoints(VaProfile) -> Result<Vec<i32>, VaError>` —
    list every entrypoint the driver advertises for a given profile.
    Maps `VA_STATUS_ERROR_UNSUPPORTED_PROFILE` to `Ok(Vec::new())` so
    capability audits don't have to special-case `Err`.
  - `Display::is_supported(VaProfile, entrypoint) -> bool` —
    convenience yes/no check; swallows errors as `false` so callers
    don't drown in `Result<bool>` for a question that's structurally
    boolean.
  - `Display::profiles_with_entrypoint(entrypoint) -> Result<Vec<VaProfile>, VaError>`
    — filter the full profile list to those that advertise a given
    entrypoint. Useful for "which codecs can I decode?" /
    "which codecs can I encode?" capability dumps.
- Extended `VaProfile::name()` to identify the wider set of profiles
  the dev box surfaces (MPEG-2 Simple/Main, VC1 Simple/Main/Advanced,
  H.264 ConstrainedBaseline, JPEG Baseline, VP8, VP9 Profile 0/2,
  HEVC Main/Main10/Main12/Main444/Main444_10/Main444_12, AV1
  Profile 0/1).
- Added the matching `VAProfile*` constants in `sys::profile`.
- New integration tests in `tests/round4_capabilities.rs` (5 passing,
  1 `#[ignore]`'d for NVDEC-specific driver-truth):
  - `entrypoints_for_h264_high_includes_vld`
  - `is_supported_recognises_h264_decode`
  - `is_supported_for_unsupported_entrypoint_is_false`
  - `decode_profile_set_is_non_empty`
  - `entrypoints_for_unsupported_profile_handled_gracefully`
  - `encode_unavailable_on_nvdec_backend` (`#[ignore]`'d)
- New `tests/capability_dump.rs` — `--ignored` diagnostic test that
  prints the full `(profile, entrypoint, RTFormat)` matrix and a
  decode/encode summary. Canonical command-line probe for hosts.

### Findings — Round 4 (Path A: VA-API encode is structurally impossible
### on `nvidia-vaapi-driver`)

The `nvidia-vaapi-driver 0.0.16` shim wraps NVDEC, NVIDIA's hardware
**decode** engine. `vaQueryConfigEntrypoints` for **every** advertised
profile (H.264 Main/High/ConstrainedBaseline, HEVC Main/Main10/Main12/
Main444 family, AV1 Profile 0, VP8, VP9 Profile 0/2, VC1 Simple/Main/
Advanced, MPEG-2 Simple/Main) returns the single entrypoint
`VAEntrypointVLD = 1`. No `EncSlice`, no `EncSliceLP`, no
`EncPicture`. `vaCreateConfig` for any `(profile, EncSlice)` pair
fails with `VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT (13)`: *"the
requested VAEntryPoint is not supported"*.

The Round 4 strategy of "if decode silently fails, try encode"
therefore has no path on this hardware via VA-API. NVENC (NVIDIA's
hardware encode engine) is exposed through NVENC-direct (used by the
`oxideav-nvidia` sibling crate) and through CUDA/Vulkan Video; the
VA-API frontend does not bridge to NVENC at all.

The `capability_dump` test confirms this on demand. The
`encode_unavailable_on_nvdec_backend` test asserts it programmatically
(skipped on non-NVDEC hosts).

### Findings — Round 4 (Path B: decode wall remains; not landed this round)

The Round 3 H.264 decode silent-fail (parameter buffers accepted,
surface returns constant 0x80) is unresolved. With encode ruled out
structurally and per the workspace clean-room policy (no consulting
`nvidia-vaapi-driver` source / ffmpeg vaapi.c / gstreamer-vaapi /
third-party Rust VA-API bindings), debugging the silent-fail requires
either:

1. A second VA-API driver on the box (Intel iGPU, AMD GPU, or
   `mesa-va-gallium`) to cross-validate parameter-buffer setups
   against, or
2. Tens of thousands of lines of bitstream parsing per codec for
   HEVC / AV1 / VP9 / VP8, each of which is independently at risk of
   the same silent-fail wall (different NVDEC code paths but the same
   driver dispatch surface).

Neither is achievable in one round without bloating the crate beyond
its bridge mandate. Proper bitstream parsing belongs in
`oxideav-h264` / `oxideav-hevc` / `oxideav-av1` / `oxideav-vp9` /
`oxideav-vp8`, where it's reusable across all hardware backends
(VA-API, NVENC, VDPAU, Vulkan Video, VideoToolbox). When those crates
land their parsers, this crate provides the pipeline glue
(`Config` / `Context` / planned `Surface` / `Buffer` / `Picture`
helpers) — and the capability-probing API added in this round is what
they will use to skip-detect what the driver can actually accelerate.

### Status — Round 4 deliverable

This round ships the capability-probing API, the codified driver-truth
findings, and the diagnostic `capability_dump` infrastructure. **No
codec factories register**: there is nothing on this hardware that
the bridge can usefully wire up via VA-API today, and the framework
already falls back to the pure-Rust path for every codec id. The
crate's value in Round 4 is that capability audits (`oxideav list`,
future codec crates' priority resolution) can now ask "does this
host's VA-API stack accelerate codec X for operation Y?" and get a
correct answer in O(1) calls.

### Added — Round 3 (Tier A: decode pipeline scaffolding)

- New `config` module — safe wrapper around `VAConfigID`.
  - `Config::new(&Display, profile, entrypoint, attribs)` calls
    `vaCreateConfig`. Empty `attribs` slice maps to `(NULL, 0)` so
    the driver picks defaults (e.g. `VA_RT_FORMAT_YUV420` on H.264
    VLD).
  - `Config::supported_attributes()` walks a portable shortlist
    (`RTFormat`, `DecSliceMode`, `MaxPictureWidth`,
    `MaxPictureHeight`, `RateControl`) and returns each value the
    driver advertises (filtered against `VA_ATTRIB_NOT_SUPPORTED`).
  - `Config::get_attribute(type)` reads a single attribute.
  - Standalone `config::supported_attributes` /
    `config::get_attribute` mirror the methods so capability
    probing doesn't require constructing a `Config` first.
  - `Drop` calls `vaDestroyConfig`.
- New `context` module — safe wrapper around `VAContextID` plus
  the render-target surfaces it owns.
  - `Context::new(&Display, &Config, w, h, num_surfaces)` allocates
    `VA_RT_FORMAT_YUV420` surfaces via `vaCreateSurfaces` and binds
    them to a context via `vaCreateContext`. Failures during
    `vaCreateContext` clean up the just-allocated surfaces.
  - `Drop` tears down in reverse order (`vaDestroyContext` →
    `vaDestroySurfaces`).
- `sys.rs` extensions:
  - Resolved into the vtable: `vaQueryConfigEntrypoints`,
    `vaGetConfigAttributes`, `vaDestroyConfig`, `vaDestroyContext`,
    `vaSyncSurface`, `vaDestroyBuffer`, `vaMapBuffer`,
    `vaUnmapBuffer`, `vaDeriveImage`, `vaDestroyImage`, `vaGetImage`,
    `vaPutImage`, `vaCreateImage`, `vaQueryImageFormats`,
    `vaMaxNumImageFormats`.
  - New constants: `VAConfigAttribType` values (`RTFormat`,
    `DecSliceMode`, `MaxPictureWidth`, `MaxPictureHeight`,
    `RateControl`, etc.), `VA_ATTRIB_NOT_SUPPORTED`,
    `VA_RT_FORMAT_YUV420/422/444/420_10`, `VA_FOURCC_NV12 / I420 /
    YV12 / IYUV`, `VABufferType` decode entries
    (`VAPictureParameterBufferType`, `VASliceParameterBufferType`,
    `VASliceDataBufferType`, `VAImageBufferType`),
    `VA_SLICE_DATA_FLAG_*`, `VA_PICTURE_H264_*`,
    `VA_PADDING_LOW/MEDIUM/HIGH`, `VA_INVALID_ID`,
    `VA_INVALID_SURFACE`.
  - New struct types: `VAConfigAttrib`, `VAImageFormat`, `VAImage`,
    `VAPictureH264` (with `::invalid()` helper), and the
    decoder-target buffers `VAPictureParameterBufferH264` and
    `VASliceParameterBufferH264`. All `#[repr(C)]`; sizes verified
    by hand against a C `sizeof()` driver to match
    `/usr/include/va/va.h` exactly (36 / 672 / 3128 / 120 / 48 / 8
    bytes respectively on x86_64).
- New integration tests in `tests/round3_pipeline.rs` (skip-friendly
  on no-driver hosts):
  - `h264_high_decode_config_creates` — `Config::new(&dpy,
    VAProfileH264High, VAEntrypointVLD, &[])` returns Ok with a
    non-zero config id, profile, and entrypoint round-tripping
    through accessors.
  - `h264_high_decode_context_creates` — config + a 1920×1088
    single-surface context create succeeds end-to-end.
  - `h264_high_supports_yuv420_render_target` — query
    `VAConfigAttribRTFormat` for `(H264High, VLD)` and assert the
    returned bitmask includes `VA_RT_FORMAT_YUV420 = 0x01`.

### Status — Round 3 (Tier B: actual decode)

- A first attempt at end-to-end H.264 IDR decode (parse SPS/PPS,
  build `VAPictureParameterBufferH264` + `VASliceParameterBufferH264`,
  submit `vaCreateBuffer × 3` → `vaBeginPicture` → `vaRenderPicture`
  → `vaEndPicture` → `vaSyncSurface` → `vaGetImage`) was implemented
  locally and the submission half works: every libva entry point
  returns `VA_STATUS_SUCCESS` and the surface read-back hits the
  `vaCreateImage` + `vaGetImage` fallback (because
  `nvidia-vaapi-driver` doesn't implement `vaDeriveImage`).
- However on the dev box (RTX 5080 + nvidia-vaapi-driver 0.0.16 +
  libva 1.22) the surface comes back as constant 0x80 luma /
  0x80/0x80 chroma — the driver accepts the parameter buffers but
  the GPU never writes to the surface. The submission flow is
  correct against the libva spec; the issue is most likely a field
  in the H.264-specific parameter buffers that
  `nvidia-vaapi-driver`'s shim handles differently from the way
  `va.h` documents (or a missing field-pic / DPB bookkeeping detail
  for the IDR-only case).
- Per workspace clean-room policy we cannot consult
  `nvidia-vaapi-driver`, ffmpeg's `vaapi.c`, gstreamer-vaapi, or
  third-party Rust bindings to diff our parameter setup against a
  known-good submission. Without a second VA-API driver on the box
  for cross-checking (Intel/AMD), debugging this becomes an
  open-ended bisection exercise across ~60 fields and the
  driver-specific quirks behind the closed-source NVDEC firmware.
- The Tier B scaffolding (NAL splitter, RBSP de-emulation,
  Exp-Golomb `BitReader`, SPS/PPS/slice-header parsers,
  C-bitfield-faithful `seq_fields` / `pic_fields` packing — all
  unit-tested) was removed from the crate before commit so the
  shipped surface only contains code that actually does what its
  docs claim. It will land as part of the H.264 decoder crate
  (`oxideav-h264`) where the parsers belong — at which point this
  crate just needs the pipeline glue to call them.
- Tier C (encode entrypoint) was therefore deliberately not started.

### Added — Round 2

- New `display` module — safe wrapper around the DRM render-node
  libva backend.
  - `Display::open_drm(path)` opens `/dev/dri/renderD*` via
    `libc::open`, calls `vaGetDisplayDRM`, then `vaInitialize`.
    Returns a fully populated `Display` on success or a precise
    error variant on each step's failure.
  - `Display::api_version()`, `Display::vendor_string()`,
    `Display::profiles()`, `Display::raw()`, `Display::fd()`.
  - `Drop` calls `vaTerminate` (only if init succeeded) then
    closes the fd. Init-failure path leaks nothing.
- New `VaError` enum: `OpenDrm(io::Error)`, `GetDisplayNull`,
  `Sys(String)`, `Init { status, message }`, `Va { status, message }`.
  The `Init` variant carries the driver-supplied string from
  `vaErrorStr`, so the no-driver-installed case bubbles up a useful
  reason instead of an opaque code.
- New `VaProfile(i32)` newtype + `VaProfile::name()` for the
  H.264 / HEVC / AV1 profiles oxideav cares about.
- `sys.rs` extensions:
  - `vaQueryVendorString`, `vaMaxNumProfiles` resolved into the
    vtable.
  - `VAProfile*` and `VAEntrypoint*` constants in
    `sys::profile::*` and `sys::entrypoint::*`.
  - Additional `VA_STATUS_ERROR_*` named constants matching the
    cases we route to specific error variants.
  - `sys::error_str(vt, status)` helper that calls `vaErrorStr` and
    returns an owned `String`.
- `libc = "0.2"` added under `target.'cfg(target_os = "linux")'`
  for `open` / `close` of the render node.
- Integration test `tests/round2_init.rs`:
  - `dlopen_succeeds`, `drm_fd_opens`,
    `va_get_display_drm_returns_non_null`,
    `va_initialize_propagates_error_when_no_driver_installed`.
  - The headline test accepts both the success path (real driver
    present, asserts `api_version > (0,0)`, non-empty vendor,
    non-empty profile list) AND the graceful-failure path
    (`Err(VaError::Init { status, message })` with non-zero status
    and non-empty driver message). Same source on both kinds of
    box.

### Added — Round 1 (previously released as 0.0.1)

- Initial scaffolding: `#![cfg(target_os = "linux")]` crate that
  dlopens `libva.so.2` + `libva-drm.so.2` via `libloading` on first
  use.
- `sys.rs` exposes opaque type aliases (`VADisplay`, `VAConfigID`,
  `VAContextID`, `VASurfaceID`, `VABufferID`, `VAStatus`) and a
  resolved `Vtable` covering the bootstrap symbol set:
  `vaInitialize`, `vaTerminate`, `vaErrorStr`, `vaQueryConfigProfiles`,
  `vaCreateConfig`, `vaCreateContext`, `vaCreateSurfaces2`,
  `vaBeginPicture`, `vaRenderPicture`, `vaEndPicture`,
  `vaCreateBuffer`, plus `vaGetDisplayDRM` from the DRM backend.
- Process-wide `OnceLock<Result<Vtable, String>>` cache so the
  dlopen + dlsym round-trip happens at most once per process.
- Unified `register(&mut RuntimeContext)` entry point. Round 1: the
  function confirms the libraries load and returns; no codec
  factories are wired up yet. If load fails (libva not installed,
  sandbox without `/dev/dri`, etc.) the function logs and returns —
  the pure-Rust codec path remains the only resolution candidate.
- Standalone-friendly `registry` feature (default-on) gates the
  `oxideav-core` + `linkme` deps.
- README coverage roadmap and priority explanation.
- Smoke tests: `frameworks_load` and `vtable_resolves` confirm
  symbol resolution on Linux machines that have the VA-API stack
  installed.
