use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct ExifSummary {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub taken: Option<String>,
    pub f_number: Option<String>,
    pub exposure: Option<String>,
    pub iso: Option<String>,
    pub focal: Option<String>,
    pub gps: Option<(f64, f64)>,
    pub all: Vec<(String, String)>,
}

pub fn read_summary(path: &Path) -> Option<ExifSummary> {
    use std::io::{BufReader, Cursor};
    let reader = exif::Reader::new();
    let ex = if let Some((archive, name)) = crate::archive::split(path) {
        let bytes = crate::archive::read_entry(&archive, &name).ok()?;
        reader.read_from_container(&mut BufReader::new(Cursor::new(bytes))).ok()?
    } else {
        reader.read_from_container(&mut BufReader::new(std::fs::File::open(path).ok()?)).ok()?
    };
    summary_from_exif(&ex)
}

fn summary_from_exif(ex: &exif::Exif) -> Option<ExifSummary> {
    use exif::{In, Tag, Value};
    let field = |tag| ex.get_field(tag, In::PRIMARY);
    let string = |tag| {
        let f = field(tag)?;
        match &f.value {
            Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).trim_end_matches('\0').to_string()),
            _ => Some(f.display_value().with_unit(ex).to_string()),
        }.filter(|s| !s.is_empty())
    };
    let number = |tag| match &field(tag)?.value {
        Value::Rational(v) => v.first().filter(|r| r.denom != 0).map(|r| r.to_f64()).filter(|n| n.is_finite()),
        _ => None,
    };
    let coordinate = |tag, reference| match &field(tag)?.value {
        Value::Rational(v) => gps_decimal(v, &string(reference)?),
        _ => None,
    };
    let all: Vec<_> = ex.fields().map(|f| (format!("{} ({})", f.tag, f.ifd_num), f.display_value().with_unit(ex).to_string())).collect();
    if all.is_empty() { return None; }
    Some(ExifSummary {
        make: string(Tag::Make), model: string(Tag::Model), lens: string(Tag::LensModel),
        taken: string(Tag::DateTimeOriginal),
        f_number: number(Tag::FNumber).map(|n| format!("f/{n}")),
        exposure: number(Tag::ExposureTime).map(exposure_text),
        iso: field(Tag::PhotographicSensitivity).and_then(|f| f.value.get_uint(0)).map(|n| format!("ISO {n}")),
        focal: number(Tag::FocalLength).map(|n| format!("{n}mm")),
        gps: coordinate(Tag::GPSLatitude, Tag::GPSLatitudeRef).zip(coordinate(Tag::GPSLongitude, Tag::GPSLongitudeRef)),
        all,
    })
}

pub fn can_strip(path: &Path) -> bool {
    // Reject virtual paths even if the archive has since disappeared.
    if path.ancestors().skip(1).any(|p| crate::archive::is_archive(p) && !p.is_dir()) { return false; }
    matches!(image::ImageFormat::from_path(path), Ok(image::ImageFormat::Jpeg | image::ImageFormat::Png | image::ImageFormat::WebP)) && !path.is_dir()
}

pub fn strip_private(bytes: &[u8], kind: image::ImageFormat) -> Result<Vec<u8>, String> {
    match kind {
        image::ImageFormat::Jpeg => strip_jpeg(bytes),
        image::ImageFormat::Png => strip_png(bytes),
        image::ImageFormat::WebP => strip_webp(bytes),
        _ => Err("개인 정보 지우기를 지원하지 않는 형식입니다".into()),
    }
}

fn orientation(bytes: &[u8]) -> Option<u8> {
    crate::loader::exif_orientation_value(bytes).filter(|v| (2..=8).contains(v))
}

pub fn strip_file(path: &Path) -> Result<bool, String> {
    if !can_strip(path) { return Err("개인 정보 지우기를 지원하지 않는 파일입니다".into()); }
    let kind = image::ImageFormat::from_path(path).map_err(|e| e.to_string())?;
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let out = strip_private(&bytes, kind)?;
    let lost_orientation = kind == image::ImageFormat::WebP && orientation(&bytes).is_some() && orientation(&out).is_none();
    crate::rotate::write_atomic(path, &out)?;
    Ok(lost_orientation)
}

fn exposure_text(seconds: f64) -> String {
    if seconds > 0.0 && seconds < 1.0 {
        let denominator = (1.0 / seconds).round();
        if (seconds * denominator - 1.0).abs() < 0.001 { return format!("1/{denominator}초"); }
    }
    format!("{seconds}초")
}

