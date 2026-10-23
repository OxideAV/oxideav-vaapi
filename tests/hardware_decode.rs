//! Whole-stream decoding on the GPU: each fixture (8 frames at 128x72,
//! H.264 and HEVC with B-frames, VP9, AV1) decodes to the frames a
//! software decoder gives (MD5 of each NV12 frame in output order, in
//! `tests/fixtures/*.md5`). Skipped on hosts without a VA-API driver for
//! the codec.
#![cfg(all(
    target_os = "linux",
    feature = "system-loader",
    feature = "hevc",
    feature = "h264",
    feature = "vp9",
    feature = "av1"
))]

use std::path::Path;

use md5::{Digest, Md5};
use oxideav_vaapi::display::Display;
use oxideav_vaapi::session::Planes;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .expect("fixture")
}

fn expected(name: &str) -> Vec<String> {
    String::from_utf8(fixture(name))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A render node whose driver decodes `profile`, if any.
fn display(profile: i32) -> Option<Display> {
    (128..136).find_map(|n| {
        let d = Display::open_drm(Path::new(&format!("/dev/dri/renderD{n}"))).ok()?;
        d.is_supported(
            oxideav_vaapi::display::VaProfile(profile),
            oxideav_vaapi::sys::entrypoint::VAEntrypointVLD,
        )
        .then_some(d)
    })
}

fn annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut e = starts.get(k + 1).map_or(data.len(), |n| n - 3);
            while e > s && data[e - 1] == 0 {
                e -= 1;
            }
            &data[s..e]
        })
        .collect()
}

fn ivf(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = u16::from_le_bytes([data[6], data[7]]) as usize;
    while at + 12 <= data.len() {
        let n = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        out.push(&data[at + 12..at + 12 + n]);
        at += 12 + n;
    }
    out
}

fn digest(width: u32, height: u32, read: impl FnOnce(&mut Planes<'_>)) -> String {
    let (w, h) = (width as usize, height as usize);
    let mut buf = vec![0u8; w * h + ((w + 1) & !1) * h.div_ceil(2)];
    let (luma, chroma) = buf.split_at_mut(w * h);
    let mut planes = Planes {
        luma,
        luma_stride: w,
        chroma,
        chroma_stride: (w + 1) & !1,
    };
    read(&mut planes);
    Md5::digest(&buf)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn h264() {
    use oxideav_vaapi::h264::H264Decoder;
    let Some(display) = display(oxideav_vaapi::sys::profile::VAProfileH264High) else {
        return;
    };
    let data = fixture("h264.h264");
    let mut dec = H264Decoder::new(display);
    let mut frames = Vec::new();
    let mut pictures = Vec::new();
    for unit in annexb(&data) {
        pictures.extend(dec.decode([unit]).unwrap());
        for p in pictures.drain(..) {
            frames.push((
                p.poc,
                digest(p.crop.2, p.crop.3, |planes| dec.read(&p, planes).unwrap()),
            ));
        }
    }
    for p in dec.flush().unwrap() {
        frames.push((
            p.poc,
            digest(p.crop.2, p.crop.3, |planes| dec.read(&p, planes).unwrap()),
        ));
    }
    frames.sort_by_key(|f| f.0);
    assert_eq!(
        frames.into_iter().map(|f| f.1).collect::<Vec<_>>(),
        expected("h264.md5")
    );
}

#[test]
fn hevc() {
    use oxideav_vaapi::hevc::HevcDecoder;
    let Some(display) = display(oxideav_vaapi::sys::profile::VAProfileHEVCMain) else {
        return;
    };
    let data = fixture("hevc.hevc");
    let mut dec = HevcDecoder::new(display);
    let mut frames = Vec::new();
    let mut pictures = Vec::new();
    for unit in annexb(&data) {
        pictures.extend(dec.decode([unit]).unwrap());
        for p in pictures.drain(..) {
            if p.output {
                let (_, _, w, h) = p.crop;
                frames.push((p.poc, digest(w, h, |planes| dec.read(&p, planes).unwrap())));
            }
        }
    }
    for p in dec.flush().unwrap() {
        let (_, _, w, h) = p.crop;
        frames.push((p.poc, digest(w, h, |planes| dec.read(&p, planes).unwrap())));
    }
    frames.sort_by_key(|f| f.0);
    assert_eq!(
        frames.into_iter().map(|f| f.1).collect::<Vec<_>>(),
        expected("hevc.md5")
    );
}

#[test]
fn vp9() {
    use oxideav_vaapi::vp9::Vp9Decoder;
    let Some(display) = display(oxideav_vaapi::sys::profile::VAProfileVP9Profile0) else {
        return;
    };
    let data = fixture("vp9.ivf");
    let mut dec = Vp9Decoder::new(display);
    let mut frames = Vec::new();
    for sample in ivf(&data) {
        if let Some(p) = dec.decode(sample).unwrap() {
            frames.push(digest(p.width, p.height, |planes| {
                dec.read(&p, planes).unwrap()
            }));
        }
    }
    assert_eq!(frames, expected("vp9.md5"));
}

#[test]
fn av1() {
    use oxideav_vaapi::av1::Av1Decoder;
    let Some(display) = display(oxideav_vaapi::sys::profile::VAProfileAV1Profile0) else {
        return;
    };
    let data = fixture("av1.ivf");
    let mut dec = Av1Decoder::new(display);
    let mut frames = Vec::new();
    for sample in ivf(&data) {
        if let Some(p) = dec.decode(sample).unwrap() {
            frames.push(digest(p.width, p.height, |planes| {
                dec.read(&p, planes).unwrap()
            }));
        }
    }
    assert_eq!(frames, expected("av1.md5"));
}
