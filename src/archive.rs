//! Read-only comic archives and virtual image paths.
use std::path::{Path, PathBuf};
use std::io::Read;

pub const ARCHIVE_EXTS: &[&str] = &["zip", "cbz", "rar", "cbr", "7z", "cb7"];

#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
}

pub fn is_archive(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str())
        .is_some_and(|e| ARCHIVE_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

pub fn is_location(path: &Path) -> bool {
    is_archive(path) && !path.is_dir()
}

pub fn badge(path: &Path) -> &'static str {
    match path.extension().unwrap_or_default().to_string_lossy().to_ascii_lowercase().as_str() {
        "rar" | "cbr" => "RAR",
        "7z" | "cb7" => "7Z",
        _ => "ZIP",
    }
}

pub fn split(path: &Path) -> Option<(PathBuf, String)> {
    for parent in path.ancestors().skip(1) {
        if is_archive(parent) && parent.is_file() {
            let name = path.strip_prefix(parent).ok()?.to_string_lossy().replace('\\', "/");
            return Some((parent.to_path_buf(), name));
        }
    }
    None
}

fn error(e: impl std::fmt::Display) -> String {
    let s = e.to_string();
    let lower = s.to_ascii_lowercase();
    if ["password", "encrypted", "encryption"].iter().any(|x| lower.contains(x)) {
        "암호가 걸린 압축 파일입니다".into()
    } else {
        format!("압축 파일을 읽을 수 없습니다: {s}")
    }
}

fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(error("손상된 데이터")))
}

fn image_name(name: &str) -> Option<String> {
    let name = name.replace('\\', "/");
    if name.split('/').any(|s| s.is_empty() || s.starts_with('.') || s == "__MACOSX" || s.contains(':'))
        || !crate::loader::is_image(Path::new(&name)) { return None; }
    Some(name)
}

fn seven(archive: &Path) -> Result<sevenz_rust2::ArchiveReader<std::fs::File>, String> {
    let mut r = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty()).map_err(error)?;
    r.set_thread_count(1);
    if r.archive().blocks.iter().flat_map(|b| &b.coders)
        .any(|c| c.encoder_method_id() == [0x06, 0xf1, 0x07, 0x01]) {
        return Err("암호가 걸린 압축 파일입니다".into());
    }
    Ok(r)
}

pub fn list_images(archive: &Path) -> Result<Vec<ArchiveEntry>, String> {
    guarded(|| {
        let mut out = Vec::new();
        match badge(archive) {
            "ZIP" => {
                let f = std::fs::File::open(archive).map_err(error)?;
                let mut r = zip::ZipArchive::new(f).map_err(error)?;
                for i in 0..r.len() {
                    let e = r.by_index(i).map_err(error)?;
                    if e.encrypted() { return Err("암호가 걸린 압축 파일입니다".into()); }
                    if !e.is_dir() {
                        if let Some(name) = image_name(e.name()) { out.push(ArchiveEntry { name, size: e.size() }); }
                    }
                }
            }
            "RAR" => {
                let r = unrar::Archive::new(archive).open_for_listing().map_err(error)?;
                for e in r {
                    let e = e.map_err(error)?;
                    if e.is_encrypted() { return Err("암호가 걸린 압축 파일입니다".into()); }
                    if e.is_file() {
                        if let Some(name) = image_name(&e.filename.to_string_lossy()) {
                            out.push(ArchiveEntry { name, size: e.unpacked_size });
                        }
                    }
                }
            }
            _ => {
                let r = seven(archive)?;
                for e in &r.archive().files {
                    if !e.is_directory {
                        if let Some(name) = image_name(&e.name) { out.push(ArchiveEntry { name, size: e.size }); }
                    }
                }
            }
        }
        out.sort_by(|a, b| crate::natural_cmp(&a.name, &b.name));
        Ok(out)
    })
}

