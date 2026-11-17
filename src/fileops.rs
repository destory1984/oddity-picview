//! Windows shell glue: file clipboard (shared with Explorer), copy/move with
//! the native progress dialog, and the Properties dialog.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const DROPEFFECT_MOVE: u32 = 2;

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Double-NUL-terminated list, as SHFileOperation wants.
fn wide_list(paths: &[PathBuf]) -> Vec<u16> {
    let mut v = Vec::new();
    for p in paths {
        v.extend(p.as_os_str().encode_wide());
        v.push(0);
    }
    v.push(0);
    v
}

fn drop_effect_format() -> Option<u32> {
    clipboard_win::register_format("Preferred DropEffect").map(|f| f.get())
}

/// Put files on the clipboard the way Explorer does, so pasting works there too.
pub fn clipboard_set_files(paths: &[PathBuf], cut: bool) -> Result<(), String> {
    let _clip = clipboard_win::Clipboard::new_attempts(10).map_err(|e| e.to_string())?;
    clipboard_win::raw::empty().map_err(|e| e.to_string())?;
    let list: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    clipboard_win::raw::set_file_list(&list).map_err(|e| e.to_string())?;
    if let Some(fmt) = drop_effect_format() {
        let effect: u32 = if cut { DROPEFFECT_MOVE } else { 1 };
        clipboard_win::raw::set_without_clear(fmt, &effect.to_le_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Files on the clipboard and whether they were cut (move) rather than copied.
pub fn clipboard_get_files() -> Option<(Vec<PathBuf>, bool)> {
    let _clip = clipboard_win::Clipboard::new_attempts(10).ok()?;
    let mut files: Vec<PathBuf> = Vec::new();
    clipboard_win::raw::get_file_list_path(&mut files).ok()?;
    if files.is_empty() {
        return None;
    }
    let cut = drop_effect_format()
        .and_then(|f| {
            let mut v = Vec::new();
            clipboard_win::raw::get_vec(f, &mut v).ok()?;
            Some(v.len() >= 4 && u32::from_le_bytes([v[0], v[1], v[2], v[3]]) & DROPEFFECT_MOVE != 0)
        })
        .unwrap_or(false);
    Some((files, cut))
}

pub fn clipboard_has_files() -> bool {
    clipboard_win::raw::is_format_avail(15) // CF_HDROP
}

pub fn clipboard_clear() {
    if let Ok(_clip) = clipboard_win::Clipboard::new_attempts(10) {
        let _ = clipboard_win::raw::empty();
    }
}

/// Detect clipboard changes while archive extraction runs in the background.
pub fn clipboard_sequence() -> u32 {
    #[link(name = "user32")]
    extern "system" { fn GetClipboardSequenceNumber() -> u32; }
    unsafe { GetClipboardSequenceNumber() }
}

/// Copy or move with Explorer's own progress / overwrite dialogs. Blocks; run on a worker thread.
/// Returns Ok(false) if the user cancelled part of it.
pub fn shell_copy_move(srcs: &[PathBuf], dest_dir: &Path, mv: bool) -> Result<bool, String> {
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_RENAMEONCOLLISION, FO_COPY, FO_MOVE, SHFILEOPSTRUCTW,
    };
    // pasting a copy into its own folder makes "name - 복사본" like Explorer
    let same_dir = srcs.iter().any(|s| s.parent() == Some(dest_dir));
    let from = wide_list(srcs);
    let to = wide_list(&[dest_dir.to_path_buf()]);
    let mut flags = FOF_ALLOWUNDO as u16;
    if same_dir && !mv {
        flags |= FOF_RENAMEONCOLLISION as u16;
    }
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: if mv { FO_MOVE } else { FO_COPY },
        pFrom: from.as_ptr(),
        pTo: to.as_ptr(),
        fFlags: flags,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 {
        return Err(format!("오류 코드 0x{r:X}"));
    }
    Ok(op.fAnyOperationsAborted == 0)
}

/// Open Explorer's Properties dialog for a file or folder.
pub fn show_properties(path: &Path) {
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOW;
    let verb = wide(std::ffi::OsStr::new("properties"));
    let file = wide(path.as_os_str());
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_INVOKEIDLIST;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.nShow = SW_SHOW;
        ShellExecuteExW(&mut info);
    }
}

/// egui only reports Ctrl+V when the clipboard holds text, so read the key state directly.
pub fn ctrl_v_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL};
    unsafe { GetKeyState(VK_CONTROL as i32) < 0 && GetKeyState(b'V' as i32) < 0 }
}