fn gps_decimal(dms: &[exif::Rational], reference: &str) -> Option<f64> {
    if dms.len() != 3 || dms.iter().any(|r| r.denom == 0) { return None; }
    let n = dms[0].to_f64() + dms[1].to_f64() / 60.0 + dms[2].to_f64() / 3600.0;
    match reference {
        "N" | "E" => Some(n), "S" | "W" => Some(-n), _ => None,
    }
}

pub fn strip_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "JPEG 구조를 읽을 수 없습니다".to_string();
    if !bytes.starts_with(&[255, 0xd8]) { return Err(bad()); }
    let mut out = bytes[..2].to_vec();
    let mut pos = 2;
    loop {
        let start = pos;
        if bytes.get(pos) != Some(&255) { return Err(bad()); }
        while bytes.get(pos) == Some(&255) { pos += 1; }
        let marker = *bytes.get(pos).ok_or_else(bad)?;
        pos += 1;
        if marker == 0xda || marker == 0xd9 {
            out.extend(&bytes[start..]);
            break;
        }
        if marker == 0 || marker == 0xd8 { return Err(bad()); }
        if marker == 1 || (0xd0..=0xd7).contains(&marker) {
            out.extend(&bytes[start..pos]);
            continue;
        }
        let len = u16::from_be_bytes(bytes.get(pos..pos + 2).ok_or_else(bad)?.try_into().unwrap()) as usize;
        if len < 2 { return Err(bad()); }
        let end = pos.checked_add(len).ok_or_else(bad)?;
        let data = bytes.get(pos + 2..end).ok_or_else(bad)?;
        let private = marker == 0xed || marker == 0xfe || (marker == 0xe1 && [
            b"Exif\0\0".as_slice(), b"http://ns.adobe.com/xap/1.0/\0", b"http://ns.adobe.com/xmp/extension/\0",
        ].iter().any(|prefix| data.starts_with(prefix)));
        if !private { out.extend(&bytes[start..end]); }
        pos = end;
    }
    match orientation(bytes) {
        Some(value) => crate::rotate::jpeg_set_orientation(&out, value),
        None => Ok(out),
    }
}

fn orientation_tiff(value: u8) -> Vec<u8> {
    let mut t = b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0".to_vec();
    t.push(value);
    t.extend([0; 6]);
    t
}

fn png_chunks(b: &[u8]) -> Result<Vec<(&[u8], &[u8])>, String> {
    let bad = || "PNG 구조를 읽을 수 없습니다".to_string();
    if !b.starts_with(b"\x89PNG\r\n\x1a\n") { return Err(bad()); }
    let mut pos = 8usize;
    let mut chunks = Vec::new();
    while pos < b.len() {
        let len = u32::from_be_bytes(b.get(pos..pos + 4).ok_or_else(bad)?.try_into().unwrap()) as usize;
        let end = pos.checked_add(12).and_then(|n| n.checked_add(len)).ok_or_else(bad)?;
        let raw = b.get(pos..end).ok_or_else(bad)?;
        let kind = &raw[4..8];
        if chunks.is_empty() && (kind != b"IHDR" || len != 13) { return Err(bad()); }
        chunks.push((kind, raw));
        pos = end;
        if kind == b"IEND" {
            return if len == 0 && pos == b.len() { Ok(chunks) } else { Err(bad()) };
        }
    }
    Err(bad())
}

pub fn strip_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let chunks = png_chunks(bytes)?;
    let mut out = bytes[..8].to_vec();
    for (kind, raw) in chunks {
        if [b"eXIf", b"tEXt", b"iTXt", b"zTXt"].iter().any(|k| kind == *k) { continue; }
        out.extend(raw);
        if kind == b"IHDR" {
            if let Some(value) = orientation(bytes) { out.extend(png_chunk(b"eXIf", &orientation_tiff(value))); }
        }
    }
    Ok(out)
}

