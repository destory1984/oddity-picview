//! Write a rotation into the file.
//!
//! JPEG: only the EXIF Orientation tag changes, so the picture is not
//! re-compressed and loses no quality. Other formats are decoded, rotated and
//! saved again (PNG/BMP/TIFF/WebP are lossless).

use image::metadata::Orientation;
use image::{DynamicImage, Rgba, RgbaImage};
use std::path::Path;

/// Rotate the file at `path` by `quarter_cw` quarter turns clockwise.
pub fn save_rotation(path: &Path, quarter_cw: i32) -> Result<(), String> {
    let q = quarter_cw.rem_euclid(4);
    if q == 0 {
        return Ok(());
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let cur = crate::loader::exif_orientation_value(&bytes).unwrap_or(1);
        let out = jpeg_set_orientation(&bytes, compose(cur, q))?;
        return write_atomic(path, &out);
    }
    if matches!(ext.as_str(), "gif" | "ico") {
        return Err("GIF, ICO는 파일 회전을 지원하지 않습니다 (보기만 돌립니다)".into());
    }
    let fmt = image::ImageFormat::from_path(path).map_err(|e| e.to_string())?;
    let mut img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    for _ in 0..q {
        img = img.rotate90();
    }
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, fmt).map_err(|e| e.to_string())?;
    write_atomic(path, &out.into_inner())
}

/// Write next to the target, then rename over it, so a failure never leaves a broken file.
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("picview-{}.tmp", std::process::id()));
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// EXIF orientation that shows the picture as orientation `cur` followed by `q` clockwise turns.
/// Found by trying all 8 values on a tiny image with distinct pixels.
pub fn compose(cur: u8, q: i32) -> u8 {
    let base = RgbaImage::from_fn(2, 3, |x, y| Rgba([x as u8, y as u8, 0, 255]));
    let apply = |n: u8| {
        let mut i = DynamicImage::ImageRgba8(base.clone());
        if let Some(o) = Orientation::from_exif(n) {
            i.apply_orientation(o);
        }
        i
    };
    let mut want = apply(cur);
    for _ in 0..q.rem_euclid(4) {
        want = want.rotate90();
    }
    (1..=8).find(|&n| apply(n) == want).unwrap_or(1)
}

// ---------------------------------------------------------------- JPEG EXIF

struct Endian(bool); // true = big endian ("MM")

impl Endian {
    fn u16(&self, b: &[u8], at: usize) -> Option<u16> {
        let s = b.get(at..at + 2)?;
        Some(if self.0 { u16::from_be_bytes([s[0], s[1]]) } else { u16::from_le_bytes([s[0], s[1]]) })
    }
    fn u32(&self, b: &[u8], at: usize) -> Option<u32> {
        let s = b.get(at..at + 4)?;
        let a = [s[0], s[1], s[2], s[3]];
        Some(if self.0 { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) })
    }
    fn put16(&self, v: u16) -> [u8; 2] {
        if self.0 { v.to_be_bytes() } else { v.to_le_bytes() }
    }
    fn put32(&self, v: u32) -> [u8; 4] {
        if self.0 { v.to_be_bytes() } else { v.to_le_bytes() }
    }
}

const TAG_ORIENTATION: u16 = 0x0112;

fn orientation_entry(e: &Endian, value: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(12);
    v.extend(e.put16(TAG_ORIENTATION));
    v.extend(e.put16(3)); // SHORT
    v.extend(e.put32(1));
    v.extend(e.put16(value as u16));
    v.extend([0, 0]);
    v
}

fn app1(tiff: &[u8]) -> Result<Vec<u8>, String> {
    let len = 2 + 6 + tiff.len();
    if len > 0xFFFF {
        return Err("EXIF가 너무 커서 고칠 수 없습니다".into());
    }
    let mut seg = vec![0xFF, 0xE1];
    seg.extend((len as u16).to_be_bytes());
    seg.extend(b"Exif\0\0");
    seg.extend(tiff);
    Ok(seg)
}

/// Return the TIFF block with IFD0's Orientation set to `value`.
fn tiff_set_orientation(tiff: &[u8], value: u8) -> Result<Vec<u8>, String> {
    let bad = || "EXIF 형식이 올바르지 않습니다".to_string();
    let e = match tiff.get(0..2) {
        Some(b"MM") => Endian(true),
        Some(b"II") => Endian(false),
        _ => return Err(bad()),
    };
    let ifd0 = e.u32(tiff, 4).ok_or_else(bad)? as usize;
    let count = e.u16(tiff, ifd0).ok_or_else(bad)? as usize;
    let mut out = tiff.to_vec();
    let mut entries: Vec<Vec<u8>> = Vec::with_capacity(count + 1);
    for k in 0..count {
        let at = ifd0 + 2 + 12 * k;
        let entry = tiff.get(at..at + 12).ok_or_else(bad)?;
        if e.u16(tiff, at) == Some(TAG_ORIENTATION) {
            // tag exists: rewrite its value in place
            out[at..at + 12].copy_from_slice(&orientation_entry(&e, value));
            return Ok(out);
        }
        entries.push(entry.to_vec());
    }
    // tag missing: copy IFD0 plus the new entry to the end of the block. Other
    // data stays where it is, so every offset in the file stays valid.
    let next = e.u32(tiff, ifd0 + 2 + 12 * count).ok_or_else(bad)?;
    let pos = entries.iter().position(|en| e.u16(en, 0).unwrap_or(0) > TAG_ORIENTATION).unwrap_or(entries.len());
    entries.insert(pos, orientation_entry(&e, value));
    if out.len() % 2 == 1 {
        out.push(0);
    }
    let new_ifd0 = out.len() as u32;
    out.extend(e.put16(entries.len() as u16));
    for en in &entries {
        out.extend(en);
    }
    out.extend(e.put32(next));
    out[4..8].copy_from_slice(&e.put32(new_ifd0));
    Ok(out)
}