/// Open with the program Windows associates with the file (video player, music player...).
pub fn open_default(path: &Path) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = wide(std::ffi::OsStr::new("open"));
    let file = wide(path.as_os_str());
    unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
    }
}

/// Thumbnail from the Windows shell, the same one Explorer shows: a frame for
/// videos, album art (or the file type icon) for audio. Safe to call from any thread.
pub fn shell_thumbnail(path: &Path, size: i32) -> Result<image::RgbaImage, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT};

    // the shell parser only takes backslashes
    let fixed = PathBuf::from(path.to_string_lossy().replace('/', "\\"));
    let wpath = wide(fixed.as_os_str());
    unsafe {
        // worker threads: join the multithreaded COM apartment (no-op if already in)
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(wpath.as_ptr()), None).map_err(|e| e.to_string())?;
        let hbmp = factory.GetImage(SIZE { cx: size, cy: size }, SIIGBF_RESIZETOFIT).map_err(|e| e.to_string())?;
        let obj = HGDIOBJ(hbmp.0);
        let mut bm = BITMAP::default();
        if GetObjectW(obj, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
            let _ = DeleteObject(obj);
            return Err("GetObject failed".into());
        }
        let (w, h) = (bm.bmWidth.max(1) as u32, bm.bmHeight.unsigned_abs().max(1));
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(None);
        let lines = GetDIBits(dc, hbmp, 0, h, Some(px.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(obj);
        if lines == 0 {
            return Err("GetDIBits failed".into());
        }
        let has_alpha = px.chunks_exact(4).any(|c| c[3] != 0);
        for c in px.chunks_exact_mut(4) {
            c.swap(0, 2); // BGRA -> RGBA
            if !has_alpha {
                c[3] = 255; // plain RGB bitmap
            } else if c[3] > 0 && c[3] < 255 {
                // shell bitmaps are premultiplied
                for k in 0..3 {
                    c[k] = ((c[k] as u32 * 255) / c[3] as u32).min(255) as u8;
                }
            }
        }
        image::RgbaImage::from_raw(w, h, px).ok_or_else(|| "bad bitmap".to_string())
    }
}

/// Decode with Windows Imaging Component: HEIC/HEIF, AVIF and camera RAW
/// (the same codecs the Photos app uses; HEIF/RAW need the free Store
/// extensions). Returns pixels (downscaled to fit `max_side`), the original
/// size, and the EXIF orientation (1..=8) if the file has one.
pub fn wic_decode(path: &Path, max_side: u32) -> Result<(image::RgbaImage, [u32; 2], Option<u8>), String> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapSource, IWICImagingFactory,
        WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
        WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

    let wpath = wide(path.as_os_str());
    let mut archive_bytes = crate::archive::split(path)
        .map(|(archive, name)| crate::archive::read_entry(&archive, &name)).transpose()?;
    let err = |e: windows::core::Error| {
        if e.code().0 as u32 == 0x88982F50 {
            "이 형식을 읽을 코덱이 없습니다 (Microsoft Store의 HEIF / Raw 이미지 확장 필요)".to_string()
        } else {
            e.message()
        }
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).map_err(err)?;
        let stream = factory.CreateStream().map_err(err)?;
        let decoder = if let Some(bytes) = archive_bytes.as_mut() {
            stream.InitializeFromMemory(bytes).map_err(err)?;
            factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).map_err(err)?
        } else {
            factory.CreateDecoderFromFilename(PCWSTR(wpath.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).map_err(err)?
        };
        let frame = decoder.GetFrame(0).map_err(err)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).map_err(err)?;
        let dims = [w, h];

        let orientation = frame.GetMetadataQueryReader().ok().and_then(|q| {
            let name: Vec<u16> = "System.Photo.Orientation".encode_utf16().chain([0]).collect();
            let mut v = PROPVARIANT::default();
            q.GetMetadataByName(PCWSTR(name.as_ptr()), &mut v).ok()?;
            u16::try_from(&v).ok().map(|o| o as u8).filter(|o| (1..=8).contains(o))
        });

        let mut source: IWICBitmapSource = frame.cast().map_err(err)?;
        if w > max_side || h > max_side {
            let k = max_side as f64 / w.max(h) as f64;
            let (nw, nh) = (((w as f64 * k) as u32).max(1), ((h as f64 * k) as u32).max(1));
            let scaler = factory.CreateBitmapScaler().map_err(err)?;
            scaler.Initialize(&source, nw, nh, WICBitmapInterpolationModeFant).map_err(err)?;
            source = scaler.cast().map_err(err)?;
            (w, h) = (nw, nh);
        }
        let conv = factory.CreateFormatConverter().map_err(err)?;
        conv.Initialize(&source, &GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .map_err(err)?;
        let mut px = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut px).map_err(err)?;
        let img = image::RgbaImage::from_raw(w, h, px).ok_or("bad size")?;
        Ok((img, dims, orientation))
    }
}

