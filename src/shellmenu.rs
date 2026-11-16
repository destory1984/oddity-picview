use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CMIC_MASK_UNICODE: u32 = SEE_MASK_UNICODE;
const SHELL_FIRST: u32 = 1;
const SHELL_LAST: u32 = 0x6fff;
const EXTRA_FIRST: u32 = 0x8000;

#[derive(Debug, PartialEq, Eq)]
pub enum Intercept {
    Rename,
    Delete,
    Cut,
    Copy,
    Paste,
}

pub fn intercept(verb: &str) -> Option<Intercept> {
    match verb.to_ascii_lowercase().as_str() {
        "rename" => Some(Intercept::Rename),
        "delete" => Some(Intercept::Delete),
        "cut" => Some(Intercept::Cut),
        "copy" => Some(Intercept::Copy),
        "paste" => Some(Intercept::Paste),
        _ => None,
    }
}

pub struct ExtraItem {
    pub id: u32,
    pub label: String,
    pub enabled: bool,
    pub submenu: Vec<ExtraItem>,
}

pub enum MenuResult {
    None,
    Extra(u32),
    Intercepted(Intercept),
    InvokedShell,
}

// Cancellation and failure share the public result, but only failures need fallback.
thread_local! { static FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub fn failed() -> bool {
    FAILED.with(|f| f.get())
}

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe {
            ILFree(Some(self.0));
        }
    }
}
fn pidl(path: &Path) -> Option<Pidl> {
    let fixed = path.as_os_str().to_string_lossy().replace('/', "\\");
    let wide: Vec<u16> = std::ffi::OsStr::new(&fixed)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let p = unsafe { ILCreateFromPathW(PCWSTR(wide.as_ptr())) };
    (!p.is_null()).then_some(Pidl(p))
}

struct Popup(windows_sys::Win32::UI::WindowsAndMessaging::HMENU);
impl Popup {
    fn new() -> Option<Self> {
        let h = unsafe { CreatePopupMenu() };
        (!h.is_null()).then_some(Self(h))
    }
}
impl Drop for Popup {
    fn drop(&mut self) {
        unsafe {
            DestroyMenu(self.0);
        }
    }
}

struct BuiltMenu {
    popup: Popup,
    context: IContextMenu,
    #[cfg(test)]
    shell_count: u32,
    extras: Vec<(u32, u32)>,
}

fn append_extras(menu: &Popup, items: &[ExtraItem], ids: &mut Vec<(u32, u32)>) -> Option<()> {
    for item in items {
        let label: Vec<u16> = item.label.encode_utf16().chain(Some(0)).collect();
        let state = if item.enabled { MF_ENABLED } else { MF_GRAYED };
        let ok = if item.submenu.is_empty() {
            let id = EXTRA_FIRST.checked_add(ids.len() as u32)?;
            if id > 0xffff {
                return None;
            }
            ids.push((id, item.id));
            unsafe { AppendMenuW(menu.0, MF_STRING | state, id as usize, label.as_ptr()) }
        } else {
            let child = Popup::new()?;
            append_extras(&child, &item.submenu, ids)?;
            let ok =
                unsafe { AppendMenuW(menu.0, MF_POPUP | state, child.0 as usize, label.as_ptr()) };
            if ok != 0 {
                std::mem::forget(child);
            }
            ok
        };
        if ok == 0 {
            return None;
        }
    }
    Some(())
}

fn build_menu(context: IContextMenu, extra: &[ExtraItem]) -> Option<BuiltMenu> {
    let popup = Popup::new()?;
    let mut extras = Vec::new();
    append_extras(&popup, extra, &mut extras)?;
    if !extra.is_empty() && unsafe { AppendMenuW(popup.0, MF_SEPARATOR, 0, std::ptr::null()) } == 0
    {
        return None;
    }
    let index = unsafe { GetMenuItemCount(popup.0) };
    if index < 0 {
        return None;
    }
    let mut flags = CMF_NORMAL | CMF_EXPLORE | CMF_CANRENAME;
    if unsafe { GetKeyState(VK_SHIFT as i32) } < 0 {
        flags |= CMF_EXTENDEDVERBS;
    }
    let hr = unsafe {
        context.QueryContextMenu(
            windows::Win32::UI::WindowsAndMessaging::HMENU(popup.0),
            index as u32,
            SHELL_FIRST,
            SHELL_LAST,
            flags,
        )
    };
    if hr.is_err() {
        return None;
    }
    Some(BuiltMenu {
        popup,
        context,
        #[cfg(test)]
        shell_count: (hr.0 as u32) & 0xffff,
        extras,
    })
}

