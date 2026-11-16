//! Image decoding. Runs on worker threads.

use image::{DynamicImage, ImageBuffer, Luma, Rgb, RgbaImage};
use std::io::Cursor;
use std::path::Path;

pub const THUMB_MAX: u32 = 256;

pub const IMG_EXTS: &[&str] = &[
    "jpg", "jpeg", "jfif", "jpe", "png", "gif", "bmp", "webp", "tif", "tiff", "ico",
];

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v", "flv", "mpg", "mpeg", "ts", "m2ts", "3gp",
];
pub const AUDIO_EXTS: &[&str] = &["mp3", "wav", "flac", "m4a", "aac", "ogg", "wma", "opus"];

#[derive(Clone, Copy, PartialEq)]
pub enum Media {
    Video,
    Audio,
}

fn ext_in(path: &Path, list: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| list.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

pub fn media_kind(path: &Path) -> Option<Media> {
    if ext_in(path, VIDEO_EXTS) {
        Some(Media::Video)
    } else if ext_in(path, AUDIO_EXTS) {
        Some(Media::Audio)
    } else {
        None
    }
}

/// Read through Windows Imaging Component (HEIF / Raw image extensions).
pub const WIC_EXTS: &[&str] = &[
    "heic", "heif", "avif", // HEIF family
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "pef", "srw", "3fr",
    "erf", "kdc", "mrw", "x3f", "raw", "iiq", "mos", // camera RAW
];

pub fn is_wic(path: &Path) -> bool {
    ext_in(path, WIC_EXTS)
}

pub fn is_image(path: &Path) -> bool {
    ext_in(path, IMG_EXTS) || is_wic(path)
}

pub fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    if let Some((archive, name)) = crate::archive::split(path) {
        crate::archive::read_entry(&archive, &name)
    } else {
        std::fs::read(path).map_err(|e| e.to_string())
    }
}

/// Rotate / mirror decoded pixels by an EXIF orientation value.
pub fn orient(img: RgbaImage, dims: [u32; 2], o: Option<u8>) -> (RgbaImage, [u32; 2]) {
    match o.filter(|&v| v > 1).and_then(image::metadata::Orientation::from_exif) {
        Some(or) => {
            let mut d = DynamicImage::ImageRgba8(img);
            d.apply_orientation(or);
            let swapped = matches!(o, Some(5..=8));
            (d.to_rgba8(), if swapped { [dims[1], dims[0]] } else { dims })
        }
        None => (img, dims),
    }
}

/// All frames of an animated GIF with their display times, scaled to fit
/// `max_side`. None for a still GIF, or one too big to keep in memory.
pub fn load_gif_frames(path: &Path, max_side: u32) -> Option<Vec<(RgbaImage, std::time::Duration)>> {
    use image::AnimationDecoder;
    const MAX_BYTES: usize = 512 * 1024 * 1024;
    let file = Cursor::new(read_bytes(path).ok()?);
    let dec = image::codecs::gif::GifDecoder::new(file).ok()?;
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for f in dec.into_frames() {
        let f = f.ok()?;
        let (n, d) = f.delay().numer_denom_ms();
        let ms = if d == 0 { 0 } else { n / d };
        // browsers treat 0-10ms frames as 100ms; so do we
        let ms = if ms <= 10 { 100 } else { ms };
        let mut img = f.into_buffer();
        if img.width() > max_side || img.height() > max_side {
            img = DynamicImage::ImageRgba8(img).resize(max_side, max_side, image::imageops::FilterType::Triangle).to_rgba8();
        }
        bytes += img.as_raw().len();
        if bytes > MAX_BYTES {
            return None;
        }
        out.push((img, std::time::Duration::from_millis(ms as u64)));
    }
    (out.len() > 1).then_some(out)
}

/// Raw EXIF Orientation value (1..=8), if the file has one.
pub fn exif_orientation_value(bytes: &[u8]) -> Option<u8> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    field.value.get_uint(0).map(|v| v as u8)
}

fn exif_orientation(bytes: &[u8]) -> Option<image::metadata::Orientation> {
    image::metadata::Orientation::from_exif(exif_orientation_value(bytes)?)
}

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8
}

