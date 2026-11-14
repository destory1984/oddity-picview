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

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMG_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
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
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
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
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
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