fn items_context(hwnd: HWND, items: &[PathBuf]) -> Option<IContextMenu> {
    let first = items.first()?;
    if items.iter().any(|p| p.parent() != first.parent()) {
        return None;
    }
    let pidls: Vec<Pidl> = items.iter().map(|p| pidl(p)).collect::<Option<_>>()?;
    let pointers: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| p.0 as *const _).collect();
    unsafe {
        let array: IShellItemArray = SHCreateShellItemArrayFromIDLists(&pointers).ok()?;
        let _ = hwnd;
        array.BindToHandler(None, &BHID_SFUIObject).ok()
    }
}

struct Forwarder {
    v3: Option<IContextMenu3>,
    v2: Option<IContextMenu2>,
}
unsafe extern "system" fn subclass(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wp: usize,
    lp: isize,
    _id: usize,
    data: usize,
) -> isize {
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let f = &*(data as *const Forwarder);
        if let Some(c) = &f.v3 {
            let mut result = windows::Win32::Foundation::LRESULT(0);
            if c.HandleMenuMsg2(
                msg,
                windows::Win32::Foundation::WPARAM(wp),
                windows::Win32::Foundation::LPARAM(lp),
                Some(&mut result),
            )
            .is_ok()
            {
                return result.0;
            }
        } else if let Some(c) = &f.v2 {
            if c.HandleMenuMsg(
                msg,
                windows::Win32::Foundation::WPARAM(wp),
                windows::Win32::Foundation::LPARAM(lp),
            )
            .is_ok()
            {
                return if matches!(msg, WM_DRAWITEM | WM_MEASUREITEM) {
                    1
                } else {
                    0
                };
            }
        }
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}
struct SubclassGuard {
    hwnd: HWND,
    _forwarder: Box<Forwarder>,
}
impl SubclassGuard {
    fn new(hwnd: HWND, c: &IContextMenu) -> Option<Self> {
        let f = Box::new(Forwarder {
            v3: c.cast().ok(),
            v2: c.cast().ok(),
        });
        if unsafe {
            SetWindowSubclass(
                hwnd.0,
                Some(subclass),
                subclass as *const () as usize,
                &*f as *const _ as usize,
            )
        } == 0
        {
            return None;
        }
        Some(Self {
            hwnd,
            _forwarder: f,
        })
    }
}
impl Drop for SubclassGuard {
    fn drop(&mut self) {
        unsafe {
            RemoveWindowSubclass(self.hwnd.0, Some(subclass), subclass as *const () as usize);
        }
    }
}

fn verb(context: &IContextMenu, offset: u32) -> Option<String> {
    let mut buffer = [0u16; 256];
    unsafe {
        context
            .GetCommandString(
                offset as usize,
                GCS_VERBW,
                None,
                PSTR(buffer.as_mut_ptr().cast()),
                buffer.len() as u32,
            )
            .ok()?;
    }
    Some(String::from_utf16_lossy(
        &buffer[..buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len())],
    ))
}

