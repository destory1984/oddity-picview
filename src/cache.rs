//! On-disk thumbnail cache.
//!
//! Layout: %LOCALAPPDATA%\oddity-picview\thumbs\<2 hex>\<16 hex>.jpg|.png
//! The key hashes the full path, file size and mtime, so an edited file gets a
//! new entry and the stale one ages out through `trim`.

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

/// Default size limit, changeable in the settings window.
pub const DEFAULT_MAX_MB: u64 = 100;
/// Bump when the thumbnail format or size changes, to ignore old entries.
const VERSION: u64 = 1;

pub fn dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        // PICVIEW_CACHE_DIR lets benchmarks run against an empty cache
        let d = match std::env::var_os("PICVIEW_CACHE_DIR") {
            Some(d) => PathBuf::from(d),
            None => PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(crate::APP_ID).join("thumbs"),
        };
        std::fs::create_dir_all(&d).ok()?;
        Some(d)
    })
    .as_deref()
}

/// FNV-1a: stable across builds, unlike std's DefaultHasher.
fn fnv(data: &[u8], mut h: u64) -> u64 {
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn key(path: &Path, size: u64, modified: SystemTime) -> u64 {
    let p = path.to_string_lossy().to_lowercase();
    let t = modified.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut h = fnv(p.as_bytes(), 0xcbf29ce484222325);
    h = fnv(&size.to_le_bytes(), h);
    h = fnv(&t.to_le_bytes(), h);
    fnv(&VERSION.to_le_bytes(), h)
}

fn entry_base(path: &Path, size: u64, modified: SystemTime) -> Option<PathBuf> {
    let k = format!("{:016x}", key(path, size, modified));
    Some(dir()?.join(&k[..2]).join(k))
}

pub fn load(path: &Path, size: u64, modified: SystemTime) -> Option<RgbaImage> {
    use std::io::Read;
    let base = entry_base(path, size, modified)?;
    for ext in ["jpg", "png"] {
        let Ok(mut f) = std::fs::File::options().read(true).write(true).open(base.with_extension(ext)) else {
            continue;
        };
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).ok()?;
        // keep recently used entries young so `trim` drops cold ones first
        let old = f
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|e| e.as_secs() > 86_400).unwrap_or(false))
            .unwrap_or(false);
        if old {
            let _ = f.set_modified(SystemTime::now());
        }
        return image::load_from_memory(&bytes).ok().map(|i| i.to_rgba8());
    }
    None
}

/// Opaque thumbnails are stored as JPEG (small), ones with alpha as PNG.
pub fn store(path: &Path, size: u64, modified: SystemTime, img: &RgbaImage) {
    let Some(base) = entry_base(path, size, modified) else { return };
    let (w, h) = img.dimensions();
    let opaque = img.pixels().all(|p| p[3] == 255);
    let mut buf = Vec::new();
    let (ext, ok) = if opaque {
        let rgb: Vec<u8> = img.pixels().flat_map(|p| [p[0], p[1], p[2]]).collect();
        let r = JpegEncoder::new_with_quality(&mut buf, 88).write_image(&rgb, w, h, ExtendedColorType::Rgb8);
        ("jpg", r.is_ok())
    } else {
        let r = PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::Sub)
            .write_image(img.as_raw(), w, h, ExtendedColorType::Rgba8);
        ("png", r.is_ok())
    };
    if !ok {
        return;
    }
    let target = base.with_extension(ext);
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // write then rename, so a crash never leaves a half-written entry
    let tmp = target.with_extension(format!("{ext}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &buf).is_ok() && std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Total size of the cache in bytes.
pub fn usage() -> u64 {
    let Some(d) = dir() else { return 0 };
    std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|shard| std::fs::read_dir(shard.path()).into_iter().flatten().flatten())
        .filter_map(|f| f.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// When the cache is over `max_bytes`, delete the least recently used entries
/// until it is down to 80% of it. Run off the UI thread.
pub fn trim(max_bytes: u64) {
    let trim_to = max_bytes / 10 * 8;
    let Some(d) = dir() else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for shard in std::fs::read_dir(d).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            let Ok(md) = f.metadata() else { continue };
            if f.path().extension().is_some_and(|e| e == "tmp") {
                let _ = std::fs::remove_file(f.path()); // leftovers from a crash
                continue;
            }
            total += md.len();
            files.push((md.modified().unwrap_or(SystemTime::UNIX_EPOCH), md.len(), f.path()));
        }
    }
    if total <= max_bytes {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, p) in files {
        if total <= trim_to {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}

pub fn clear() -> std::io::Result<()> {
    if let Some(d) = dir() {
        for shard in std::fs::read_dir(d)?.flatten() {
            let _ = std::fs::remove_dir_all(shard.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_opaque_and_alpha() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let opaque = RgbaImage::from_pixel(40, 30, image::Rgba([10, 200, 30, 255]));
        let alpha = RgbaImage::from_pixel(20, 50, image::Rgba([10, 200, 30, 100]));
        let (a, b) = (Path::new("Z:/__picview_test/a.jpg"), Path::new("Z:/__picview_test/b.png"));
        store(a, 1, t, &opaque);
        store(b, 2, t, &alpha);
        let la = load(a, 1, t).expect("opaque entry");
        let lb = load(b, 2, t).expect("alpha entry");
        assert_eq!(la.dimensions(), (40, 30));
        assert_eq!(lb.dimensions(), (20, 50));
        assert_eq!(lb.get_pixel(0, 0)[3], 100);
        assert!(load(a, 1, t + std::time::Duration::from_secs(1)).is_none(), "changed mtime must miss");
        for p in [entry_base(a, 1, t).unwrap().with_extension("jpg"), entry_base(b, 2, t).unwrap().with_extension("png")] {
            std::fs::remove_file(p).unwrap();
        }
    }

    /// PICVIEW_BENCH_DIR=<folder> cargo test --release bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench() {
        use rayon::prelude::*;
        let d = std::env::var("PICVIEW_BENCH_DIR").expect("set PICVIEW_BENCH_DIR");
        let files: Vec<(PathBuf, u64, SystemTime)> = std::fs::read_dir(d)
            .unwrap()
            .flatten()
            .filter(|e| crate::loader::is_image(&e.path()))
            .map(|e| {
                let m = e.metadata().unwrap();
                (e.path(), m.len(), m.modified().unwrap())
            })
            .collect();
        let t0 = std::time::Instant::now();
        files.par_iter().for_each(|(p, s, m)| {
            if let Ok(i) = crate::loader::make_thumb(p) {
                store(p, *s, *m, &i);
            }
        });
        let cold = t0.elapsed();
        let t1 = std::time::Instant::now();
        let hits = files.par_iter().filter(|(p, s, m)| load(p, *s, *m).is_some()).count();
        let warm = t1.elapsed();
        println!("{} files: decode+store {:?}, cache load {:?} ({hits} hits)", files.len(), cold, warm);
    }
}
