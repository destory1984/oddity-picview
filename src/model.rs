//! Read embedded model previews without rendering or modifying the source.
use image::RgbaImage;
use serde_json::Value;
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::path::Path;

pub const MODEL_EXTS: &[&str] = &["blend", "vrm"];
const MAX_READ: u64 = 64 * 1024 * 1024;
const MAX_BLOCKS: usize = 4096;
const BAD: &str = "미리보기 데이터가 올바르지 않거나 잘렸습니다";
const MISSING: &str = "미리보기 그림이 없습니다";
const LIMIT: &str = "미리보기 탐색 한도를 넘었습니다";

pub fn is_model(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MODEL_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn is_blend(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("blend"))
}

pub fn thumbnail(bytes: &[u8], path: &Path) -> Result<RgbaImage, String> {
    if is_blend(path) {
        blend(Cursor::new(bytes), &mut DecodeInfo::default())
    } else if is_model(path) {
        vrm(bytes)
    } else {
        Err("지원하지 않는 모델 형식입니다".into())
    }
}

pub fn load_thumbnail(path: &Path) -> Result<RgbaImage, String> {
    if is_blend(path) && crate::archive::split(path).is_none() {
        let file = std::fs::File::open(path).map_err(|_| "모델 파일을 읽을 수 없습니다")?;
        blend(file, &mut DecodeInfo::default())
    } else {
        thumbnail(&crate::loader::read_bytes(path)?, path)
    }
}

#[derive(Default)]
struct DecodeInfo {
    header_kind: &'static str,
    compression: &'static str,
    bytes_read: u64,
    decompressed_bytes: u64,
}

struct Counted<'a, R> {
    inner: R,
    count: &'a mut u64,
}
impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(out)?;
        *self.count += n as u64;
        Ok(n)
    }
}

// Blender writes independent zstd frames. Recreate the decoder between frames.
struct Zstd<R: BufRead> {
    source: Option<R>,
    frame: Option<ruzstd::decoding::StreamingDecoder<R, ruzstd::decoding::FrameDecoder>>,
    frames: usize,
}
impl<R: BufRead> Read for Zstd<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.frame.is_none() {
                if self.frames == MAX_BLOCKS {
                    return Err(io::Error::other(LIMIT));
                }
                self.frames += 1;
                let mut source = self.source.take().ok_or_else(|| io::Error::other(BAD))?;
                if source.fill_buf()?.is_empty() {
                    self.source = Some(source);
                    return Ok(0);
                }
                // A seekable zstd stream may finish with a skippable index frame.
                let magic = source.fill_buf()?;
                if magic.len() >= 4 && magic[0] & 0xf0 == 0x50 && magic[1..4] == [0x2a, 0x4d, 0x18]
                {
                    let mut header = [0; 8];
                    source.read_exact(&mut header)?;
                    let n = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
                    if n > MAX_READ {
                        return Err(io::Error::other(LIMIT));
                    }
                    if io::copy(&mut source.by_ref().take(n), &mut io::sink())? != n {
                        return Err(io::Error::other(BAD));
                    }
                    self.source = Some(source);
                    continue;
                }
                self.frame = Some(
                    ruzstd::decoding::StreamingDecoder::new_with_max_window_size(source, MAX_READ)
                        .map_err(|_| io::Error::other(BAD))?,
                );
            }
            let frame = self.frame.as_mut().unwrap();
            let n = frame.read(out)?;
            if n != 0 {
                return Ok(n);
            }
            self.source = Some(self.frame.take().unwrap().into_inner());
        }
    }
}

fn exact(r: &mut impl Read, bytes: &mut [u8]) -> Result<(), String> {
    r.read_exact(bytes).map_err(|_| BAD.into())
}