pub fn jpeg_set_orientation(jpg: &[u8], value: u8) -> Result<Vec<u8>, String> {
    let bad = || "JPEG 구조를 읽을 수 없습니다".to_string();
    if jpg.get(0..2) != Some(&[0xFF, 0xD8]) {
        return Err(bad());
    }
    let mut pos = 2;
    let mut insert_at = 2;
    loop {
        if jpg.get(pos) != Some(&0xFF) {
            return Err(bad());
        }
        let marker = *jpg.get(pos + 1).ok_or_else(bad)?;
        if marker == 0xFF {
            pos += 1; // fill byte
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            break; // image data starts: no EXIF found before it
        }
        let len = u16::from_be_bytes([*jpg.get(pos + 2).ok_or_else(bad)?, *jpg.get(pos + 3).ok_or_else(bad)?]) as usize;
        let end = pos + 2 + len;
        let data = jpg.get(pos + 4..end).ok_or_else(bad)?;
        if marker == 0xE1 && data.starts_with(b"Exif\0\0") {
            let tiff = tiff_set_orientation(&data[6..], value)?;
            let mut out = Vec::with_capacity(jpg.len() + 32);
            out.extend(&jpg[..pos]);
            out.extend(app1(&tiff)?);
            out.extend(&jpg[end..]);
            return Ok(out);
        }
        if marker == 0xE0 {
            insert_at = end; // keep JFIF APP0 first
        }
        pos = end;
    }
    // no EXIF at all: add a minimal one
    let e = Endian(true);
    let mut tiff = b"MM\0\x2A".to_vec();
    tiff.extend(e.put32(8));
    tiff.extend(e.put16(1));
    tiff.extend(orientation_entry(&e, value));
    tiff.extend(e.put32(0));
    let mut out = Vec::with_capacity(jpg.len() + 64);
    out.extend(&jpg[..insert_at]);
    out.extend(app1(&tiff)?);
    out.extend(&jpg[insert_at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::exif_orientation_value;

    fn sample_jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_fn(16, 8, |x, _| image::Rgb([x as u8 * 16, 0, 0]));
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90)
            .encode_image(&img)
            .unwrap();
        buf
    }

    #[test]
    fn compose_rotations() {
        assert_eq!(compose(1, 1), 6);
        assert_eq!(compose(6, 1), 3);
        assert_eq!(compose(3, 1), 8);
        assert_eq!(compose(8, 1), 1);
        assert_eq!(compose(1, -1), 8);
        assert_eq!(compose(6, 3), 1);
        // mirrored orientations stay mirrored
        assert!(matches!(compose(2, 1), 5 | 7));
    }

    #[test]
    fn jpeg_without_exif_gets_tag_and_stays_decodable() {
        let jpg = sample_jpeg();
        assert_eq!(exif_orientation_value(&jpg), None);
        let out = jpeg_set_orientation(&jpg, 6).unwrap();
        assert_eq!(exif_orientation_value(&out), Some(6));
        let out = jpeg_set_orientation(&out, 3).unwrap(); // in-place rewrite
        assert_eq!(exif_orientation_value(&out), Some(3));
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!((img.width(), img.height()), (16, 8), "pixels untouched");
    }

    #[test]
    fn jpeg_exif_without_orientation_keeps_other_tags() {
        // EXIF with only Make = "abc" (ASCII, 4 bytes inline)
        let e = Endian(false);
        let mut tiff = b"II\x2A\0".to_vec();
        tiff.extend(e.put32(8));
        tiff.extend(e.put16(1));
        tiff.extend(e.put16(0x010F));
        tiff.extend(e.put16(2));
        tiff.extend(e.put32(4));
        tiff.extend(b"abc\0");
        tiff.extend(e.put32(0));
        let jpg = sample_jpeg();
        let mut with_exif = jpg[..2].to_vec();
        with_exif.extend(app1(&tiff).unwrap());
        with_exif.extend(&jpg[2..]);

        let out = jpeg_set_orientation(&with_exif, 8).unwrap();
        assert_eq!(exif_orientation_value(&out), Some(8));
        let ex = exif::Reader::new().read_from_container(&mut std::io::Cursor::new(&out)).unwrap();
        let make = ex.get_field(exif::Tag::Make, exif::In::PRIMARY).unwrap();
        assert_eq!(make.display_value().to_string(), "\"abc\"");
        assert!(image::load_from_memory(&out).is_ok());
    }

    #[test]
    fn png_file_is_rotated() {
        let dir = std::env::temp_dir().join(format!("picview-rot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.png");
        let img = RgbaImage::from_fn(4, 2, |x, y| Rgba([x as u8, y as u8, 7, 255]));
        img.save(&p).unwrap();
        save_rotation(&p, 1).unwrap();
        let r = image::open(&p).unwrap().to_rgba8();
        assert_eq!(r.dimensions(), (2, 4));
        assert_eq!(r, image::imageops::rotate90(&img));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