#[cfg(test)]
mod tests {
    /// PICVIEW_MEDIA=<file> PICVIEW_OUT=<png> cargo test --release shell_thumb -- --ignored
    #[test]
    #[ignore]
    fn shell_thumb() {
        let p = std::env::var("PICVIEW_MEDIA").expect("set PICVIEW_MEDIA");
        let img = super::shell_thumbnail(std::path::Path::new(&p), 256).expect("thumbnail");
        println!("{}x{}", img.width(), img.height());
        if let Ok(out) = std::env::var("PICVIEW_OUT") {
            img.save(out).unwrap();
        }
    }

    /// PICVIEW_MEDIA=<heic or raw> PICVIEW_OUT=<png> cargo test --release wic_full -- --ignored --nocapture
    #[test]
    #[ignore]
    fn wic_full() {
        let p = std::env::var("PICVIEW_MEDIA").expect("set PICVIEW_MEDIA");
        let (img, dims, orient) = super::wic_decode(std::path::Path::new(&p), 1024).expect("decode");
        println!("{dims:?} -> {}x{} orientation {orient:?}", img.width(), img.height());
        if let Ok(out) = std::env::var("PICVIEW_OUT") {
            img.save(out).unwrap();
        }
    }
}

/// Hand files to Windows' own drag-and-drop so they can be dropped on other
/// programs (browser, Explorer, chat apps). Blocks until the drop ends; call on
/// the UI thread (winit has already set it up for OLE). Returns the effect the
/// target chose: 0 = cancelled, 1 = copy, 2 = move, 4 = link.
pub fn drag_out(hwnd: windows::Win32::Foundation::HWND, paths: &[PathBuf], copy_only: bool) -> Result<u32, String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        BHID_DataObject, ILCreateFromPathW, ILFree, IShellItemArray, SHCreateShellItemArrayFromIDLists, SHDoDragDrop,
    };

    let mut pidls: Vec<*const ITEMIDLIST> = Vec::new();
    for p in paths {
        let fixed = PathBuf::from(p.to_string_lossy().replace('/', "\\"));
        let w = wide(fixed.as_os_str());
        let pidl = unsafe { ILCreateFromPathW(PCWSTR(w.as_ptr())) };
        if !pidl.is_null() {
            pidls.push(pidl);
        }
    }
    if pidls.is_empty() {
        return Err("끌 파일이 없습니다".into());
    }
    let result = unsafe {
        (|| -> windows::core::Result<u32> {
            let items: IShellItemArray = SHCreateShellItemArrayFromIDLists(&pidls)?;
            let data: IDataObject = items.BindToHandler(None, &BHID_DataObject)?;
            // copy or link only: a browser or chat app must never "move" (delete) the original
            let allowed = if copy_only { DROPEFFECT_COPY } else { DROPEFFECT_COPY | DROPEFFECT_LINK };
            let effect: DROPEFFECT = SHDoDragDrop(Some(hwnd), &data, None, allowed)?;
            Ok(effect.0)
        })()
    };
    for p in pidls {
        unsafe { ILFree(Some(p)) };
    }
    result.map_err(|e| e.to_string())
}

/// After an OLE drag loop the window never sees the left button come up, so
/// egui would think it is still held. Post the release ourselves.
pub fn post_left_button_up(hwnd: windows::Win32::Foundation::HWND) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_LBUTTONUP};
    unsafe {
        if !hwnd.0.is_null() {
            PostMessageW(hwnd.0, WM_LBUTTONUP, 0, 0);
        }
    }
}

/// Play length as Explorer shows it (the "길이" column). More exact than the
/// decoder's guess for MP3s, which estimates from the bitrate.
pub fn media_duration(path: &Path) -> Option<std::time::Duration> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::EnhancedStorage::PKEY_Media_Duration;
    use windows::Win32::UI::Shell::{IShellItem2, SHCreateItemFromParsingName};
    let fixed = PathBuf::from(path.to_string_lossy().replace('/', "\\"));
    let w = wide(fixed.as_os_str());
    unsafe {
        let item: IShellItem2 = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let hns = item.GetUInt64(&PKEY_Media_Duration).ok()?; // 100 ns units
        (hns > 0).then(|| std::time::Duration::from_nanos(hns * 100))
    }
}