fn show(
    hwnd: HWND,
    pos: (i32, i32),
    context: Option<IContextMenu>,
    extra: &[ExtraItem],
) -> MenuResult {
    FAILED.with(|f| f.set(true));
    let Some(menu) = context.and_then(|c| build_menu(c, extra)) else {
        return MenuResult::None;
    };
    let Some(guard) = SubclassGuard::new(hwnd, &menu.context) else {
        return MenuResult::None;
    };
    FAILED.with(|f| f.set(false));
    unsafe {
        SetForegroundWindow(hwnd.0);
        SetLastError(0);
    }
    let command = unsafe {
        TrackPopupMenuEx(
            menu.popup.0,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pos.0,
            pos.1,
            hwnd.0,
            std::ptr::null(),
        )
    } as u32;
    let track_failed = command == 0 && unsafe { GetLastError() } != 0;
    unsafe {
        PostMessageW(hwnd.0, WM_NULL, 0, 0);
    }
    drop(guard);
    unsafe {
        PostMessageW(hwnd.0, WM_RBUTTONUP, 0, 0);
    }
    if command == 0 {
        FAILED.with(|f| f.set(track_failed));
        return MenuResult::None;
    }
    if let Some((_, id)) = menu.extras.iter().find(|(native, _)| *native == command) {
        return MenuResult::Extra(*id);
    }
    if !(SHELL_FIRST..=SHELL_LAST).contains(&command) {
        return MenuResult::None;
    }
    let offset = command - SHELL_FIRST;
    if let Some(action) = verb(&menu.context, offset).and_then(|v| intercept(&v)) {
        return MenuResult::Intercepted(action);
    }
    let mut info = CMINVOKECOMMANDINFOEX::default();
    info.cbSize = std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32;
    info.fMask = CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE;
    if unsafe { GetKeyState(VK_CONTROL as i32) } < 0 {
        info.fMask |= CMIC_MASK_CONTROL_DOWN;
    }
    if unsafe { GetKeyState(VK_SHIFT as i32) } < 0 {
        info.fMask |= CMIC_MASK_SHIFT_DOWN;
    }
    info.hwnd = hwnd;
    info.lpVerb = PCSTR(offset as usize as *const u8);
    info.lpVerbW = PCWSTR(offset as usize as *const u16);
    info.nShow = SW_SHOWNORMAL;
    info.ptInvoke = POINT { x: pos.0, y: pos.1 };
    if unsafe {
        menu.context
            .InvokeCommand((&info as *const CMINVOKECOMMANDINFOEX).cast())
    }
    .is_err()
    {
        FAILED.with(|f| f.set(true));
        return MenuResult::None;
    }
    MenuResult::InvokedShell
}

pub fn show_for_items(
    hwnd: HWND,
    screen_pos: (i32, i32),
    items: &[PathBuf],
    extra: &[ExtraItem],
) -> MenuResult {
    show(hwnd, screen_pos, items_context(hwnd, items), extra)
}

pub fn show_for_folder_background(
    hwnd: HWND,
    screen_pos: (i32, i32),
    dir: &Path,
    extra: &[ExtraItem],
) -> MenuResult {
    let context = (|| unsafe {
        let p = pidl(dir)?;
        let desktop: IShellFolder = SHGetDesktopFolder().ok()?;
        let folder: IShellFolder = desktop.BindToObject(p.0, None).ok()?;
        folder.CreateViewObject(hwnd).ok()
    })();
    show(hwnd, screen_pos, context, extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intercepts_our_verbs() {
        for (verb, expected) in [
            ("rename", Intercept::Rename),
            ("DELETE", Intercept::Delete),
            ("cut", Intercept::Cut),
            ("copy", Intercept::Copy),
            ("paste", Intercept::Paste),
        ] {
            assert_eq!(intercept(verb), Some(expected));
        }
    }

    #[test]
    fn leaves_other_verbs_to_shell() {
        for verb in ["open", "properties", "edit", "print", ""] {
            assert_eq!(intercept(verb), None);
        }
    }

    #[test]
    fn builds_menu_for_real_file() {
        use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
        unsafe {
            OleInitialize(None).unwrap();
        }
        let path =
            std::env::temp_dir().join(format!("picview-shellmenu-{}.png", std::process::id()));
        image::RgbaImage::new(1, 1).save(&path).unwrap();
        let result = build_test_menu(&[path.clone()]);
        std::fs::remove_file(path).unwrap();
        unsafe {
            OleUninitialize();
        }
        let (count, verbs) = result.expect("shell menu must build");
        assert!(count >= 5, "only {count} items");
        assert!(verbs.iter().any(|v| v == "properties"), "{verbs:?}");
    }
}

#[cfg(test)]
fn build_test_menu(items: &[PathBuf]) -> Option<(i32, Vec<String>)> {
    let menu = build_menu(items_context(HWND::default(), items)?, &[])?;
    let count = unsafe { GetMenuItemCount(menu.popup.0) };
    let verbs = (0..menu.shell_count)
        .filter_map(|i| verb(&menu.context, i))
        .collect();
    Some((count, verbs))
}