pub fn read_entry(archive: &Path, name: &str) -> Result<Vec<u8>, String> {
    guarded(|| {
        if image_name(name).as_deref() != Some(name) { return Err(error("잘못된 그림 경로")); }
        match badge(archive) {
            "ZIP" => {
                let mut r = zip::ZipArchive::new(std::fs::File::open(archive).map_err(error)?).map_err(error)?;
                for i in 0..r.len() {
                    let mut e = r.by_index(i).map_err(error)?;
                    if e.name().replace('\\', "/") == name {
                        let mut bytes = Vec::new();
                        e.read_to_end(&mut bytes).map_err(error)?;
                        return Ok(bytes);
                    }
                }
            }
            "RAR" => {
                let mut r = unrar::Archive::new(archive).open_for_processing().map_err(error)?;
                while let Some(e) = r.read_header().map_err(error)? {
                    if e.entry().is_encrypted() { return Err("암호가 걸린 압축 파일입니다".into()); }
                    if e.entry().filename.to_string_lossy().replace('\\', "/") == name {
                        return e.read().map(|(bytes, _)| bytes).map_err(error);
                    }
                    r = e.skip().map_err(error)?;
                }
            }
            _ => {
                let mut r = seven(archive)?;
                let original = r.archive().files.iter().find(|e| e.name.replace('\\', "/") == name).map(|e| e.name.clone());
                if let Some(original) = original { return r.read_file(&original).map_err(error); }
            }
        }
        Err(error("그림을 찾을 수 없습니다"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!("picview-archive-{}-{}-{}", std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
        fn zip(&self) -> PathBuf {
            let p = self.0.join("a.cbz");
            let mut w = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
            for name in NAMES {
                w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
                w.write_all(&png()).unwrap();
            }
            w.finish().unwrap();
            p
        }
    }
    impl Drop for Temp { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    const NAMES: [&str; 8] = ["10.png", "2.png", "1.png", "notes.txt", "__MACOSX/._1.png", "sub/3.png", ".hidden.png", "sub/.hidden.png"];
    fn png() -> Vec<u8> {
        let mut b = Cursor::new(Vec::new());
        image::RgbaImage::new(3, 5).write_to(&mut b, image::ImageFormat::Png).unwrap();
        b.into_inner()
    }
    fn check(p: &Path) {
        let v = list_images(p).unwrap();
        assert_eq!(v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["1.png", "2.png", "10.png", "sub/3.png"]);
        assert!(v.iter().all(|e| e.size == png().len() as u64));
    }
    #[test] fn zip_lists_images_in_natural_order() { let t = Temp::new(); check(&t.zip()); }
    #[test] fn zip_read_entry_decodes() {
        let t = Temp::new();
        let b = read_entry(&t.zip(), "2.png").unwrap();
        let i = image::load_from_memory(&b).unwrap();
        assert_eq!((i.width(), i.height()), (3, 5));
    }
    #[test] fn sevenz_lists_and_reads() {
        let t = Temp::new();
        let p = t.0.join("a.cb7");
        let mut w = sevenz_rust2::ArchiveWriter::create(&p).unwrap();
        let source = t.0.join("source.png");
        std::fs::write(&source, png()).unwrap();
        for name in NAMES {
            w.push_archive_entry(sevenz_rust2::ArchiveEntry::from_path(&source, name.into()), Some(Cursor::new(png()))).unwrap();
        }
        w.finish().unwrap();
        check(&p);
        let i = image::load_from_memory(&read_entry(&p, "sub/3.png").unwrap()).unwrap();
        assert_eq!((i.width(), i.height()), (3, 5));
    }
    #[test] fn split_finds_archive_ancestor() {
        let t = Temp::new(); let p = t.zip();
        assert_eq!(split(&p.join("sub/x.jpg")), Some((p, "sub/x.jpg".into())));
        assert_eq!(split(&t.0.join("x.jpg")), None);
        let d = t.0.join("folder.cbz"); std::fs::create_dir(&d).unwrap();
        assert_eq!(split(&d.join("x.jpg")), None);
    }
    #[test] fn corrupt_archive_is_error_not_panic() {
        let t = Temp::new();
        for ext in ["zip", "rar", "7z"] {
            let p = t.0.join(format!("bad.{ext}")); std::fs::write(&p, b"bad archive").unwrap();
            assert!(list_images(&p).is_err());
        }
    }
    #[test] fn is_archive_ignores_case() {
        for p in ["A.CBZ", "b.Rar", "c.7Z"] { assert!(is_archive(Path::new(p))); }
        assert!(!is_archive(Path::new("d.png")));
    }
    #[test] fn sevenz_directory_entries_are_ignored() {
        let t = Temp::new(); let p = t.0.join("dirs.7z");
        let d = t.0.join("source"); std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("1.png"), png()).unwrap();
        let mut w = sevenz_rust2::ArchiveWriter::create(&p).unwrap();
        w.push_source_path(&d, |_| true).unwrap(); w.finish().unwrap();
        let entries = list_images(&p).unwrap(); assert_eq!(entries.len(), 1);
        assert_eq!(read_entry(&p, &entries[0].name).unwrap(), png());
    }
    #[test] fn virtual_gif_and_wic_decode_from_memory() {
        use image::{Delay, Frame, RgbaImage};
        let t = Temp::new(); let p = t.0.join("decoders.zip");
        let mut gif = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut gif);
            for _ in 0..2 { enc.encode_frame(Frame::from_parts(RgbaImage::new(3, 5), 0, 0, Delay::from_numer_denom_ms(50, 1))).unwrap(); }
        }
        let mut w = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
        w.start_file("a.gif", zip::write::SimpleFileOptions::default()).unwrap(); w.write_all(&gif).unwrap();
        w.start_file("a.heic", zip::write::SimpleFileOptions::default()).unwrap(); w.write_all(&png()).unwrap(); w.finish().unwrap();
        assert_eq!(crate::loader::load_gif_frames(&p.join("a.gif"), 100).unwrap().len(), 2);
        assert_eq!(crate::loader::load_full(&p.join("a.heic"), 100).unwrap().1, [3, 5]);
        assert_eq!(crate::loader::make_thumb(&p.join("a.heic")).unwrap().dimensions(), (3, 5));
    }
    #[test] fn encrypted_sevenz_reports_password_for_headers_and_content() {
        let t = Temp::new(); let source = t.0.join("1.png"); std::fs::write(&source, png()).unwrap();
        for encrypt_header in [true, false] {
            let p = t.0.join(format!("encrypted-{encrypt_header}.7z"));
            let mut w = sevenz_rust2::ArchiveWriter::create(&p).unwrap();
            w.set_content_methods(vec![sevenz_rust2::encoder_options::AesEncoderOptions::new("test-only".into()).into(),
                sevenz_rust2::encoder_options::Lzma2Options::from_level(1).into()]);
            w.set_encrypt_header(encrypt_header);
            w.push_archive_entry(sevenz_rust2::ArchiveEntry::from_path(&source, "1.png".into()), Some(Cursor::new(png()))).unwrap();
            w.finish().unwrap();
            assert_eq!(list_images(&p).unwrap_err(), "암호가 걸린 압축 파일입니다");
            assert_eq!(read_entry(&p, "1.png").unwrap_err(), "암호가 걸린 압축 파일입니다");
        }
    }
}