fn blend(source: impl Read, info: &mut DecodeInfo) -> Result<RgbaImage, String> {
    let counted = Counted {
        inner: source,
        count: &mut info.bytes_read,
    };
    let mut source = BufReader::new(counted.take(MAX_READ));
    let magic = source.fill_buf().map_err(|_| BAD)?;
    let mut decoded: Box<dyn Read + '_> = if magic.starts_with(&[0x1f, 0x8b]) {
        info.compression = "gzip";
        Box::new(flate2::bufread::GzDecoder::new(source))
    } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        info.compression = "zstd";
        Box::new(Zstd {
            source: Some(source),
            frame: None,
            frames: 0,
        })
    } else {
        info.compression = "plain";
        Box::new(source)
    };
    let counted = Counted {
        inner: &mut decoded,
        count: &mut info.decompressed_bytes,
    };
    let mut r = counted.take(MAX_READ);
    let mut h = [0; 17];
    exact(&mut r, &mut h[..12])?;
    if &h[..7] != b"BLENDER" {
        return Err(BAD.into());
    }
    // Layout verified against Blender's BLO_core_blend_header.hh and BLO_core_bhead.hh.
    let (header_len, len_at, large) = match h[7] {
        b'_' | b'-' => {
            if h[8] == b'V' {
                return Err("빅 엔디언 Blender 파일은 지원하지 않습니다".into());
            }
            if h[8] != b'v' || !h[9..12].iter().all(u8::is_ascii_digit) {
                return Err(BAD.into());
            }
            info.header_kind = if h[7] == b'_' { "legacy32" } else { "legacy64" };
            (if h[7] == b'_' { 20 } else { 24 }, 4, false)
        }
        b'1' if &h[7..12] == b"17-01" => {
            exact(&mut r, &mut h[12..])?;
            if h[12] != b'v' || !h[13..].iter().all(u8::is_ascii_digit) {
                return Err(BAD.into());
            }
            info.header_kind = "5.x";
            (32, 16, true)
        }
        _ => return Err("지원하지 않는 Blender 헤더입니다".into()),
    };
    for _ in 0..MAX_BLOCKS {
        if r.limit() < header_len as u64 {
            return Err(LIMIT.into());
        }
        let mut block = [0; 32];
        exact(&mut r, &mut block[..header_len])?;
        let len = if large {
            i64::from_le_bytes(block[len_at..len_at + 8].try_into().unwrap())
        } else {
            i32::from_le_bytes(block[len_at..len_at + 4].try_into().unwrap()) as i64
        };
        if len < 0 {
            return Err(BAD.into());
        }
        if &block[..4] == b"ENDB" {
            return Err(MISSING.into());
        }
        let len = len as u64;
        if len > r.limit() {
            return Err(LIMIT.into());
        }
        if &block[..4] == b"TEST" {
            if len < 8 {
                return Err(BAD.into());
            }
            let mut dims = [0; 8];
            exact(&mut r, &mut dims)?;
            let w = i32::from_le_bytes(dims[..4].try_into().unwrap());
            let h = i32::from_le_bytes(dims[4..].try_into().unwrap());
            if !(1..=4096).contains(&w)
                || !(1..=4096).contains(&h)
                || len != 8 + w as u64 * h as u64 * 4
            {
                return Err(BAD.into());
            }
            let mut pixels = vec![0; (len - 8) as usize];
            exact(&mut r, &mut pixels)?;
            let mut img = RgbaImage::from_raw(w as u32, h as u32, pixels).ok_or(BAD)?;
            image::imageops::flip_vertical_in_place(&mut img);
            return Ok(img);
        }
        let n = io::copy(&mut r.by_ref().take(len), &mut io::sink()).map_err(|_| BAD)?;
        if n != len {
            return Err(BAD.into());
        }
    }
    Err(LIMIT.into())
}

fn word(bytes: &[u8], at: usize) -> Result<u32, String> {
    let end = at.checked_add(4).ok_or(BAD)?;
    Ok(u32::from_le_bytes(
        bytes.get(at..end).ok_or(BAD)?.try_into().unwrap(),
    ))
}

fn index(v: &Value) -> Result<usize, String> {
    usize::try_from(v.as_u64().ok_or(MISSING)?).map_err(|_| BAD.into())
}