/// JPEG decode with DCT scaling (1/2, 1/4, 1/8). Much faster than a full decode
/// when only a thumbnail is needed.
fn decode_jpeg_scaled(bytes: &[u8], max: u32) -> Option<DynamicImage> {
    let mut d = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    d.read_info().ok()?;
    let (w, h) = d.scale(max as u16, max as u16).ok()?;
    let px = d.decode().ok()?;
    let info = d.info()?;
    let (w, h) = (w as u32, h as u32);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            ImageBuffer::<Rgb<u8>, _>::from_raw(w, h, px).map(DynamicImage::ImageRgb8)
        }
        jpeg_decoder::PixelFormat::L8 => {
            ImageBuffer::<Luma<u8>, _>::from_raw(w, h, px).map(DynamicImage::ImageLuma8)
        }
        _ => None,
    }
}

pub fn make_thumb(path: &Path) -> Result<RgbaImage, String> {
    if is_wic(path) {
        return crate::fileops::wic_decode(path, THUMB_MAX).map(|(img, dims, o)| orient(img, dims, o).0);
    }
    let bytes = read_bytes(path)?;
    let orient = exif_orientation(&bytes);
    let img = if is_jpeg(&bytes) {
        match decode_jpeg_scaled(&bytes, THUMB_MAX) {
            Some(i) => i,
            None => image::load_from_memory(&bytes).map_err(|e| e.to_string())?,
        }
    } else {
        image::load_from_memory(&bytes).map_err(|e| e.to_string())?
    };
    let mut img = if img.width() > THUMB_MAX || img.height() > THUMB_MAX {
        img.thumbnail(THUMB_MAX, THUMB_MAX)
    } else {
        img
    };
    if let Some(o) = orient {
        img.apply_orientation(o);
    }
    Ok(img.to_rgba8())
}

/// Full decode for the preview / viewer. Returns the image (downscaled to fit
/// `max_side` if needed) and the original oriented size.
pub fn load_full(path: &Path, max_side: u32) -> Result<(RgbaImage, [u32; 2]), String> {
    if is_wic(path) {
        return crate::fileops::wic_decode(path, max_side).map(|(img, dims, o)| orient(img, dims, o));
    }
    let bytes = read_bytes(path)?;
    let orient = exif_orientation(&bytes);
    let mut img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    if let Some(o) = orient {
        img.apply_orientation(o);
    }
    let dims = [img.width(), img.height()];
    if img.width() > max_side || img.height() > max_side {
        img = img.resize(max_side, max_side, image::imageops::FilterType::Triangle);
    }
    Ok((img.to_rgba8(), dims))
}

#[cfg(test)]
mod tests {
    #[test]
    fn animated_gif_frames() {
        // two 4x4 frames, 50ms and 0ms (0 must become 100ms)
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame, Rgba, RgbaImage};
        let dir = std::env::temp_dir().join(format!("oddity-gif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.gif");
        {
            let f = std::fs::File::create(&p).unwrap();
            let mut enc = GifEncoder::new(f);
            let a = Frame::from_parts(RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255])), 0, 0, Delay::from_numer_denom_ms(50, 1));
            let b = Frame::from_parts(RgbaImage::from_pixel(4, 4, Rgba([0, 0, 255, 255])), 0, 0, Delay::from_numer_denom_ms(0, 1));
            enc.encode_frames(vec![a, b]).unwrap();
        }
        let frames = super::load_gif_frames(&p, 1024).expect("animated");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].1.as_millis(), 50);
        assert_eq!(frames[1].1.as_millis(), 100);
        assert_eq!(frames[1].0.get_pixel(0, 0)[2], 255);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// PICVIEW_GIF=<file> cargo test --release real_gif -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_gif() {
        let p = std::env::var("PICVIEW_GIF").unwrap();
        let t = std::time::Instant::now();
        let f = super::load_gif_frames(std::path::Path::new(&p), 4096).expect("animated");
        let total: u128 = f.iter().map(|x| x.1.as_millis()).sum();
        println!("{} frames, {}x{}, {} ms loop, decoded in {:?}", f.len(), f[0].0.width(), f[0].0.height(), total, t.elapsed());
    }
}