pub fn strip_webp(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "WebP 구조를 읽을 수 없습니다".to_string();
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") { return Err(bad()); }
    let size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as u64;
    if size + 8 != bytes.len() as u64 { return Err(bad()); }
    let mut out = bytes[..12].to_vec();
    let mut pos = 12usize;
    let mut vp8x = None;
    while pos < bytes.len() {
        let header = bytes.get(pos..pos + 8).ok_or_else(bad)?;
        let kind = &header[..4];
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let end = pos.checked_add(8).and_then(|n| n.checked_add(len)).and_then(|n| n.checked_add(len % 2)).ok_or_else(bad)?;
        let raw = bytes.get(pos..end).ok_or_else(bad)?;
        if kind != b"EXIF" && kind != b"XMP " {
            if kind == b"VP8X" {
                if len != 10 || vp8x.is_some() { return Err(bad()); }
                vp8x = Some(out.len() + 8);
            }
            out.extend(raw);
        }
        pos = end;
    }
    if let Some(at) = vp8x {
        out[at] &= !0x0c;
        if let Some(value) = orientation(bytes) {
            out.extend(webp_chunk(b"EXIF", &orientation_tiff(value)));
            out[at] |= 8;
        }
    }
    let size = u32::try_from(out.len() - 8).map_err(|_| bad())?;
    out[4..8].copy_from_slice(&size.to_le_bytes());
    Ok(out)
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(data);
    let mut crc = !0u32;
    for &b in &out[4..] {
        crc ^= b as u32;
        for _ in 0..8 { crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1)); }
    }
    out.extend((!crc).to_be_bytes());
    out
}