/// `n` frames spread over a video (at 5%, 15%, ... 95% of its length), each
/// scaled to `width` pixels wide. Uses Windows' own decoders (Media
/// Foundation), so it reads what the Films & TV app reads. Run on a worker thread.
pub fn video_frames(path: &Path, n: usize, width: u32) -> Result<Vec<image::RgbaImage>, String> {
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Variant::VT_I8;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    let fixed = PathBuf::from(path.to_string_lossy().replace('/', "\\"));
    let wpath = wide(fixed.as_os_str());
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| e.to_string())?;
        let res = (|| -> windows::core::Result<Vec<image::RgbaImage>> {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 2)?;
            let attrs = attrs.ok_or_else(windows::core::Error::empty)?;
            // the reader converts to RGB32 and scales down itself (much cheaper than
            // converting full-size frames), and may use the GPU's video decoder
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            let reader = MFCreateSourceReaderFromURL(PCWSTR(wpath.as_ptr()), &attrs)?;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(stream, true)?;
            let native = reader.GetNativeMediaType(stream, 0)?;
            let ns = native.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let (nw, nh) = ((ns >> 32) as u32, (ns & 0xffff_ffff) as u32);
            let want = MFCreateMediaType()?;
            want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            if nw > width && nh > 0 {
                let h = ((width as u64 * nh as u64) / nw as u64).max(2) as u32 & !1;
                want.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | h as u64)?;
            }
            reader.SetCurrentMediaType(stream, None, &want)?;
            let cur = reader.GetCurrentMediaType(stream)?;
            let fs = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let (fw, fh) = ((fs >> 32) as u32, (fs & 0xffff_ffff) as u32);
            if fw == 0 || fh == 0 {
                return Err(windows::core::Error::empty());
            }
            let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or(fw as i32 * 4);
            let dur_pv = reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)?;
            let dur = dur_pv.Anonymous.Anonymous.Anonymous.uhVal; // VT_UI8, 100 ns units
            let height = ((width as u64 * fh as u64) / fw as u64).max(1) as u32;

            let mut out = Vec::new();
            for k in 0..n {
                let mut at = 0i64;
                if dur > 0 {
                    at = (dur as f64 * (k as f64 + 0.5) / n as f64) as i64;
                    let mut pos = PROPVARIANT::default();
                    (*pos.Anonymous.Anonymous).vt = VT_I8;
                    (*pos.Anonymous.Anonymous).Anonymous.hVal = at;
                    reader.SetCurrentPosition(&GUID::zeroed(), &pos)?;
                }
                // seeking lands on the keyframe before `at`: decode forward until
                // the frame at `at` (at most 400 frames, about 15 s of 25 fps video)
                let mut sample: Option<IMFSample> = None;
                for _ in 0..400 {
                    let mut flags = 0u32;
                    let mut ts = 0i64;
                    let mut got: Option<IMFSample> = None;
                    reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut got))?;
                    if got.is_some() {
                        sample = got;
                    }
                    if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 || (sample.is_some() && ts >= at) {
                        break;
                    }
                }
                let Some(sample) = sample else { break };
                let buf = sample.ConvertToContiguousBuffer()?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut ptr, None, Some(&mut len))?;
                let data = std::slice::from_raw_parts(ptr, len as usize);
                let row = stride.unsigned_abs() as usize;
                let mut img = image::RgbaImage::new(fw, fh);
                for y in 0..fh as usize {
                    // negative stride = bottom-up rows
                    let src_y = if stride < 0 { fh as usize - 1 - y } else { y };
                    let Some(src) = data.get(src_y * row..src_y * row + fw as usize * 4) else { break };
                    for (x, px) in src.chunks_exact(4).enumerate() {
                        img.put_pixel(x as u32, y as u32, image::Rgba([px[2], px[1], px[0], 255])); // BGRX
                    }
                }
                let _ = buf.Unlock();
                out.push(image::imageops::resize(&img, width, height, image::imageops::FilterType::Triangle));
            }
            Ok(out)
        })();
        let _ = MFShutdown();
        match res {
            Ok(v) if !v.is_empty() => Ok(v),
            Ok(_) => Err("프레임이 없습니다".into()),
            Err(e) => Err(e.to_string()),
        }
    }
}