fn chunk<'a>(bytes: &'a [u8], at: &mut usize, kind: u32) -> Result<&'a [u8], String> {
    let len = word(bytes, *at)? as usize;
    if len % 4 != 0 || word(bytes, at.checked_add(4).ok_or(BAD)?)? != kind {
        return Err(BAD.into());
    }
    let start = at.checked_add(8).ok_or(BAD)?;
    *at = start.checked_add(len).ok_or(BAD)?;
    bytes.get(start..*at).ok_or_else(|| BAD.into())
}

fn vrm(bytes: &[u8]) -> Result<RgbaImage, String> {
    if bytes.get(..4) != Some(b"glTF")
        || word(bytes, 4)? != 2
        || word(bytes, 8)? as usize != bytes.len()
    {
        return Err(BAD.into());
    }
    let mut at = 12;
    let json = chunk(bytes, &mut at, 0x4e4f534a)?;
    // Bound JSON parsing independently of the size of the model's mesh/textures.
    if json.len() > 4 * 1024 * 1024 {
        return Err(LIMIT.into());
    }
    let doc: Value = serde_json::from_slice(json).map_err(|_| BAD)?;
    let bin = chunk(bytes, &mut at, 0x004e4942)?;
    // Additional GLB chunks are legal, but still have to be bounded.
    while at < bytes.len() {
        let kind = word(bytes, at.checked_add(4).ok_or(BAD)?)?;
        chunk(bytes, &mut at, kind)?;
    }
    let img = if !doc["extensions"]["VRMC_vrm"].is_null() {
        index(&doc["extensions"]["VRMC_vrm"]["meta"]["thumbnailImage"])?
    } else {
        let texture = index(&doc["extensions"]["VRM"]["meta"]["texture"])?;
        index(&doc["textures"][texture]["source"])?
    };
    let view = index(&doc["images"][img]["bufferView"])?;
    let view = &doc["bufferViews"][view];
    if view["buffer"].as_u64() != Some(0) || !doc["buffers"][0]["uri"].is_null() {
        return Err(BAD.into());
    }
    let buffer_len = index(&doc["buffers"][0]["byteLength"])?;
    if buffer_len > bin.len() || bin.len() - buffer_len > 3 {
        return Err(BAD.into());
    }
    let offset = if view["byteOffset"].is_null() {
        0
    } else {
        index(&view["byteOffset"])?
    };
    let end = offset.checked_add(index(&view["byteLength"])?).ok_or(BAD)?;
    if end > buffer_len {
        return Err(BAD.into());
    }
    let bytes = bin.get(offset..end).ok_or(BAD)?;
    let format = image::guess_format(bytes).map_err(|_| BAD)?;
    if !matches!(format, image::ImageFormat::Png | image::ImageFormat::Jpeg) {
        return Err(BAD.into());
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(MAX_READ);
    reader.limits(limits);
    reader
        .decode()
        .map(|i| i.to_rgba8())
        .map_err(|_| BAD.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgba};
    use serde_json::json;
    use std::io::Write;

    fn pixels() -> RgbaImage {
        RgbaImage::from_fn(3, 2, |x, y| Rgba([x as u8 * 60, y as u8 * 120, 17, 255]))
    }

    fn block(out: &mut Vec<u8>, code: &[u8; 4], len: i64, layout: usize) {
        out.extend(code);
        if layout == 32 {
            out.extend([0; 12]);
            out.extend(len.to_le_bytes());
            out.extend(1i64.to_le_bytes());
        } else {
            out.extend((len as i32).to_le_bytes());
            out.extend(vec![0; layout - 8]);
        }
    }

    fn scene(layout: usize) -> Vec<u8> {
        let mut out = match layout {
            20 => b"BLENDER_v280".to_vec(),
            24 => b"BLENDER-v405".to_vec(),
            _ => b"BLENDER17-01v0500".to_vec(),
        };
        block(&mut out, b"REND", 4, layout);
        out.extend([0; 4]);
        block(&mut out, b"TEST", 32, layout);
        out.extend(3i32.to_le_bytes());
        out.extend(2i32.to_le_bytes());
        out.extend(image::imageops::flip_vertical(&pixels()).into_raw());
        out
    }

    // Standard zstd single-segment frame containing a raw (uncompressed) block.
    fn zstd_frame(bytes: &[u8]) -> Vec<u8> {
        assert!(bytes.len() < 256);
        let mut out = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, bytes.len() as u8];
        let header = ((bytes.len() as u32) << 3) | 1;
        out.extend(&header.to_le_bytes()[..3]);
        out.extend(bytes);
        out
    }

    fn glb(mut doc: Value, png: &[u8]) -> Vec<u8> {
        doc["buffers"] = json!([{"byteLength": png.len()}]);
        doc["bufferViews"] = json!([{"buffer": 0, "byteLength": png.len()}]);
        doc["images"] = json!([{"bufferView": 0, "mimeType": "image/png"}]);
        let mut text = serde_json::to_vec(&doc).unwrap();
        while text.len() % 4 != 0 {
            text.push(b' ');
        }
        let mut bin = png.to_vec();
        while bin.len() % 4 != 0 {
            bin.push(0);
        }
        let mut out = b"glTF".to_vec();
        out.extend(2u32.to_le_bytes());
        out.extend(((28 + text.len() + bin.len()) as u32).to_le_bytes());
        out.extend((text.len() as u32).to_le_bytes());
        out.extend(0x4e4f534au32.to_le_bytes());
        out.extend(text);
        out.extend((bin.len() as u32).to_le_bytes());
        out.extend(0x004e4942u32.to_le_bytes());
        out.extend(bin);
        out
    }

    fn png() -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels())
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn blend_legacy32_legacy64_and_modern_flip() {
        for layout in [20, 24, 32] {
            assert_eq!(
                thumbnail(&scene(layout), Path::new("scene.blend")).unwrap(),
                pixels()
            );
        }
    }

    #[test]
    fn blend_gzip_flip() {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&scene(24)).unwrap();
        assert_eq!(
            thumbnail(&encoder.finish().unwrap(), Path::new("scene.blend")).unwrap(),
            pixels()
        );
    }

    #[test]
    fn blend_zstd_and_multiple_frames_flip() {
        let bytes = scene(32);
        assert_eq!(
            thumbnail(&zstd_frame(&bytes), Path::new("scene.blend")).unwrap(),
            pixels()
        );
        let mut frames = zstd_frame(&bytes[..50]);
        frames.extend(zstd_frame(&bytes[50..]));
        assert_eq!(
            thumbnail(&frames, Path::new("scene.blend")).unwrap(),
            pixels()
        );
    }

    #[test]
    fn vrm_zero_and_one() {
        for doc in [
            json!({"extensions":{"VRM":{"meta":{"texture":0}}},"textures":[{"source":0}]}),
            json!({"extensions":{"VRMC_vrm":{"meta":{"thumbnailImage":0}}}}),
        ] {
            assert_eq!(
                thumbnail(&glb(doc, &png()), Path::new("avatar.vrm")).unwrap(),
                pixels()
            );
        }
    }

    #[test]
    fn malformed_models_do_not_panic() {
        let good = [
            scene(20),
            scene(24),
            scene(32),
            zstd_frame(&scene(32)),
            glb(
                json!({"extensions":{"VRMC_vrm":{"meta":{"thumbnailImage":0}}}}),
                &png(),
            ),
        ];
        for (i, bytes) in good.iter().enumerate() {
            let path = Path::new(if i == 4 { "a.vrm" } else { "a.blend" });
            for end in 0..bytes.len() {
                assert!(
                    thumbnail(&bytes[..end], path).is_err(),
                    "prefix {end} of sample {i}"
                );
            }
        }
        for ext in ["blend", "vrm"] {
            let path = std::path::PathBuf::from(format!("a.{ext}"));
            for n in 0..100 {
                assert!(thumbnail(&vec![0xff; n], &path).is_err());
            }
        }
    }

    #[test]
    fn missing_preview_and_invalid_dimensions() {
        let mut bytes = b"BLENDER-v405".to_vec();
        block(&mut bytes, b"ENDB", 0, 24);
        assert_eq!(
            thumbnail(&bytes, Path::new("a.blend")).unwrap_err(),
            MISSING
        );
        for dimension in [-1i32, 0, 4097, i32::MAX] {
            let mut bytes = scene(32);
            bytes[85..89].copy_from_slice(&dimension.to_le_bytes());
            assert!(thumbnail(&bytes, Path::new("a.blend")).is_err());
        }
        assert!(thumbnail(&glb(json!({}), &png()), Path::new("a.vrm")).is_err());
    }

    #[test]
    fn vrm_rejects_invalid_references_and_lengths() {
        let base = json!({"extensions":{"VRMC_vrm":{"meta":{"thumbnailImage":0}}}});
        for pointer in [
            "/images/0/bufferView",
            "/bufferViews/0/buffer",
            "/bufferViews/0/byteOffset",
            "/bufferViews/0/byteLength",
        ] {
            let mut doc: Value = serde_json::from_slice(
                &glb(base.clone(), &png())
                    [20..20 + word(&glb(base.clone(), &png()), 12).unwrap() as usize],
            )
            .unwrap();
            if pointer.ends_with("byteOffset") {
                doc["bufferViews"][0]["byteOffset"] = json!(0);
            }
            *doc.pointer_mut(pointer).unwrap() = json!(u64::MAX);
            let mut text = serde_json::to_vec(&doc).unwrap();
            while text.len() % 4 != 0 {
                text.push(b' ');
            }
            let original = glb(base.clone(), &png());
            let bin_at = 20 + word(&original, 12).unwrap() as usize;
            let mut bytes = original[..12].to_vec();
            bytes.extend((text.len() as u32).to_le_bytes());
            bytes.extend(0x4e4f534au32.to_le_bytes());
            bytes.extend(text);
            bytes.extend(&original[bin_at..]);
            let len = bytes.len() as u32;
            bytes[8..12].copy_from_slice(&len.to_le_bytes());
            assert!(thumbnail(&bytes, Path::new("a.vrm")).is_err());
        }
        let mut bytes = glb(base, &png());
        bytes[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(thumbnail(&bytes, Path::new("a.vrm")).is_err());
    }

    #[test]
    fn blend_limits_and_early_stop() {
        let mut bytes = b"BLENDER-v405".to_vec();
        block(&mut bytes, b"DATA", MAX_READ as i64, 24);
        let mut info = DecodeInfo::default();
        assert_eq!(blend(Cursor::new(bytes), &mut info).unwrap_err(), LIMIT);
        assert!(info.bytes_read <= MAX_READ);
        let mut bytes = b"BLENDER-v405".to_vec();
        for _ in 0..MAX_BLOCKS {
            block(&mut bytes, b"DATA", 0, 24);
        }
        assert_eq!(thumbnail(&bytes, Path::new("a.blend")).unwrap_err(), LIMIT);
        // A virtual 301 MiB stream must stop immediately after the thumbnail.
        let bytes = scene(32);
        let stream = Cursor::new(&bytes).chain(io::repeat(0).take(301 * 1024 * 1024));
        let mut info = DecodeInfo::default();
        assert_eq!(blend(stream, &mut info).unwrap(), pixels());
        assert_eq!(info.decompressed_bytes, bytes.len() as u64);
        assert!(info.bytes_read <= 8192);
    }

    #[test]
    fn model_files_are_view_only_images() {
        for name in ["a.BLEND", "a.vrm", "a.VRM"] {
            let path = Path::new(name);
            assert!(is_model(path));
            assert!(crate::loader::is_image(path));
            assert!(!crate::meta::can_strip(path));
            for turns in [0, 1] {
                assert!(crate::rotate::save_rotation(path, turns)
                    .unwrap_err()
                    .contains("보기만 돌립니다"));
            }
        }
        assert!(!is_model(Path::new("a.glb")));
    }

    #[test]
    fn disk_and_archive_loaders_decode_models() {
        let dir = std::env::temp_dir().join(format!(
            "picview-model-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let samples = [
            ("scene.BLEND", scene(32)),
            (
                "avatar.vrm",
                glb(
                    json!({"extensions":{"VRM":{"meta":{"texture":0}}},"textures":[{"source":0}]}),
                    &png(),
                ),
            ),
        ];
        let zip_path = dir.join("models.zip");
        let seven_path = dir.join("models.7z");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let mut seven = sevenz_rust2::ArchiveWriter::create(&seven_path).unwrap();
        for (name, bytes) in &samples {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(
                crate::loader::load_full(&path, 4096).unwrap(),
                (pixels(), [3, 2])
            );
            assert_eq!(crate::loader::make_thumb(&path).unwrap(), pixels());
            assert_eq!(
                crate::loader::load_full(&path, 2).unwrap().0.dimensions(),
                (2, 1)
            );
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
            seven
                .push_archive_entry(
                    sevenz_rust2::ArchiveEntry::from_path(&path, (*name).into()),
                    Some(Cursor::new(bytes)),
                )
                .unwrap();
        }
        zip.finish().unwrap();
        seven.finish().unwrap();
        for archive in [zip_path, seven_path] {
            assert_eq!(crate::archive::list_images(&archive).unwrap().len(), 2);
            for (name, _) in &samples {
                let path = archive.join(name);
                assert_eq!(crate::loader::make_thumb(&path).unwrap(), pixels());
                assert_eq!(
                    crate::loader::load_full(&path, 4096).unwrap(),
                    (pixels(), [3, 2])
                );
                assert!(!crate::meta::can_strip(&path));
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Set PICVIEW_MODEL_DIR to the sample root; outputs stay under target/alt.
    #[test]
    #[ignore]
    fn real_models() {
        let root = std::path::PathBuf::from(
            std::env::var_os("PICVIEW_MODEL_DIR").expect("PICVIEW_MODEL_DIR"),
        );
        let output = Path::new("target/alt/model_sample");
        std::fs::create_dir_all(output.join("out")).unwrap();
        let mut files = Vec::new();
        if let Some(names) = std::env::var_os("PICVIEW_MODEL_FILES") {
            files.extend(std::env::split_paths(&names).map(|name| root.join(name)));
        } else {
            for entry in std::fs::read_dir(&root).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() && is_model(&entry.path()) {
                    files.push(entry.path());
                }
            }
        }
        for name in ["plain.blend", "zstd.blend"] {
            let p = output.join(name);
            if p.exists() {
                files.push(p);
            }
        }
        files.sort();
        let mut rows = Vec::new();
        for (i, path) in files.iter().enumerate() {
            let started = std::time::Instant::now();
            let mut info = DecodeInfo::default();
            let decoded = if is_blend(path) {
                blend(std::fs::File::open(path).unwrap(), &mut info)
            } else {
                info.header_kind = "GLB2";
                info.compression = "plain";
                let bytes = std::fs::read(path).unwrap();
                info.bytes_read = bytes.len() as u64;
                thumbnail(&bytes, path)
            };
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let (width, height) = decoded
                .as_ref()
                .map(|img| img.dimensions())
                .unwrap_or((0, 0));
            let name = path.file_name().unwrap().to_string_lossy();
            if let Ok(img) = &decoded {
                img.save(output.join("out").join(format!("{i}-{name}.png")))
                    .unwrap();
            }
            rows.push(json!({"name": name, "header_kind": info.header_kind, "compression": info.compression,
                "ok": decoded.is_ok(), "error": decoded.err(), "width": width, "height": height,
                "ms": ms, "bytes_read": info.bytes_read, "decompressed_bytes": info.decompressed_bytes}));
            assert!(!is_blend(path) || info.bytes_read <= MAX_READ);
        }
        std::fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();
        println!("{}", serde_json::to_string_pretty(&rows).unwrap());
    }
}