fn webp_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = kind.to_vec();
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    if data.len() % 2 != 0 { out.push(0); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageFormat::{Jpeg, Png, WebP};
    use std::io::Cursor;

    fn image_bytes(kind: image::ImageFormat) -> Vec<u8> {
        let image = image::RgbImage::from_fn(400, 300, |x, y| image::Rgb([x as u8, y as u8, 100]));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, kind).unwrap();
        out.into_inner()
    }

    fn rational(n: u32, d: u32) -> Vec<u8> { [n.to_le_bytes(), d.to_le_bytes()].concat() }

    fn ifd(t: &mut Vec<u8>, at: usize, entries: &[(u16, u16, u32, Vec<u8>)]) {
        t[at..at + 2].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        for (i, (tag, typ, count, data)) in entries.iter().enumerate() {
            let p = at + 2 + i * 12;
            t[p..p + 2].copy_from_slice(&tag.to_le_bytes());
            t[p + 2..p + 4].copy_from_slice(&typ.to_le_bytes());
            t[p + 4..p + 8].copy_from_slice(&count.to_le_bytes());
            if data.len() <= 4 { t[p + 8..p + 8 + data.len()].copy_from_slice(data); }
            else {
                let offset = t.len() as u32;
                t[p + 8..p + 12].copy_from_slice(&offset.to_le_bytes());
                t.extend(data);
            }
        }
    }

    fn sample_exif(orientation: u16) -> Vec<u8> {
        let mut t = vec![0; 194];
        t[..8].copy_from_slice(b"II\x2a\0\x08\0\0\0");
        ifd(&mut t, 8, &[
            (0x010f, 2, 8, b"TestCam\0".to_vec()),
            (0x0110, 2, 5, b"TC-1\0".to_vec()),
            (0x0112, 3, 1, orientation.to_le_bytes().to_vec()),
            (0x8769, 4, 1, 74u32.to_le_bytes().to_vec()),
            (0x8825, 4, 1, 140u32.to_le_bytes().to_vec()),
        ]);
        ifd(&mut t, 74, &[
            (0x829a, 5, 1, rational(1, 120)),
            (0x829d, 5, 1, rational(17, 10)),
            (0x8827, 3, 1, 50u16.to_le_bytes().to_vec()),
            (0x9003, 2, 20, b"2026:11:10 14:22:05\0".to_vec()),
            (0x920a, 5, 1, rational(63, 10)),
        ]);
        ifd(&mut t, 140, &[
            (1, 2, 2, b"N\0".to_vec()),
            (2, 5, 3, [rational(37, 1), rational(33, 1), rational(594, 10)].concat()),
            (3, 2, 2, b"E\0".to_vec()),
            (4, 5, 3, [rational(126, 1), rational(58, 1), rational(408, 10)].concat()),
        ]);
        t
    }

    fn jpeg_segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        [vec![255, marker], ((payload.len() + 2) as u16).to_be_bytes().to_vec(), payload.to_vec()].concat()
    }

    fn with_metadata(kind: image::ImageFormat, orientation: u16) -> Vec<u8> {
        let b = image_bytes(kind);
        let tiff = sample_exif(orientation);
        match kind {
            Jpeg => [b[..2].to_vec(),
                jpeg_segment(0xe1, &[b"Exif\0\0".as_slice(), &tiff].concat()),
                jpeg_segment(0xe1, b"http://ns.adobe.com/xap/1.0/\0private"),
                jpeg_segment(0xe1, &[b"http://ns.adobe.com/xmp/extension/\0".as_slice(),
                    b"00000000000000000000000000000000", &4u32.to_be_bytes(), &0u32.to_be_bytes(), b"<x/>"].concat()),
                jpeg_segment(0xed, b"private IPTC"), jpeg_segment(0xfe, b"private comment"),
                jpeg_segment(0xe2, b"ICC_PROFILE\0test"), b[2..].to_vec()].concat(),
            Png => [b[..33].to_vec(), png_chunk(b"eXIf", &tiff),
                png_chunk(b"tEXt", b"Author\0private"), png_chunk(b"iTXt", b"private"),
                png_chunk(b"zTXt", b"private"), png_chunk(b"iCCP", b"test\0\0x"), b[33..].to_vec()].concat(),
            WebP => {
                let mut vp8x = vec![0x0c, 0, 0, 0];
                vp8x.extend([0x8f, 1, 0, 0x2b, 1, 0]);
                let mut out = b[..12].to_vec();
                out.extend(webp_chunk(b"VP8X", &vp8x));
                out.extend(&b[12..]);
                out.extend(webp_chunk(b"EXIF", &tiff));
                out.extend(webp_chunk(b"XMP ", b"private"));
                let len = (out.len() - 8) as u32;
                out[4..8].copy_from_slice(&len.to_le_bytes());
                out
            }
            _ => unreachable!(),
        }
    }

    fn summary(bytes: &[u8]) -> Option<ExifSummary> {
        let dir = Path::new("target/alt/exif-tests");
        std::fs::create_dir_all(dir).unwrap();
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = dir.join(format!("{}-{}.jpg", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::write(&path, bytes).unwrap();
        let result = read_summary(&path);
        std::fs::remove_file(path).unwrap();
        result
    }

    fn sos(b: &[u8]) -> &[u8] { &b[b.windows(2).position(|w| w == [255, 0xda]).unwrap()..] }

    #[test]
    fn jpeg_strip_removes_gps_keeps_pixels_and_icc() {
        let input = with_metadata(Jpeg, 6);
        let s = summary(&input).unwrap();
        assert_eq!(s.make.as_deref(), Some("TestCam"));
        assert_eq!(s.model.as_deref(), Some("TC-1"));
        assert_eq!(s.taken.as_deref(), Some("2026:11:10 14:22:05"));
        assert_eq!(s.f_number.as_deref(), Some("f/1.7"));
        assert_eq!(s.exposure.as_deref(), Some("1/120초"));
        assert_eq!(s.iso.as_deref(), Some("ISO 50"));
        assert_eq!(s.focal.as_deref(), Some("6.3mm"));
        assert!(s.gps.is_some());
        let out = strip_private(&input, Jpeg).unwrap();
        let s = summary(&out).unwrap();
        assert!(s.gps.is_none() && s.make.is_none());
        assert_eq!(s.all.len(), 1);
        assert_eq!(crate::loader::exif_orientation_value(&out), Some(6));
        assert_eq!(sos(&input), sos(&out));
        let icc = jpeg_segment(0xe2, b"ICC_PROFILE\0test");
        assert!(out.windows(icc.len()).any(|w| w == icc));
        assert!(!out.windows(7).any(|w| w == b"private"));
        assert!(image::load_from_memory(&out).is_ok());
    }

    #[test]
    fn jpeg_strip_without_orientation_adds_no_exif() {
        let mut missing = with_metadata(Jpeg, 6);
        let tag = missing.windows(4).position(|w| w == [0x12, 0x01, 0x03, 0]).unwrap();
        missing[tag] = 0x13;
        for input in [with_metadata(Jpeg, 1), image_bytes(Jpeg), missing] {
            let out = strip_private(&input, Jpeg).unwrap();
            assert!(!out.windows(2).any(|w| w == [255, 0xe1]));
        }
    }

    #[test]
    fn png_strip_removes_text_and_exif_keeps_iccp() {
        let input = with_metadata(Png, 1);
        let out = strip_private(&input, Png).unwrap();
        let names = png_chunks(&out).unwrap().iter().map(|c| c.0.to_vec()).collect::<Vec<_>>();
        assert_eq!(names, [b"IHDR", b"iCCP", b"IDAT", b"IEND"]);
        let icc = png_chunk(b"iCCP", b"test\0\0x");
        assert!(out.windows(icc.len()).any(|w| w == icc));
        assert_eq!(image::load_from_memory(&input).unwrap().to_rgb8(), image::load_from_memory(&out).unwrap().to_rgb8());
        let rotated = strip_private(&with_metadata(Png, 6), Png).unwrap();
        assert_eq!(crate::loader::exif_orientation_value(&rotated), Some(6));
        assert_eq!(png_chunks(&rotated).unwrap()[1].0, b"eXIf");
    }

    #[test]
    fn webp_strip_clears_flags() {
        let input = with_metadata(WebP, 1);
        let out = strip_private(&input, WebP).unwrap();
        assert_eq!(out[20] & 0x0c, 0);
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);
        assert_eq!(image::load_from_memory(&input).unwrap().to_rgba8(), image::load_from_memory(&out).unwrap().to_rgba8());
        let rotated = strip_private(&with_metadata(WebP, 6), WebP).unwrap();
        assert_eq!(rotated[20] & 0x0c, 8);
        assert_eq!(crate::loader::exif_orientation_value(&rotated), Some(6));
        assert_eq!(summary(&rotated).unwrap().all.len(), 1);
    }

    #[test]
    fn webp_without_vp8x_reports_orientation_loss() {
        let mut input = image_bytes(WebP);
        input.extend(webp_chunk(b"EXIF", &sample_exif(6)));
        let size = (input.len() - 8) as u32;
        input[4..8].copy_from_slice(&size.to_le_bytes());
        assert_eq!(crate::loader::exif_orientation_value(&input), Some(6));
        let path = Path::new("target/alt/exif-tests/no-vp8x.webp");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, &input).unwrap();
        assert!(strip_file(path).unwrap());
        let out = std::fs::read(path).unwrap();
        assert_eq!(out, image_bytes(WebP));
        assert!(image::load_from_memory(&out).is_ok());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn malformed_containers_fail_without_panicking() {
        for (bytes, kind) in [
            (b"\xff\xd8\xff\xe1\x00\x01".as_slice(), Jpeg),
            (b"\x89PNG\r\n\x1a\n\xff\xff\xff\xffeXIf", Png),
            (b"RIFF\xff\xff\xff\xffWEBP", WebP),
        ] { assert!(strip_private(bytes, kind).is_err()); }
    }

    #[test]
    fn gps_dms_to_decimal() {
        let r = |n, d| exif::Rational { num: n, denom: d };
        let lat = [r(37, 1), r(33, 1), r(594, 10)];
        let lon = [r(126, 1), r(58, 1), r(408, 10)];
        for (reference, sign) in [("N", 1.0), ("S", -1.0)] {
            assert!((gps_decimal(&lat, reference).unwrap() - sign * 37.5665).abs() < 1e-4);
        }
        for (reference, sign) in [("E", 1.0), ("W", -1.0)] {
            assert!((gps_decimal(&lon, reference).unwrap() - sign * 126.978).abs() < 1e-4);
        }
    }

    #[test]
    fn exposure_label() {
        assert_eq!(exposure_text(1.0 / 120.0), "1/120초");
        assert_eq!(exposure_text(2.5), "2.5초");
    }

    #[test]
    fn can_strip_by_kind() {
        for p in ["a.jpg", "a.JPEG", "a.png", "a.webp"] { assert!(can_strip(Path::new(p))); }
        for p in ["a.heic", "a.gif", "a.tiff", "a.raw", "a.bmp", "a.ico", "a.cbz/x.jpg"] { assert!(!can_strip(Path::new(p))); }
    }

    #[test]
    #[ignore]
    fn make_exif_samples() {
        let dir = Path::new("target/alt/exif_sample");
        std::fs::create_dir_all(dir).unwrap();
        for (ext, kind) in [("jpg", Jpeg), ("png", Png), ("webp", WebP)] {
            let path = dir.join(format!("sample.{ext}"));
            let bytes = with_metadata(kind, 6);
            std::fs::write(&path, &bytes).unwrap();
            let summary = read_summary(&path).unwrap();
            assert_eq!(summary.make.as_deref(), Some("TestCam"));
            assert_eq!(summary.model.as_deref(), Some("TC-1"));
            let (lat, lon) = summary.gps.unwrap();
            assert!((lat - 37.5665).abs() < 1e-4 && (lon - 126.978).abs() < 1e-4);
            assert_eq!(crate::loader::exif_orientation_value(&bytes), Some(6));
            let image = image::load_from_memory(&bytes).unwrap();
            assert_eq!((image.width(), image.height()), (400, 300));
        }
    }
}
