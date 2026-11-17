#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod archive;
mod cache;
mod fileops;
mod icons;
mod loader;
mod meta;
mod model;
mod rotate;
mod pane;
mod watch;
mod shellmenu;

use pane::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use eframe::egui::{
    self, collapsing_header::CollapsingState, load::SizedTexture, pos2, text::LayoutJob, vec2,
    Align, Align2, Color32, ColorImage, FontFamily, FontId, Id, Key, Rect, Sense, Stroke,
    TextFormat, TextureHandle, TextureOptions, Ui, Vec2,
};
use std::cmp::Ordering as CmpOrd;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

/// Folder name for settings (%APPDATA%) and the thumbnail cache (%LOCALAPPDATA%).
pub const APP_ID: &str = "oddity-picview";
const AUTHOR: &str = "destory1984";
const REPO_URL: &str = "https://github.com/destory1984/oddity-picview";

const MAX_THUMBS_LOADED: usize = 800;
const BIG_CACHE: usize = 8;
const SCROLL_ANIM: Duration = Duration::from_millis(120);
const PREVIEW_DELAY: Duration = Duration::from_millis(120);
/// Video preview: frames taken over the whole length, shown in a loop.
const CLIP_FRAMES: usize = 10;
const CLIP_W: u32 = 400;
const CLIP_FRAME_TIME: Duration = Duration::from_millis(700);
/// How long the pointer must rest on a video thumbnail before it plays.
const CLIP_HOVER: Duration = Duration::from_millis(300);
/// Video previews kept in memory (10 textures each).
const CLIP_CACHE: usize = 24;

fn overlay_alpha(idle: Duration) -> f32 {
    if idle < Duration::from_millis(1500) {
        1.0
    } else {
        (1.0 - idle.saturating_sub(Duration::from_millis(1500)).as_secs_f32() / 0.3).max(0.0)
    }
}

fn hide_cursor(fullscreen: bool, idle: Duration) -> bool {
    fullscreen && idle >= Duration::from_secs(2)
}

fn fullscreen_menu_label(fullscreen: bool) -> &'static str {
    if fullscreen { "창으로 보기" } else { "전체 화면으로 보기" }
}

#[cfg(test)]
mod fullscreen_tests {
    use super::{fullscreen_menu_label, hide_cursor, overlay_alpha};
    use std::time::Duration;

    #[test]
    fn fullscreen_label_follows_state() {
        assert_eq!(fullscreen_menu_label(false), "전체 화면으로 보기");
        assert_eq!(fullscreen_menu_label(true), "창으로 보기");
    }

    #[test]
    fn overlay_visible_while_moving() {
        assert_eq!(overlay_alpha(Duration::ZERO), 1.0);
        assert_eq!(overlay_alpha(Duration::from_millis(1499)), 1.0);
    }

    #[test]
    fn overlay_fades_out() {
        assert!((0.4..=0.6).contains(&overlay_alpha(Duration::from_millis(1650))));
        assert_eq!(overlay_alpha(Duration::from_millis(1800)), 0.0);
        assert_eq!(overlay_alpha(Duration::from_secs(5)), 0.0);
    }

    #[test]
    fn cursor_hidden_only_in_fullscreen_after_2s() {
        assert!(!hide_cursor(true, Duration::from_millis(1999)));
        assert!(hide_cursor(true, Duration::from_secs(2)));
        assert!(!hide_cursor(false, Duration::from_secs(10)));
    }
}

// ---------------------------------------------------------------- data types

/// Which kinds of files the list shows. Folders always stay.
#[derive(Clone, Copy, PartialEq)]
enum KindFilter {
    All,
    Image,
    Video,
    Audio,
}

impl KindFilter {
    const ALL: [KindFilter; 4] = [KindFilter::All, KindFilter::Image, KindFilter::Video, KindFilter::Audio];

    fn label(self) -> &'static str {
        match self {
            KindFilter::All => "전체",
            KindFilter::Image => "그림",
            KindFilter::Video => "동영상",
            KindFilter::Audio => "음성",
        }
    }

    fn keeps(self, e: &Entry) -> bool {
        e.is_dir
            || match self {
                KindFilter::All => true,
                KindFilter::Image => e.is_img() || archive::is_archive(&e.path),
                KindFilter::Video => e.media == Some(loader::Media::Video),
                KindFilter::Audio => e.media == Some(loader::Media::Audio),
            }
    }
}

enum Big {
    Pending,
    Ready(TextureHandle, [u32; 2]),
    Failed,
}

enum Msg {
    ArchiveCopied { request: u64, sequence: u32, res: Result<Vec<PathBuf>, String> },
    Exif { key: MetaKey, request: u64, summary: Option<Arc<meta::ExifSummary>> },
    Stripped { paths: Vec<PathBuf>, status: String },
    ArchiveListed { pane: usize, request: u64, path: PathBuf, stamp: Option<(u64, SystemTime)>, res: Result<Vec<archive::ArchiveEntry>, String> },
    DirChanged { dir: PathBuf },
    Thumb { pane: usize, gen: u64, idx: usize, mtime: SystemTime, tex: TextureHandle },
    ThumbFail { pane: usize, gen: u64, idx: usize },
    Skip { pane: usize, gen: u64, idx: usize },
    Big { path: PathBuf, stamp: Option<(u64, SystemTime)>, res: Result<(TextureHandle, [u32; 2]), String> },
    Rotated { path: PathBuf, turns: i32, res: Result<(), String> },
    /// frames of an animated GIF, sent after its first frame (`Big`)
    Anim { path: PathBuf, stamp: Option<(u64, SystemTime)>, frames: Vec<(TextureHandle, Duration)> },
    /// video preview frames (None: Windows could not decode it)
    Clip { path: PathBuf, stamp: Option<(u64, SystemTime)>, frames: Option<Vec<TextureHandle>> },
    OpDone { status: String, dirs: Vec<PathBuf>, clear_clipboard: bool },
}

/// Commands from menus and shortcuts. Queued and run after the frame is drawn,
/// because most of them change `entries` or the folder tree.
enum Action {
    StripPrivate(Vec<PathBuf>),
    Open(usize, usize),
    ToggleFullscreen,
    CloseViewer,
    NewFolder(PathBuf),
    AddFavorite(PathBuf),
    RemoveFavorite(PathBuf),
    Cut(Vec<PathBuf>),
    Copy(Vec<PathBuf>),
    Paste(PathBuf),
    Delete(Vec<PathBuf>),
    Rename(PathBuf),
    Properties(PathBuf),
    Refresh(PathBuf),
    Rotate(Vec<PathBuf>, i32),
    ShowInExplorer(PathBuf),
    Go(PathBuf),
    Up,
    /// copy (false) or move (true) files into a folder
    Transfer(Vec<PathBuf>, PathBuf, bool),
    /// ask for a folder with the system dialog, then copy / move there
    PickAndTransfer(Vec<PathBuf>, bool),
}

/// Drag-and-drop payload: the files being dragged.
struct DragFiles(Vec<PathBuf>);

const TRANSFER_MAX: usize = 10;

/// Range of the thumbnail cache limit in the settings window, in MB.
const CACHE_MIN_MB: u64 = 100;
const CACHE_MAX_MB: u64 = 1024;

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
enum SortKey {
    Name,
    Date,
    Size,
}

const RECENT_MAX: usize = 10;

/// Saved between runs (eframe storage, %APPDATA%/oddity-picview/data/app.ron).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    last_dir: Option<PathBuf>,
    pane_dirs: Vec<PathBuf>,
    split: Split,
    active_pane: usize,
    recent: Vec<PathBuf>,
    sort: SortKey,
    sort_desc: bool,
    thumb_size: f32,
    favorites: Vec<PathBuf>,
    /// last 10 folders files were copied / moved into
    transfer_dirs: Vec<PathBuf>,
    /// thumbnail cache limit; trimmed to 80% of it at start-up
    cache_max_mb: u64,
    /// ask before sending files to the recycle bin
    confirm_delete: bool,
    /// make and play 10-frame video previews
    video_preview: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            last_dir: None,
            pane_dirs: Vec::new(),
            split: Split::One,
            active_pane: 0,
            recent: Vec::new(),
            sort: SortKey::Name,
            sort_desc: false,
            thumb_size: 160.0,
            favorites: Vec::new(),
            transfer_dirs: Vec::new(),
            cache_max_mb: cache::DEFAULT_MAX_MB,
            confirm_delete: true,
            video_preview: true,
        }
    }
}

/// Animated GIF being shown: frame textures with their display times.
struct Anim {
    frames: Vec<(TextureHandle, Duration)>,
    total: Duration,
    start: Instant,
}

/// Video preview: frames over the whole video, looped like a GIF.
enum Clip {
    Pending,
    Ready(Anim),
    Failed,
}

struct Viewer {
    pane: usize,
    idx: usize,
    zoom: Option<f32>, // None = fit to window
    pan: Vec2,
}

enum Dialog {
    StripPrivate(Vec<PathBuf>),
    Delete(Vec<PathBuf>),
    Rename { path: PathBuf, text: String, first: bool },
    NewFolder { parent: PathBuf, text: String, first: bool },
}

// ---------------------------------------------------------------- app

struct App {
    meta_cache: HashMap<MetaKey, MetaState>,
    meta_request: u64,
    privacy_saving: HashSet<PathBuf>,
    transfers_running: usize,
    archive_catalog: ArchiveCatalog,
    hwnd: windows::Win32::Foundation::HWND,
    panes: Vec<Pane>,
    split: Split,
    active: usize,
    ctx: egui::Context,

    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    watch: Option<watch::DirWatch>,
    dir_changes: HashMap<PathBuf, (Instant, Instant)>,

    thumb_size: f32,

    sort: SortKey,
    sort_desc: bool,

    big: HashMap<PathBuf, Big>,
    big_order: VecDeque<PathBuf>,
    anims: HashMap<PathBuf, Anim>,
    rot: HashMap<PathBuf, i32>,

    viewer: Option<Viewer>,
    fullscreen: bool,
    viewer_last_motion: Option<Instant>,
    viewer_menu_pending: Option<egui::Pos2>,
    dialog: Option<Dialog>,
    status: String,

    drives: Vec<PathBuf>,
    tree_children: HashMap<PathBuf, Vec<PathBuf>>,
    tree_scroll_pending: bool,
    recent: Vec<PathBuf>,
    favorites: Vec<PathBuf>,
    actions: Vec<Action>,
    /// files cut to the clipboard by us, drawn faded until pasted
    cut_paths: HashSet<PathBuf>,
    clipboard_request: u64,
    ctrl_v_prev: bool,
    /// rotation (quarter turns) being written to each file right now
    rot_saving: HashMap<PathBuf, i32>,
    /// rotation already written to the file but not to this loaded texture
    baked: HashMap<egui::TextureId, i32>,
    icons: Arc<icons::Icons>,
    transfer_dirs: Vec<PathBuf>,
    /// folder under the pointer while dragging files (set by drop targets each frame)
    dnd_target: Option<PathBuf>,
    dnd_folder_hover: bool,
    cache_max_mb: u64,
    confirm_delete: bool,
    /// settings: video previews on / off
    video_preview: bool,
    show_settings: bool,
    show_about: bool,
    /// bytes in the thumbnail cache, measured in the background (u64::MAX = not yet)
    cache_usage: Arc<AtomicU64>,
    /// audio preview (Space on a sound file); stops when the focus moves
    player: Option<audio::Player>,
    /// video previews by path, most recently used last in `clip_order`
    clips: HashMap<PathBuf, Clip>,
    clip_order: VecDeque<PathBuf>,
    /// file kind shown in the list (address bar buttons)
    kind_filter: KindFilter,
}

struct ArchiveListing {
    stamp: Option<(u64, SystemTime)>,
    images: std::sync::OnceLock<Result<Vec<archive::ArchiveEntry>, String>>,
}
type ArchiveCatalog = Arc<std::sync::Mutex<HashMap<PathBuf, Arc<ArchiveListing>>>>;
type MetaKey = (PathBuf, Option<(u64, SystemTime)>);

enum MetaState {
    Pending(u64),
    Ready(Option<Arc<meta::ExifSummary>>),
}

fn archive_stamp(path: &Path) -> Option<(u64, SystemTime)> {
    std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
}

fn watch_dir(path: &Path) -> PathBuf {
    if archive::is_location(path) { path.parent().unwrap_or(path).to_path_buf() }
    else { path.to_path_buf() }
}

fn archive_read_only(path: &Path) -> bool {
    archive::split(path).is_some()
}

fn archive_action_error(a: &Action) -> Option<&'static str> {
    match a {
        Action::Cut(ps) | Action::PickAndTransfer(ps, true) | Action::Transfer(ps, _, true)
            if ps.iter().any(|p| archive_read_only(p)) =>
                return Some("압축 파일 안의 그림은 옮길 수 없습니다. 복사만 됩니다"),
        _ => {}
    }
    let blocked = match a {
        Action::Delete(ps) => ps.iter().any(|p| archive_read_only(p)),
        Action::Rename(p) | Action::Properties(p) | Action::ShowInExplorer(p) => archive_read_only(p),
        Action::Paste(p) | Action::NewFolder(p) => archive::is_location(p) || archive_read_only(p),
        Action::Transfer(_, d, _) => archive::is_location(d) || archive_read_only(d),
        _ => false,
    };
    blocked.then_some("압축 파일 안에서는 할 수 없습니다")
}

fn cached_archive(catalog: &ArchiveCatalog, path: &Path) -> Result<Vec<archive::ArchiveEntry>, String> {
    let stamp = archive_stamp(path);
    let listing = {
        let mut cache = catalog.lock().map_err(|_| "압축 파일을 읽을 수 없습니다: 목록 잠금 실패".to_string())?;
        if cache.get(path).is_some_and(|old| old.stamp != stamp) { cache.remove(path); }
        if cache.len() >= 128 && !cache.contains_key(path) { cache.clear(); }
        cache.entry(path.to_path_buf()).or_insert_with(|| Arc::new(ArchiveListing {
            stamp, images: std::sync::OnceLock::new(),
        })).clone()
    };
    listing.images.get_or_init(|| archive::list_images(path)).clone()
}

impl App {
    fn reject_archive(&mut self, paths: &[PathBuf]) -> bool {
        let blocked = paths.iter().any(|p| archive_read_only(p));
        if blocked { self.status = "압축 파일 안에서는 할 수 없습니다".into(); }
        blocked
    }
    fn pane(&self) -> &Pane { &self.panes[self.active] }

    fn pane_mut(&mut self) -> &mut Pane { &mut self.panes[self.active] }

    fn activate(&mut self, pi: usize) {
        if pi >= self.panes.len() || pi == self.active { return; }
        self.player = None;
        self.active = pi;
        self.panes[pi].addr_text = self.panes[pi].cur_dir.display().to_string();
        self.open_tree_to(&self.panes[pi].cur_dir.clone());
    }

    fn set_split(&mut self, s: Split) {
        if self.split == s { return; }
        self.split = s;
        let n = s.len();
        let dir = self.pane().cur_dir.clone();
        let result = shrink_panes(PaneCountChange {
            active: self.active, viewer_pane: self.viewer.as_ref().map(|v| v.pane), n,
        });
        if result.close_viewer { self.viewer = None; }
        self.activate(result.active);
        // Cancel old jobs and give reused indices a fresh generation.
        for pane in self.panes.iter().skip(n) {
            pane.gen.fetch_add(1, Ordering::SeqCst);
        }
        self.panes.truncate(n);
        while self.panes.len() < n {
            let pi = self.panes.len();
            let mut pane = Pane::new();
            static NEXT_GEN: AtomicU64 = AtomicU64::new(1 << 32);
            pane.gen = Arc::new(AtomicU64::new(NEXT_GEN.fetch_add(1 << 32, Ordering::SeqCst)));
            self.panes.push(pane);
            self.navigate(pi, dir.clone(), false);
        }
        self.sync_watch_dirs();
        self.open_tree_to(&self.pane().cur_dir.clone());
    }

    fn new(cc: &eframe::CreationContext<'_>, start: Option<PathBuf>) -> Self {
        std::thread::spawn(archive::cleanup_extracted);
        let hwnd = match cc.window_handle().expect("main window handle").as_raw() {
            RawWindowHandle::Win32(handle) => {
                windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut std::ffi::c_void)
            }
            _ => panic!("expected a Win32 window handle"),
        };
        setup_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        icons::apply_menu_frame(&cc.egui_ctx);
        let (tx, rx) = mpsc::channel();

        let drives = ('A'..='Z')
            .map(|c| PathBuf::from(format!("{c}:\\")))
            .filter(|p| p.exists())
            .collect();

        let saved: Settings = cc.storage.and_then(|st| eframe::get_value(st, eframe::APP_KEY)).unwrap_or_default();
        let limit = saved.cache_max_mb.clamp(CACHE_MIN_MB, CACHE_MAX_MB) * 1024 * 1024;
        std::thread::spawn(move || cache::trim(limit));
        let mut app = App {
            meta_cache: HashMap::new(),
            meta_request: 0,
            privacy_saving: HashSet::new(),
            transfers_running: 0,
            archive_catalog: Arc::default(),
            hwnd,
            panes: vec![Pane::new()],
            split: Split::One,
            active: 0,
            ctx: cc.egui_ctx.clone(),
            watch: watch::DirWatch::new(tx.clone(), cc.egui_ctx.clone()),
            dir_changes: HashMap::new(),
            tx,
            rx,
            thumb_size: saved.thumb_size.clamp(64.0, 256.0),
            sort: saved.sort,
            sort_desc: saved.sort_desc,
            big: HashMap::new(),
            big_order: VecDeque::new(),
            anims: HashMap::new(),
            rot: HashMap::new(),
            viewer: None,
            fullscreen: false,
            viewer_last_motion: None,
            viewer_menu_pending: None,
            dialog: None,
            status: String::new(),
            drives,
            tree_children: HashMap::new(),
            tree_scroll_pending: false,
            favorites: saved.favorites.clone(),
            actions: Vec::new(),
            cut_paths: HashSet::new(),
            clipboard_request: 0,
            ctrl_v_prev: false,
            rot_saving: HashMap::new(),
            baked: HashMap::new(),
            icons: Arc::new(icons::Icons::new(&cc.egui_ctx)),
            transfer_dirs: saved.transfer_dirs.iter().filter(|p| p.is_dir()).take(TRANSFER_MAX).cloned().collect(),
            dnd_target: None,
            dnd_folder_hover: false,
            cache_max_mb: saved.cache_max_mb.clamp(CACHE_MIN_MB, CACHE_MAX_MB),
            confirm_delete: saved.confirm_delete,
            video_preview: saved.video_preview,
            show_settings: false,
            show_about: false,
            cache_usage: Arc::new(AtomicU64::new(u64::MAX)),
            player: None,
            clips: HashMap::new(),
            clip_order: VecDeque::new(),
            kind_filter: KindFilter::All,
            recent: saved.recent.into_iter().filter(|p| p.is_dir()).take(RECENT_MAX).collect(),
        };

        let home = std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("C:\\"));
        let pics = home.join("Pictures");
        let fallback = if pics.is_dir() { pics } else { restore_dir(&home, Path::new("C:/")) };
        let n = saved.split.len();
        let active = if saved.active_pane < n { saved.active_pane } else { 0 };
        let active_saved = saved.pane_dirs.get(active).or(saved.last_dir.as_ref());
        let mut active_dir = active_saved.map(|d| restore_dir(d, &fallback)).unwrap_or(fallback);
        let start = start.map(normalize);
        if let Some(p) = start.as_ref() {
            if p.is_dir() || (archive::is_archive(p) && p.is_file()) { active_dir = p.clone(); }
            else if p.is_file() {
                if let Some(parent) = p.parent() { active_dir = parent.to_path_buf(); }
            }
        }
        app.navigate(0, active_dir.clone(), false);
        app.set_split(saved.split);
        for pi in 0..n {
            let dir = if pi == active { active_dir.clone() }
                else { saved.pane_dirs.get(pi).or(if pi == 0 { saved.last_dir.as_ref() } else { None })
                    .map(|d| restore_dir(d, &active_dir)).unwrap_or_else(|| active_dir.clone()) };
            if app.panes[pi].cur_dir != normalize(dir.clone()) { app.navigate(pi, dir, false); }
        }
        app.activate(active);
        app.open_tree_to(&app.pane().cur_dir.clone());
        if let Some(p) = start.filter(|p| p.is_file() && !archive::is_archive(p)) {
            if let Some(i) = app.pane().index_of(&p) {
                app.select_single(app.active, i);
                app.viewer = Some(Viewer { pane: app.active, idx: i, zoom: None, pan: Vec2::ZERO });
            }
        }
        app
    }

    // ------------------------------------------------------------ navigation

    fn navigate(&mut self, pi: usize, dir: PathBuf, push_history: bool) {
        if pi == self.active { self.player = None; }
        let dir = normalize(dir);
        let in_archive = archive::is_archive(&dir) && dir.is_file();
        let entries = match if in_archive { Ok(Vec::new()) } else { self.list_filtered(pi, &dir) } {
            Ok(e) => e,
            Err(e) => {
                self.status = format!("폴더를 열 수 없습니다: {e}");
                return;
            }
        };
        if push_history && !self.panes[pi].cur_dir.as_os_str().is_empty() && self.panes[pi].cur_dir != dir {
            let pane = &mut self.panes[pi];
            pane.back.push(pane.cur_dir.clone());
            self.panes[pi].fwd.clear();
        }
        self.status.clear();
        self.recent.retain(|p| p != &dir);
        self.recent.insert(0, dir.clone());
        self.recent.truncate(RECENT_MAX);
        self.panes[pi].cur_dir = dir.clone();
        self.panes[pi].archive_images = None;
        self.panes[pi].archive_stamp = None;
        self.panes[pi].archive_loading = false;
        self.panes[pi].archive_request = 0;
        self.panes[pi].addr_text = dir.display().to_string();
        self.panes[pi].entries = entries;
        self.panes[pi].thumbs = (0..self.panes[pi].entries.len()).map(|_| Thumb::None).collect();
        self.panes[pi].thumbs_loaded = 0;
        self.panes[pi].gen.fetch_add(1, Ordering::SeqCst);
        self.sort_entries(pi);
        self.panes[pi].selected.clear();
        self.panes[pi].anchor = None;
        self.panes[pi].focus = None;
        self.panes[pi].focus_dims = None;
        self.panes[pi].grid_scroll_to = Some(0.0);
        if pi == self.active { self.open_tree_to(&dir); }
        self.sync_watch_dirs();
        if in_archive { self.start_archive_list(pi); }
    }

    fn start_archive_list(&mut self, pi: usize) {
        let path = self.panes[pi].cur_dir.clone();
        self.big.retain(|p, _| !p.starts_with(&path));
        self.big_order.retain(|p| !p.starts_with(&path));
        self.anims.retain(|p, _| !p.starts_with(&path));
        self.panes[pi].thumbs = (0..self.panes[pi].entries.len()).map(|_| Thumb::None).collect();
        self.panes[pi].thumbs_loaded = 0;
        self.panes[pi].gen.fetch_add(1, Ordering::SeqCst);
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let pane = &mut self.panes[pi];
        pane.archive_request = NEXT.fetch_add(1, Ordering::Relaxed);
        pane.archive_loading = true;
        pane.archive_stamp = archive_stamp(&pane.cur_dir);
        if pane.entries.is_empty() { pane.kind_counts = [0; 4]; }
        let (path, stamp, request) = (pane.cur_dir.clone(), pane.archive_stamp, pane.archive_request);
        let (tx, ctx, catalog) = (self.tx.clone(), self.ctx.clone(), self.archive_catalog.clone());
        rayon::spawn(move || {
            let res = cached_archive(&catalog, &path);
            let _ = tx.send(Msg::ArchiveListed { pane: pi, request, path, stamp, res });
            ctx.request_repaint();
        });
    }

    /// List a folder, count each kind, and keep only the kind picked in the filter.
    fn list_filtered(&mut self, pi: usize, dir: &Path) -> std::io::Result<Vec<Entry>> {
        let mut v = if archive::is_archive(dir) && dir.is_file() {
            let modified = self.panes[pi].archive_stamp.map(|s| s.1).unwrap_or(SystemTime::UNIX_EPOCH);
            self.panes[pi].archive_images.as_deref().unwrap_or_default().iter().map(|e| Entry {
                path: dir.join(&e.name), name: e.name.clone(), is_dir: false, media: None, size: e.size, modified,
            }).collect()
        } else { list_dir(dir)? };
        let files = v.iter().filter(|e| !e.is_dir);
        self.panes[pi].kind_counts = [0; 4];
        for e in files {
            self.panes[pi].kind_counts[0] += 1;
            for (k, f) in KindFilter::ALL.iter().enumerate().skip(1) {
                if f.keeps(e) {
                    self.panes[pi].kind_counts[k] += 1;
                }
            }
        }
        let f = self.kind_filter;
        v.retain(|e| f.keeps(e));
        Ok(v)
    }

    fn set_kind_filter(&mut self, f: KindFilter) {
        if self.kind_filter != f {
            self.kind_filter = f;
            self.player = None;
            for pi in 0..self.panes.len() {
                self.reload(pi, None);
                self.panes[pi].grid_scroll_to = Some(0.0);
            }
        }
    }

    fn sync_watch_dirs(&mut self) {
        let dirs: Vec<_> = self.panes.iter().map(|p| watch_dir(&p.cur_dir)).collect();
        self.dir_changes.retain(|d, _| dirs.contains(d));
        if let Some(watch) = &mut self.watch { watch.set_dirs(&dirs); }
    }

    fn refresh_changed_dirs(&mut self) {
        if self.dir_changes.is_empty() { return; }
        if self.dialog.is_some() || egui::DragAndDrop::has_any_payload(&self.ctx) {
            self.ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let now = Instant::now();
        let ready: Vec<_> = self.dir_changes.iter()
            .filter(|(_, (first, last))| watch::due(*first, *last, now))
            .map(|(dir, _)| dir.clone()).collect();
        for dir in ready {
            self.dir_changes.remove(&dir);
            self.refresh_dir(&dir);
        }
        if let Some(wait) = self.dir_changes.values().map(|&(first, last)| watch::wait_for(first, last, now)).min() {
            self.ctx.request_repaint_after(wait);
        }
    }

    fn restore_missing_dirs(&mut self) {
        for pi in 0..self.panes.len() {
            let cur = self.panes[pi].cur_dir.clone();
            if !cur.as_os_str().is_empty() && !cur.is_dir() && !(archive::is_archive(&cur) && cur.is_file()) {
                if self.viewer.as_ref().is_some_and(|v| v.pane == pi) { self.viewer = None; }
                let fallback = cur.ancestors().last().map(Path::to_path_buf).unwrap_or_default();
                self.navigate(pi, restore_dir(&cur, &fallback), false);
            }
        }
    }

    /// Re-read the current folder, keeping unchanged textures and paths.
    fn reload(&mut self, pi: usize, focus_path: Option<PathBuf>) {
        let pane = &self.panes[pi];
        if archive::is_archive(&pane.cur_dir) && pane.cur_dir.is_file()
            && (pane.archive_images.is_none() || pane.archive_stamp != archive_stamp(&pane.cur_dir)) {
            if !pane.archive_loading || pane.archive_stamp != archive_stamp(&pane.cur_dir) { self.start_archive_list(pi); }
            return;
        }
        let Ok(entries) = self.list_filtered(pi, &self.panes[pi].cur_dir.clone()) else { return };
        let pane = &mut self.panes[pi];
        let selected: Vec<_> = pane.selected.iter().filter_map(|&i| pane.entries.get(i)).map(|e| e.path.clone()).collect();
        let anchor = pane.anchor.and_then(|i| pane.entries.get(i)).map(|e| e.path.clone());
        let old_focus = pane.focus;
        let old_focus_path = old_focus.and_then(|i| pane.entries.get(i)).map(|e| e.path.clone());
        let viewed = self.viewer.as_ref().filter(|v| v.pane == pi)
            .and_then(|v| pane.entries.get(v.idx)).map(|e| e.path.clone());
        let mut old = HashMap::new();
        for (e, t) in pane.entries.iter().zip(pane.thumbs.drain(..)) {
            old.insert(e.path.clone(), (e.size, e.modified, t));
        }
        pane.thumbs = entries.iter().map(|e| {
            match old.remove(&e.path) {
                Some((size, modified, t)) if thumb_still_valid(size, modified, e.size, e.modified) => {
                    if matches!(t, Thumb::Ready(_)) { t } else { Thumb::None }
                }
                Some(_) => {
                    self.big.remove(&e.path);
                    self.big_order.retain(|p| p != &e.path);
                    self.anims.remove(&e.path);
                    self.clips.remove(&e.path);
                    self.clip_order.retain(|p| p != &e.path);
                    Thumb::None
                }
                None => Thumb::None,
            }
        }).collect();
        for path in old.keys() {
            self.big.remove(path);
            self.big_order.retain(|p| p != path);
            self.anims.remove(path);
            self.clips.remove(path);
            self.clip_order.retain(|p| p != path);
        }
        pane.entries = entries;
        pane.focus = None;
        pane.focus_dims = None;
        pane.clip_hover = None;
        pane.pending_open = None;
        pane.thumbs_loaded = pane.thumbs.iter().filter(|t| matches!(t, Thumb::Ready(_))).count();
        pane.gen.fetch_add(1, Ordering::SeqCst);
        self.sort_entries(pi);
        let pane = &mut self.panes[pi];
        pane.selected = selected.iter().filter_map(|p| pane.index_of(p)).collect();
        pane.anchor = anchor.and_then(|p| pane.index_of(&p));
        let n = pane.entries.len();
        let explicit = focus_path.is_some();
        let target = focus_path.and_then(|p| pane.index_of(&p))
            .or_else(|| old_focus_path.and_then(|p| pane.index_of(&p)))
            .or_else(|| old_focus.filter(|_| n > 0).map(|f| f.min(n - 1)));
        if let Some(i) = target {
            if explicit { self.select_single(pi, i); }
            else { self.set_focus(pi, i); }
        }
        if self.viewer.as_ref().is_some_and(|v| v.pane == pi) {
            match viewed.and_then(|p| self.panes[pi].index_of(&p)) {
                Some(i) => self.viewer.as_mut().unwrap().idx = i,
                None => self.resume_viewer(pi, self.viewer.as_ref().unwrap().idx),
            }
        }
        self.tree_children.remove(&self.panes[pi].cur_dir);
    }

    fn go_back(&mut self) {
        if let Some(p) = self.panes[self.active].back.pop() {
            let pane = self.pane_mut();
            pane.fwd.push(pane.cur_dir.clone());
            self.navigate(self.active, p, false);
        }
    }

    fn go_fwd(&mut self) {
        if let Some(p) = self.panes[self.active].fwd.pop() {
            let pane = self.pane_mut();
            pane.back.push(pane.cur_dir.clone());
            self.navigate(self.active, p, false);
        }
    }

    fn go_up(&mut self) {
        let cur = self.panes[self.active].cur_dir.clone();
        if let Some(parent) = cur.parent() {
            self.navigate(self.active, parent.to_path_buf(), true);
            if let Some(i) = self.pane().index_of(&cur) {
                self.select_single(self.active, i);
            }
        }
    }

    fn open_address(&mut self) {
        let p = PathBuf::from(self.panes[self.active].addr_text.trim().trim_matches('"'));
        if p.is_file() && !archive::is_archive(&p) {
            let p = normalize(p);
            if let Some(parent) = p.parent() {
                self.navigate(self.active, parent.to_path_buf(), true);
            }
            if let Some(i) = self.pane().index_of(&p) {
                self.select_single(self.active, i);
            }
        } else {
            self.navigate(self.active, p, true);
        }
    }

    /// Space on a sound file: start, pause or resume the preview.
    fn audio_toggle(&mut self) {
        let Some(e) = self.panes[self.active].focus.and_then(|i| self.panes[self.active].entries.get(i)) else { return };
        if e.media != Some(loader::Media::Audio) {
            return;
        }
        let path = e.path.clone();
        let r = match &mut self.player {
            Some(p) if p.path == path => p.toggle(),
            _ => audio::Player::start(&path).map(|p| self.player = Some(p)),
        };
        if let Err(e) = r {
            self.player = None;
            self.status = e;
        }
    }

    /// Play button, time and seek bar under the audio preview.
    fn audio_controls(&mut self, ui: &mut Ui, path: &Path) {
        let mine = self.player.as_ref().filter(|p| p.path == path);
        let playing = mine.is_some_and(|p| p.is_playing());
        let pos = mine.map(|p| p.pos()).unwrap_or_default();
        let dur = mine.and_then(|p| p.duration);
        let have = mine.is_some();
        let mut toggle = false;
        let mut seek_to: Option<Duration> = None;
        ui.horizontal(|ui| {
            let label = if playing { "⏸ 일시정지" } else { "▶ 재생" };
            let btn = egui::Button::new(egui::RichText::new(label).size(15.0))
                .min_size(vec2(96.0, 28.0))
                .fill(if playing { Color32::from_rgb(204, 228, 255) } else { Color32::from_gray(235) });
            if ui.add(btn).on_hover_text("Space").clicked() {
                toggle = true;
            }
            let t = match dur {
                Some(d) => format!("{} / {}", audio::fmt_dur(pos), audio::fmt_dur(d)),
                None => audio::fmt_dur(pos),
            };
            ui.monospace(t);
        });
        // seek bar: click or drag anywhere on it
        let (bar, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::click_and_drag());
        let track = Rect::from_center_size(bar.center(), vec2(bar.width(), 4.0));
        let painter = ui.painter();
        painter.rect_filled(track, 2.0f32, Color32::from_gray(210));
        if let Some(d) = dur.filter(|d| !d.is_zero()) {
            let frac = (pos.as_secs_f32() / d.as_secs_f32()).clamp(0.0, 1.0);
            let x = track.left() + track.width() * frac;
            let done = Rect::from_min_max(track.min, pos2(x, track.max.y));
            let col = Color32::from_rgb(40, 120, 220);
            painter.rect_filled(done, 2.0f32, col);
            let r = if resp.hovered() || resp.dragged() { 6.0 } else { 4.5 };
            painter.circle_filled(pos2(x, track.center().y), r, col);
            if have && (resp.clicked() || resp.dragged()) {
                if let Some(p) = resp.interact_pointer_pos() {
                    let f = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
                    seek_to = Some(d.mul_f32(f));
                }
            }
        }
        if let (Some(to), Some(p)) = (seek_to, self.player.as_mut()) {
            p.seek(to);
        }
        if toggle {
            self.audio_toggle();
        }
        if playing {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        ui.weak("Space: 재생 / 일시정지 · 더블 클릭하면 기본 프로그램으로 엽니다");
    }

    fn open_entry(&mut self, pi: usize, i: usize) {
        let Some(e) = self.panes[pi].entries.get(i) else { return };
        if e.is_dir || archive::is_archive(&e.path) {
            let p = e.path.clone();
            self.navigate(pi, p, true);
        } else if e.media.is_some() {
            fileops::open_default(&e.path);
        } else {
            self.viewer = Some(Viewer { pane: pi, idx: i, zoom: None, pan: Vec2::ZERO });
            self.viewer_last_motion = None;
        }
    }

    fn open_selected_fullscreen(&mut self) {
        let Some(i) = self.pane().focus.filter(|&i| {
            self.pane().entries.get(i).is_some_and(|e| e.is_img())
        }) else { return };
        self.open_entry(self.active, i);
        if self.viewer.is_some() && !self.fullscreen {
            self.toggle_fullscreen();
        }
        self.ctx.request_repaint();
    }

    fn sort_entries(&mut self, pi: usize) {
        let focus_path = self.panes[pi].focus.and_then(|i| self.panes[pi].entries.get(i)).map(|e| e.path.clone());
        let thumbs = std::mem::take(&mut self.panes[pi].thumbs);
        let mut v: Vec<(Entry, Thumb)> = self.panes[pi].entries.drain(..).zip(thumbs).collect();
        let (key, desc) = (self.sort, self.sort_desc);
        v.sort_by(|(a, _), (b, _)| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let o = match key {
                    SortKey::Name => natural_cmp(&a.name, &b.name),
                    SortKey::Date => a.modified.cmp(&b.modified),
                    SortKey::Size => a.size.cmp(&b.size),
                }
                .then_with(|| natural_cmp(&a.name, &b.name));
                if desc { o.reverse() } else { o }
            })
        });
        for (e, t) in v {
            // indices change: in-flight jobs become invalid
            self.panes[pi].thumbs.push(if matches!(t, Thumb::Pending) { Thumb::None } else { t });
            self.panes[pi].entries.push(e);
        }
        self.panes[pi].gen.fetch_add(1, Ordering::SeqCst);
        if let Some(p) = focus_path {
            if let Some(i) = self.panes[pi].index_of(&p) {
                self.select_single(pi, i);
            }
        }
    }

    // ------------------------------------------------------------ selection

    fn set_focus(&mut self, pi: usize, i: usize) {
        if pi == self.active && self.player.as_ref().is_some_and(|p| self.panes[pi].entries.get(i).map(|e| &e.path) != Some(&p.path)) {
            self.player = None;
        }
        if self.panes[pi].focus != Some(i) {
            self.panes[pi].focus = Some(i);
            self.panes[pi].focus_time = Instant::now();
            self.panes[pi].focus_dims = self.panes[pi].entries.get(i)
                .and_then(|e| match self.big.get(&e.path) { Some(Big::Ready(_, dims)) => Some(*dims), _ => None })
                .or_else(|| self.panes[pi].entries
                .get(i)
                .filter(|e| e.is_img() && archive::split(&e.path).is_none())
                .and_then(|e| image::image_dimensions(&e.path).ok())
                .map(|(w, h)| [w, h]));
        }
    }

    fn select_single(&mut self, pi: usize, i: usize) {
        self.panes[pi].selected.clear();
        self.panes[pi].selected.insert(i);
        self.panes[pi].anchor = Some(i);
        self.set_focus(pi, i);
        self.ensure_visible(pi, i);
    }

    fn move_focus(&mut self, pi: usize, t: usize, shift: bool) {
        if shift {
            let a = self.panes[pi].anchor.unwrap_or(t);
            self.panes[pi].selected = (a.min(t)..=a.max(t)).collect();
            self.set_focus(pi, t);
            self.ensure_visible(pi, t);
        } else {
            self.select_single(pi, t);
        }
    }

    fn click_entry(&mut self, pi: usize, i: usize, mods: egui::Modifiers) {
        if mods.command {
            if !self.panes[pi].selected.insert(i) {
                self.panes[pi].selected.remove(&i);
            }
            self.panes[pi].anchor = Some(i);
            self.set_focus(pi, i);
        } else if mods.shift {
            self.move_focus(pi, i, true);
        } else {
            self.select_single(pi, i);
        }
    }

    fn cell_size(&self) -> Vec2 {
        vec2(self.thumb_size + 16.0, self.thumb_size + 16.0 + 22.0)
    }

    fn ensure_visible(&mut self, pi: usize, i: usize) {
        let lay = self.panes[pi].layout(self.panes[pi].grid_w, self.thumb_size);
        let Some(r) = lay.row_of(i + self.panes[pi].slot_off()).map(|r| lay.rows[r]) else { return };
        let (top, cell) = (r.top, r.cell);
        if top < self.panes[pi].grid_offset {
            self.panes[pi].grid_scroll_to = Some(top);
        } else if top + cell.y > self.panes[pi].grid_offset + self.panes[pi].grid_view_h {
            self.panes[pi].grid_scroll_to = Some(top + cell.y - self.panes[pi].grid_view_h);
        }
    }

    fn rot_of(&self, p: &Path) -> i32 {
        self.rot.get(p).copied().unwrap_or(0)
    }

    /// Frame of the animated GIF `p` to show now, scheduling the next repaint.
    fn anim_frame(&self, p: &Path) -> Option<TextureHandle> {
        self.anim_pick(self.anims.get(p)?).map(|f| f.0)
    }

    /// Frame of `a` to show now and its number, scheduling the next repaint.
    fn anim_pick(&self, a: &Anim) -> Option<(TextureHandle, usize)> {
        if a.total.is_zero() {
            return None;
        }
        let mut t = Duration::from_nanos((a.start.elapsed().as_nanos() % a.total.as_nanos()) as u64);
        for (k, (tex, d)) in a.frames.iter().enumerate() {
            if t < *d {
                self.ctx.request_repaint_after(*d - t);
                return Some((tex.clone(), k));
            }
            t -= *d;
        }
        a.frames.last().map(|f| (f.0.clone(), a.frames.len() - 1))
    }

    /// Current frame of the video preview of `p`, if it is ready.
    fn clip_frame(&self, p: &Path) -> Option<(TextureHandle, usize)> {
        if !self.video_preview {
            return None;
        }
        match self.clips.get(p)? {
            Clip::Ready(a) => self.anim_pick(a),
            _ => None,
        }
    }

    /// Start making the video preview of entry `i` (from the disk cache if it is there).
    fn request_clip(&mut self, pi: usize, i: usize) {
        if !self.video_preview {
            return;
        }
        let Some(e) = self.panes[pi].entries.get(i).filter(|e| e.media == Some(loader::Media::Video)) else { return };
        let path = e.path.clone();
        if let Some(pos) = self.clip_order.iter().position(|p| p == &path) {
            let p = self.clip_order.remove(pos).unwrap();
            self.clip_order.push_back(p);
            return;
        }
        self.clips.insert(path.clone(), Clip::Pending);
        self.clip_order.push_back(path.clone());
        while self.clip_order.len() > CLIP_CACHE {
            if let Some(old) = self.clip_order.pop_front() {
                self.clips.remove(&old);
            }
        }
        let (size, modified) = (e.size, e.modified);
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            // cached as one strip of frames side by side, under its own key
            let key = PathBuf::from(format!("{}#clip{CLIP_FRAMES}", path.display()));
            let frames: Option<Vec<image::RgbaImage>> = match cache::load(&key, size, modified) {
                Some(strip) if strip.width() >= CLIP_W => {
                    let n = (strip.width() / CLIP_W) as usize;
                    Some((0..n).map(|k| image::imageops::crop_imm(&strip, k as u32 * CLIP_W, 0, CLIP_W, strip.height()).to_image()).collect())
                }
                _ => fileops::video_frames(&path, CLIP_FRAMES, CLIP_W).ok().inspect(|v| {
                    let h = v[0].height();
                    let mut strip = image::RgbaImage::new(CLIP_W * v.len() as u32, h);
                    for (k, f) in v.iter().enumerate() {
                        image::imageops::replace(&mut strip, f, k as i64 * CLIP_W as i64, 0);
                    }
                    cache::store(&key, size, modified, &strip);
                }),
            };
            let frames = frames.map(|v| {
                v.into_iter()
                    .enumerate()
                    .map(|(k, img)| {
                        let ci = ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                        ctx.load_texture(format!("{}#clip{k}", path.display()), ci, tex_opts())
                    })
                    .collect()
            });
            let _ = tx.send(Msg::Clip { path, stamp: Some((size, modified)), frames });
            ctx.request_repaint();
        });
    }

    /// Rotation to draw `tex` of file `p` with: turns not yet in the file,
    /// plus turns already in the file but not in this texture.
    fn disp_rot(&self, p: &Path, tex: &TextureHandle) -> i32 {
        self.rot_of(p) + self.baked.get(&tex.id()).copied().unwrap_or(0)
    }

    fn rotate_current(&mut self, d: i32) {
        let targets = match &self.viewer {
            Some(v) => self.panes[v.pane].entries.get(v.idx).map(|e| vec![e.path.clone()]).unwrap_or_default(),
            None => self.pane().selected_paths(),
        };
        self.rotate_paths(targets, d);
    }

    /// Show the turn at once, then write it to the file in the background.
    fn rotate_paths(&mut self, paths: Vec<PathBuf>, d: i32) {
        if self.reject_archive(&paths) { return; }
        for p in paths {
            if self.privacy_saving.contains(&p) {
                self.status = "개인 정보를 지우는 중입니다".into();
                continue;
            }
            if !loader::is_image(&p) {
                continue;
            }
            let r = self.rot.entry(p.clone()).or_insert(0);
            *r = (*r + d).rem_euclid(4);
            self.save_rotation(&p);
        }
    }

    fn save_rotation(&mut self, p: &Path) {
        if model::is_model(p) {
            self.status = "Blender, VRM 파일은 파일 회전을 지원하지 않습니다 (보기만 돌립니다)".into();
            return;
        }
        if self.reject_archive(&[p.to_path_buf()]) { return; }
        let turns = self.rot_of(p);
        if turns == 0 || self.rot_saving.contains_key(p) {
            return; // nothing to do, or a save is running and will pick up the rest
        }
        self.rot_saving.insert(p.to_path_buf(), turns);
        let (path, tx, ctx) = (p.to_path_buf(), self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let res = rotate::save_rotation(&path, turns);
            let _ = tx.send(Msg::Rotated { path, turns, res });
            ctx.request_repaint();
        });
    }

    fn on_rotated(&mut self, path: PathBuf, turns: i32, res: Result<(), String>) {
        self.rot_saving.remove(&path);
        if let Err(e) = res {
            self.status = format!("회전 저장 실패: {e}");
            return;
        }
        self.meta_cache.retain(|(p, _), _| p != &path);
        // the file now holds `turns`; textures already loaded do not
        let left = (self.rot_of(&path) - turns).rem_euclid(4);
        if left == 0 {
            self.rot.remove(&path);
        } else {
            self.rot.insert(path.clone(), left);
        }
        let mut texs: Vec<egui::TextureId> = Vec::new();
        if let Some(Big::Ready(t, _)) = self.big.get(&path) {
            texs.push(t.id());
        }
        if matches!(self.big.get(&path), Some(Big::Pending)) {
            self.big.remove(&path); // still decoding the old file; load again
        }
        let metadata = std::fs::metadata(&path).ok();
        for pane in &mut self.panes {
            if let Some(i) = pane.index_of(&path) {
                if let Thumb::Ready(t) = &pane.thumbs[i] { texs.push(t.id()); }
                if matches!(pane.thumbs[i], Thumb::Pending) { pane.thumbs[i] = Thumb::None; }
                if let Some(md) = &metadata {
                    pane.entries[i].size = md.len();
                    pane.entries[i].modified = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                }
            }
        }
        for id in texs {
            *self.baked.entry(id).or_insert(0) += turns;
        }
        self.status = "회전을 파일에 저장했습니다".into();
        self.save_rotation(&path); // more turns pressed while saving
    }

    // ------------------------------------------------------------ loading

    fn request_thumb(&mut self, pi: usize, i: usize) {
        let Some(e) = self.panes[pi].entries.get(i) else { return };
        if e.is_dir || !matches!(self.panes[pi].thumbs[i], Thumb::None) {
            return;
        }
        // video, audio, HEIC and RAW: the shell's thumbnail (RAW has a fast embedded preview)
        let shell = e.media.is_some() || (loader::is_wic(&e.path) && archive::split(&e.path).is_none());
        let (path, size, modified, media) = (e.path.clone(), e.size, e.modified, shell);
        self.panes[pi].thumbs[i] = Thumb::Pending;
        let gen = self.panes[pi].gen.load(Ordering::SeqCst);
        let (gen_now, vs, ve) = (self.panes[pi].gen.clone(), self.panes[pi].vis_start.clone(), self.panes[pi].vis_end.clone());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let catalog = self.archive_catalog.clone();
        rayon::spawn(move || {
            if gen_now.load(Ordering::SeqCst) != gen {
                return;
            }
            // user scrolled away before we got here: give up, it will be asked again
            let (s, e) = (vs.load(Ordering::Relaxed), ve.load(Ordering::Relaxed));
            if i + 120 < s || i > e + 120 {
                let _ = tx.send(Msg::Skip { pane: pi, gen, idx: i });
                ctx.request_repaint();
                return;
            }
            let cache_stamp = archive::split(&path).and_then(|(a, _)| archive_stamp(&a)).unwrap_or((size, modified));
            let made = match cache::load(&path, cache_stamp.0, cache_stamp.1) {
                Some(img) => Ok(img),
                None => {
                    let made = if media {
                        fileops::shell_thumbnail(&path, loader::THUMB_MAX as i32)
                    } else if archive::is_archive(&path) {
                        cached_archive(&catalog, &path).and_then(|entries| {
                            entries.first().ok_or_else(|| "그림 없음".to_string())
                                .and_then(|e| loader::make_thumb(&path.join(&e.name)))
                        })
                    } else {
                        loader::make_thumb(&path)
                    };
                    made.inspect(|img| cache::store(&path, cache_stamp.0, cache_stamp.1, img))
                }
            };
            let msg = match made {
                Ok(img) => {
                    let ci = ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                    let tex = ctx.load_texture(format!("t{gen}/{i}"), ci, tex_opts());
                    Msg::Thumb { pane: pi, gen, idx: i, mtime: modified, tex }
                }
                Err(_) => Msg::ThumbFail { pane: pi, gen, idx: i },
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn file_stamp(&self, path: &Path) -> Option<(u64, SystemTime)> {
        if let Some((a, _)) = archive::split(path) { return archive_stamp(&a); }
        self.panes.iter().find_map(|p| p.index_of(path).map(|i| (p.entries[i].size, p.entries[i].modified)))
    }

    fn request_big(&mut self, path: &Path) {
        if self.big.contains_key(path) {
            if let Some(pos) = self.big_order.iter().position(|p| p == path) {
                let p = self.big_order.remove(pos).unwrap();
                self.big_order.push_back(p);
            }
            return;
        }
        self.big.insert(path.to_path_buf(), Big::Pending);
        self.big_order.push_back(path.to_path_buf());
        while self.big_order.len() > BIG_CACHE {
            if let Some(old) = self.big_order.pop_front() {
                self.big.remove(&old);
                self.anims.remove(&old);
            }
        }
        let stamp = self.file_stamp(path);
        let path = path.to_path_buf();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let max_side = ctx.input(|i| i.max_texture_side).min(16384) as u32;
        std::thread::spawn(move || {
            let decoded = if loader::is_wic(&path) && archive::split(&path).is_none() {
                fileops::wic_decode(&path, max_side).map(|(img, dims, o)| loader::orient(img, dims, o))
            } else {
                loader::load_full(&path, max_side)
            };
            let res = decoded.map(|(img, dims)| {
                let ci = ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                (ctx.load_texture(path.display().to_string(), ci, tex_opts()), dims)
            });
            let ok = res.is_ok();
            let _ = tx.send(Msg::Big { path: path.clone(), stamp, res });
            ctx.request_repaint();
            // animated GIF: the first frame is up already, the rest follow
            let is_gif = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gif"));
            if ok && is_gif {
                if let Some(frames) = loader::load_gif_frames(&path, max_side) {
                    let frames = frames
                        .into_iter()
                        .enumerate()
                        .map(|(k, (img, delay))| {
                            let ci = ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                            (ctx.load_texture(format!("{}#{k}", path.display()), ci, tex_opts()), delay)
                        })
                        .collect();
                    let _ = tx.send(Msg::Anim { path, stamp, frames });
                    ctx.request_repaint();
                }
            }
        });
    }

    fn poll_msgs(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Exif { key, request, summary } => {
                    if matches!(self.meta_cache.get(&key), Some(MetaState::Pending(n)) if *n == request) {
                        self.meta_cache.insert(key, MetaState::Ready(summary));
                    }
                }
                Msg::Stripped { paths, status } => {
                    let mut dirs = HashSet::new();
                    for path in paths {
                        self.privacy_saving.remove(&path);
                        self.meta_cache.retain(|(p, _), _| p != &path);
                        self.big.remove(&path);
                        self.big_order.retain(|p| p != &path);
                        self.anims.remove(&path);
                        for pane in &mut self.panes {
                            if let Some(i) = pane.index_of(&path) {
                                if matches!(pane.thumbs[i], Thumb::Ready(_)) { pane.thumbs_loaded = pane.thumbs_loaded.saturating_sub(1); }
                                pane.thumbs[i] = Thumb::None;
                            }
                        }
                        if let Some(parent) = path.parent() { dirs.insert(parent.to_path_buf()); }
                    }
                    for dir in dirs { self.refresh_dir(&dir); }
                    if let Some(v) = &self.viewer {
                        if let Some(e) = self.panes[v.pane].entries.get(v.idx) {
                            let path = e.path.clone();
                            self.request_big(&path);
                        }
                    }
                    self.status = status;
                }
                Msg::ArchiveListed { pane, request, path, stamp, res } => {
                    if !self.panes.get(pane).is_some_and(|p| p.cur_dir == path && p.archive_request == request) { continue; }
                    if stamp != archive_stamp(&path) {
                        self.restore_missing_dirs();
                        if self.panes[pane].cur_dir == path { self.start_archive_list(pane); }
                        continue;
                    }
                    self.panes[pane].archive_loading = false;
                    self.panes[pane].archive_images = Some(res.as_ref().cloned().unwrap_or_default());
                    self.reload(pane, None);
                    if let Err(e) = res { self.status = e; }
                }
                Msg::DirChanged { dir } => {
                    if self.panes.iter().any(|p| watch_dir(&p.cur_dir) == dir) {
                        let now = Instant::now();
                        self.dir_changes.entry(dir).and_modify(|times| times.1 = now).or_insert((now, now));
                    }
                }
                Msg::Thumb { pane, gen: g, idx, mtime, tex } if route_ok(&self.panes, pane, g, idx) => {
                    if self.panes[pane].entries[idx].modified == mtime {
                        self.panes[pane].thumbs[idx] = Thumb::Ready(tex);
                        self.panes[pane].thumbs_loaded += 1;
                    } else {
                        self.panes[pane].thumbs[idx] = Thumb::None; // file changed while decoding
                    }
                }
                Msg::ThumbFail { pane, gen: g, idx } if route_ok(&self.panes, pane, g, idx) => self.panes[pane].thumbs[idx] = Thumb::Failed,
                Msg::Skip { pane, gen: g, idx } if route_ok(&self.panes, pane, g, idx) => {
                    if matches!(self.panes[pane].thumbs[idx], Thumb::Pending) {
                        self.panes[pane].thumbs[idx] = Thumb::None;
                    }
                }
                Msg::Rotated { path, turns, res } => self.on_rotated(path, turns, res),
                Msg::Clip { path, stamp, frames } => {
                    if stamp != self.file_stamp(&path) { continue; }
                    if self.clips.contains_key(&path) {
                        let clip = match frames {
                            Some(f) if !f.is_empty() => {
                                let frames: Vec<_> = f.into_iter().map(|t| (t, CLIP_FRAME_TIME)).collect();
                                let total = CLIP_FRAME_TIME * frames.len() as u32;
                                Clip::Ready(Anim { frames, total, start: Instant::now() })
                            }
                            _ => Clip::Failed,
                        };
                        self.clips.insert(path, clip);
                    }
                }
                Msg::Anim { path, stamp, frames } => {
                    if stamp != self.file_stamp(&path) { continue; }
                    if self.big.contains_key(&path) {
                        let total = frames.iter().map(|f| f.1).sum();
                        self.anims.insert(path, Anim { frames, total, start: Instant::now() });
                    }
                }
                Msg::ArchiveCopied { request, sequence, res } => {
                    if request != self.clipboard_request || sequence != fileops::clipboard_sequence() { continue; }
                    match res.and_then(|paths| fileops::clipboard_set_files(&paths, false).map(|_| paths.len())) {
                        Ok(n) => { self.cut_paths.clear(); self.status = format!("{n}개 항목을 복사했습니다"); }
                        Err(e) => self.status = format!("복사 실패: {e}"),
                    }
                }
                Msg::OpDone { status, dirs, clear_clipboard } => {
                    self.transfers_running = self.transfers_running.saturating_sub(1);
                    self.status = status;
                    if clear_clipboard {
                        fileops::clipboard_clear();
                        self.cut_paths.clear();
                    }
                    for d in dirs {
                        self.refresh_dir(&d);
                    }
                    self.restore_missing_dirs();
                }
                Msg::Big { path, stamp, res } => {
                    let now = self.file_stamp(&path);
                    if stamp != now { continue; }
                    if let Ok((_, dims)) = &res {
                        for pane in &mut self.panes {
                            if pane.focus.and_then(|i| pane.entries.get(i)).is_some_and(|e| e.path == path) { pane.focus_dims = Some(*dims); }
                        }
                    }
                    if let Some(b) = self.big.get_mut(&path) {
                        *b = match res {
                            Ok((t, d)) => Big::Ready(t, d),
                            Err(_) => Big::Failed,
                        };
                    }
                }
                _ => {}
            }
        }
    }

    fn evict_thumbs(&mut self) {
        let loaded: usize = self.panes.iter().map(|p| p.thumbs_loaded).sum();
        if loaded <= MAX_THUMBS_LOADED { return; }
        let mut candidates = Vec::new();
        for (pi, p) in self.panes.iter().enumerate() {
            let s = p.vis_start.load(Ordering::Relaxed);
            let e = p.vis_end.load(Ordering::Relaxed);
            for (i, t) in p.thumbs.iter().enumerate() {
                if matches!(t, Thumb::Ready(_)) {
                    let distance = if i < s { s - i } else if i >= e { i - e + 1 } else { 0 };
                    candidates.push((distance, pi, i));
                }
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, pi, i) in candidates.into_iter().take(loaded - MAX_THUMBS_LOADED) {
            self.panes[pi].thumbs[i] = Thumb::None;
            self.panes[pi].thumbs_loaded -= 1;
        }
    }

    // ------------------------------------------------------------ file ops

    fn request_exif(&mut self, path: &Path) -> MetaKey {
        let key = (path.to_path_buf(), self.file_stamp(path));
        if self.meta_cache.contains_key(&key) { return key; }
        if self.meta_cache.len() >= 128 { self.meta_cache.clear(); }
        self.meta_request += 1;
        let request = self.meta_request;
        self.meta_cache.insert(key.clone(), MetaState::Pending(request));
        let (tx, ctx, result_key) = (self.tx.clone(), self.ctx.clone(), key.clone());
        std::thread::spawn(move || {
            let summary = meta::read_summary(&result_key.0).map(Arc::new);
            let _ = tx.send(Msg::Exif { key: result_key, request, summary });
            ctx.request_repaint();
        });
        key
    }

    fn reject_saving(&mut self, paths: &[PathBuf]) -> bool {
        let busy = paths.iter().any(|p| self.privacy_saving.iter().chain(self.rot_saving.keys()).any(|saving| saving.starts_with(p)));
        if busy { self.status = "파일을 저장하는 중입니다. 끝난 뒤 다시 시도하세요".into(); }
        busy
    }

    fn ask_strip_private(&mut self, paths: Vec<PathBuf>) {
        if paths.iter().any(|p| model::is_model(p)) {
            self.status = "Blender, VRM 파일은 개인 정보 지우기를 지원하지 않습니다".into();
        }
        let mut seen = HashSet::new();
        let paths: Vec<_> = paths.into_iter().filter(|p| meta::can_strip(p) && seen.insert(p.clone())).collect();
        if !paths.is_empty() { self.dialog = Some(Dialog::StripPrivate(paths)); }
    }

    fn do_strip_private(&mut self, paths: Vec<PathBuf>) {
        if self.transfers_running > 0 || paths.iter().any(|p| self.rot_saving.contains_key(p) || self.privacy_saving.contains(p)) {
            self.status = "파일을 저장하는 중입니다. 끝난 뒤 다시 시도하세요".into();
            return;
        }
        self.privacy_saving.extend(paths.iter().cloned());
        self.status = "개인 정보를 지우는 중입니다".into();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let (mut done, mut failed, mut lost) = (0, 0, 0);
            let mut first = None;
            for path in &paths {
                let stamp = archive_stamp(path);
                match meta::strip_file(path) {
                    Ok(orientation_lost) => {
                        done += 1;
                        lost += usize::from(orientation_lost);
                        if let Some((size, time)) = stamp { cache::invalidate(path, size, time); }
                    }
                    Err(reason) => {
                        failed += 1;
                        if first.is_none() {
                            first = Some(format!("{} - {reason}", path.file_name().unwrap_or_default().to_string_lossy()));
                        }
                    }
                }
            }
            let mut status = format!("{done}개 지움, {failed}개 실패");
            if let Some(first) = first { status.push_str(&format!(" (첫 실패: {first})")); }
            if lost > 0 { status.push_str(&format!(" · {lost}개 WebP는 VP8X가 없어 방향을 보존하지 못했습니다")); }
            let _ = tx.send(Msg::Stripped { paths, status });
            ctx.request_repaint();
        });
    }

    fn ask_delete(&mut self, paths: Vec<PathBuf>) {
        if self.reject_archive(&paths) { return; }
        // drive roots cannot go to the recycle bin
        let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists() && p.parent().is_some()).collect();
        if paths.is_empty() {
            return;
        }
        if self.confirm_delete {
            self.dialog = Some(Dialog::Delete(paths));
        } else {
            self.do_delete(paths); // still the recycle bin, so it can be undone
        }
    }

    fn do_delete(&mut self, paths: Vec<PathBuf>) {
        if self.reject_saving(&paths) { return; }
        if self.reject_archive(&paths) { return; }
        match trash::delete_all(&paths) {
            Ok(()) => self.status = format!("{}개 항목을 휴지통으로 보냈습니다", paths.len()),
            Err(e) => self.status = format!("삭제 실패: {e}"),
        }
        for p in &paths {
            self.big.remove(p);
            self.cut_paths.remove(p);
            if let Some(parent) = p.parent() {
                self.tree_children.remove(parent);
            }
            self.tree_children.retain(|k, _| !k.starts_with(p));
        }
        let viewer = self.viewer.as_ref().map(|v| (v.pane, v.idx));
        for pi in 0..self.panes.len() {
            let dir = self.panes[pi].cur_dir.clone();
            if paths.iter().any(|p| dir.starts_with(p)) {
                let fallback = self.pane().cur_dir.clone();
                self.navigate(pi, restore_dir(&dir, &restore_dir(&fallback, Path::new("C:/"))), false);
            } else if paths.iter().any(|p| p.parent() == Some(dir.as_path())) {
                self.reload(pi, None);
            }
        }
        if let Some((pi, idx)) = viewer {
            self.resume_viewer(pi, idx);
        }
    }

    fn ask_rename(&mut self) {
        if let Some(p) = self.panes[self.active].focus.and_then(|i| self.panes[self.active].entries.get(i)).map(|e| e.path.clone()) {
            self.ask_rename_path(p);
        }
    }

    fn ask_rename_path(&mut self, path: PathBuf) {
        if self.reject_archive(std::slice::from_ref(&path)) { return; }
        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) {
            self.dialog = Some(Dialog::Rename { path, text: name, first: true });
        }
    }

    fn ask_new_folder(&mut self, parent: PathBuf) {
        if archive::is_location(&parent) || self.reject_archive(std::slice::from_ref(&parent)) {
            self.status = "압축 파일 안에서는 할 수 없습니다".into(); return;
        }
        let mut name = "새 폴더".to_string();
        let mut k = 2;
        while parent.join(&name).exists() {
            name = format!("새 폴더 ({k})");
            k += 1;
        }
        self.dialog = Some(Dialog::NewFolder { parent, text: name, first: true });
    }

    fn do_new_folder(&mut self, parent: PathBuf, name: &str) {
        if archive::is_location(&parent) || self.reject_archive(std::slice::from_ref(&parent)) {
            self.status = "압축 파일 안에서는 할 수 없습니다".into(); return;
        }
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let target = parent.join(name);
        if target.exists() {
            self.status = format!("이미 있는 이름입니다: {name}");
            return;
        }
        match std::fs::create_dir(&target) {
            Ok(()) => {
                self.status = format!("폴더를 만들었습니다: {name}");
                self.tree_children.remove(&parent);
                let mut st = CollapsingState::load_with_default_open(&self.ctx, tree_id(&parent), false);
                st.set_open(true);
                st.store(&self.ctx);
                for pi in panes_showing(&self.panes, &parent) {
                    let focus = (pi == self.active).then(|| target.clone());
                    self.reload(pi, focus);
                }
            }
            Err(e) => self.status = format!("폴더 만들기 실패: {e}"),
        }
    }

    /// Forget cached tree children under `d` and re-read the grid if it shows `d`.
    fn refresh_dir(&mut self, d: &Path) {
        self.restore_missing_dirs();
        self.tree_children.retain(|k, _| !k.starts_with(d));
        let panes: Vec<_> = self.panes.iter().enumerate().filter_map(|(pi, p)|
            (p.cur_dir == d || watch_dir(&p.cur_dir) == d).then_some(pi)).collect();
        for pi in panes {
            self.reload(pi, None);
        }
        self.sync_watch_dirs();
    }

    fn do_rename(&mut self, path: PathBuf, new_name: &str) {
        if self.reject_saving(std::slice::from_ref(&path)) { return; }
        if self.reject_archive(std::slice::from_ref(&path)) { return; }
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return;
        }
        let to = path.with_file_name(new_name);
        if to == path {
            return;
        }
        if to.exists() {
            self.status = format!("이미 있는 이름입니다: {new_name}");
            return;
        }
        match std::fs::rename(&path, &to) {
            Ok(()) => {
                if let Some(r) = self.rot.remove(&path) {
                    self.rot.insert(to.clone(), r);
                }
                if let Some(parent) = path.parent() {
                    self.tree_children.remove(parent);
                }
                self.tree_children.retain(|k, _| !k.starts_with(&path));
                for f in self.favorites.iter_mut().chain(self.recent.iter_mut()) {
                    if let Ok(rest) = f.strip_prefix(&path) {
                        *f = if rest.as_os_str().is_empty() { to.clone() } else { to.join(rest) };
                    }
                }
                for pi in 0..self.panes.len() {
                    if let Ok(rest) = self.panes[pi].cur_dir.clone().strip_prefix(&path) {
                        let new = if rest.as_os_str().is_empty() { to.clone() } else { to.join(rest) };
                        self.navigate(pi, new, false);
                    } else if path.parent() == Some(self.panes[pi].cur_dir.as_path()) {
                        self.reload(pi, (pi == self.active).then(|| to.clone()));
                    }
                }
            }
            Err(e) => self.status = format!("이름 바꾸기 실패: {e}"),
        }
    }

    fn run_actions(&mut self) {
        for a in std::mem::take(&mut self.actions) {
            self.run(a);
        }
    }

    fn run(&mut self, a: Action) {
        if matches!(&a, Action::Copy(_) | Action::Cut(_)) {
            self.clipboard_request = self.clipboard_request.wrapping_add(1);
        }
        if let Some(reason) = archive_action_error(&a) { self.status = reason.into(); return; }
        match a {
            Action::Open(pi, i) => self.open_entry(pi, i),
            Action::ToggleFullscreen => self.toggle_fullscreen(),
            Action::CloseViewer => self.close_viewer(),
            Action::Go(p) => self.navigate(self.active, p, true),
            Action::Up => self.go_up(),
            Action::NewFolder(parent) => self.ask_new_folder(parent),
            Action::AddFavorite(p) => {
                if !self.favorites.contains(&p) {
                    self.favorites.push(p);
                }
            }
            Action::RemoveFavorite(p) => self.favorites.retain(|f| f != &p),
            Action::Cut(ps) | Action::Copy(ps) if ps.is_empty() => {}
            Action::Cut(ps) => match fileops::clipboard_set_files(&ps, true) {
                Ok(()) => {
                    self.status = format!("{}개 항목을 잘라냈습니다", ps.len());
                    self.cut_paths = ps.into_iter().collect();
                }
                Err(e) => self.status = format!("잘라내기 실패: {e}"),
            },
            Action::Copy(ps) if ps.iter().any(|p| archive_read_only(p)) => {
                self.status = "복사하는 중…".into();
                let request = self.clipboard_request;
                let sequence = fileops::clipboard_sequence();
                let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(Msg::ArchiveCopied { request, sequence, res: archive::extract(&ps) });
                    ctx.request_repaint();
                });
            }
            Action::Copy(ps) => match fileops::clipboard_set_files(&ps, false) {
                Ok(()) => {
                    self.status = format!("{}개 항목을 복사했습니다", ps.len());
                    self.cut_paths.clear();
                }
                Err(e) => self.status = format!("복사 실패: {e}"),
            },
            Action::Paste(dest) => self.paste(dest),
            Action::Delete(ps) => self.ask_delete(ps),
            Action::StripPrivate(ps) => self.ask_strip_private(ps),
            Action::Rename(p) => self.ask_rename_path(p),
            Action::Properties(p) => fileops::show_properties(&p),
            Action::Refresh(d) => self.refresh_dir(&d),
            Action::Rotate(ps, d) => self.rotate_paths(ps, d),
            Action::ShowInExplorer(p) => self.show_in_explorer(&p),
            Action::Transfer(files, dest, mv) => self.transfer(files, dest, mv, false),
            Action::PickAndTransfer(files, mv) => {
                let title = if mv { "옮길 폴더 선택" } else { "복사할 폴더 선택" };
                if let Some(dest) = rfd::FileDialog::new().set_title(title).set_directory(&self.panes[self.active].cur_dir).pick_folder() {
                    self.transfer(files, dest, mv, false);
                }
            }
        }
    }

    /// Submenu listing the last folders files went to, plus a folder picker.
    fn dest_menu(&mut self, ui: &mut Ui, files: &[PathBuf], mv: bool) {
        icons::style_menu(ui, 280.0);
        let ic = self.icons.clone();
        let mut act: Option<Action> = None;
        for d in &self.transfer_dirs {
            if icons::menu_item(ui, Some(&ic.folder), d.display().to_string(), "", true).clicked() {
                act = Some(Action::Transfer(files.to_vec(), d.clone(), mv));
            }
        }
        if self.transfer_dirs.is_empty() {
            ui.weak("아직 없음");
        }
        icons::menu_sep(ui);
        if icons::menu_item(ui, Some(&ic.folder), "폴더 선택…", "", true).clicked() {
            act = Some(Action::PickAndTransfer(files.to_vec(), mv));
        }
        if let Some(a) = act {
            self.actions.push(a);
            ui.close_menu();
        }
    }

    /// Default drop: move within the same drive, copy to another drive (like
    /// Explorer). Ctrl forces copy, Shift forces move.
    fn drop_is_move(&self, files: &[PathBuf], dest: &Path) -> bool {
        let m = self.ctx.input(|i| i.modifiers);
        if m.ctrl {
            false
        } else if m.shift {
            true
        } else if !files.is_empty() && files.iter().all(|p| archive_read_only(p)) {
            false
        } else {
            files.first().is_some_and(|f| same_volume(f, dest))
        }
    }

    /// Make `resp` (a folder) accept dragged files.
    fn drop_target(&mut self, ui: &Ui, resp: &egui::Response, dir: &Path) {
        if archive::is_location(dir) || archive_read_only(dir) {
            if resp.dnd_hover_payload::<DragFiles>().is_some() { self.status = "압축 파일 안에서는 할 수 없습니다".into(); }
            return;
        }
        if let Some(drag) = resp.dnd_hover_payload::<DragFiles>() {
            self.dnd_folder_hover = true;
            let valid = drop_dest(dir, &drag.0).is_some();
            if valid {
                ui.painter().rect_stroke(
                    resp.rect.expand(1.0),
                    4.0f32,
                    Stroke::new(2.0f32, Color32::from_rgb(40, 120, 220)),
                    egui::StrokeKind::Outside,
                );
                self.dnd_target = Some(dir.to_path_buf());
            }
        }
        if let Some(drag) = resp.dnd_release_payload::<DragFiles>() {
            if self.dnd_target.as_deref() == Some(dir) {
                let mv = self.drop_is_move(&drag.0, dir);
                self.actions.push(Action::Transfer(drag.0.clone(), dir.to_path_buf(), mv));
            }
        }
    }

    /// Label that follows the pointer while dragging files.
    fn drag_overlay(&mut self, ctx: &egui::Context) {
        let Some(drag) = egui::DragAndDrop::payload::<DragFiles>(ctx) else {
            self.dnd_target = None;
            return;
        };
        // pointer left the window with the button still held: hand the files to
        // Windows so they can be dropped on a browser, Explorer, chat app...
        let (held, latest) = ctx.input(|i| (i.pointer.primary_down(), i.pointer.latest_pos()));
        let outside = match latest {
            Some(p) => !ctx.screen_rect().shrink(2.0).contains(p),
            None => true,
        };
        if held && outside {
            let files = drag.0.clone();
            egui::DragAndDrop::clear_payload(ctx);
            self.dnd_target = None;
            let n = files.len();
            let from_archive = files.iter().any(|p| archive_read_only(p));
            let prepared = if from_archive { archive::extract_for_drag(&files) } else { Ok(files) };
            match prepared.and_then(|files| fileops::drag_out(self.hwnd, &files, from_archive)) {
                Ok(0) => {}
                Ok(_) => self.status = format!("{n}개 항목을 다른 프로그램에 넘겼습니다"),
                Err(e) => self.status = format!("끌어놓기 실패: {e}"),
            }
            fileops::post_left_button_up(self.hwnd);
            ctx.request_repaint();
            return;
        }
        let Some(pos) = ctx.pointer_hover_pos() else { return };
        let text = match &self.dnd_target {
            Some(dest) => {
                let mv = self.drop_is_move(&drag.0, dest);
                ctx.set_cursor_icon(if mv { egui::CursorIcon::Move } else { egui::CursorIcon::Copy });
                let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dest.display().to_string());
                format!("{}개 → {name} ({})", drag.0.len(), if mv { "이동" } else { "복사" })
            }
            None => {
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                if drag.0.iter().all(|p| archive_read_only(p)) {
                    format!("{}개 항목 (복사)", drag.0.len())
                } else { format!("{}개 항목 (Ctrl: 복사, Shift: 이동)", drag.0.len()) }
            }
        };
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("dnd_label")));
        let at = pos + vec2(16.0, 18.0);
        let galley = painter.layout_no_wrap(text, FontId::proportional(13.0), Color32::WHITE);
        let r = Rect::from_min_size(at, galley.size()).expand(6.0);
        painter.rect_filled(r, 4.0f32, Color32::from_rgba_unmultiplied(30, 60, 110, 230));
        painter.galley(at, galley, Color32::WHITE);
        self.dnd_target = None; // drop targets set it again next frame
    }

    fn paste(&mut self, dest: PathBuf) {
        if archive::is_location(&dest) || self.reject_archive(std::slice::from_ref(&dest)) {
            self.status = "압축 파일 안에서는 할 수 없습니다".into(); return;
        }
        let Some((files, cut)) = fileops::clipboard_get_files() else {
            self.status = "붙여넣을 파일이 없습니다".into();
            return;
        };
        self.transfer(files, dest, cut, cut);
    }

    /// Copy or move `files` into `dest` on a worker thread with Explorer's dialogs.
    fn transfer(&mut self, files: Vec<PathBuf>, dest: PathBuf, cut: bool, from_clipboard: bool) {
        if self.reject_saving(&files) || self.reject_saving(std::slice::from_ref(&dest)) { return; }
        if let Some(reason) = archive_action_error(&Action::Transfer(files.clone(), dest.clone(), cut)) {
            self.status = reason.into(); return;
        }
        if files.is_empty() {
            return;
        }
        if cut && files.iter().all(|f| f.parent() == Some(dest.as_path())) {
            self.status = "같은 폴더라서 옮길 것이 없습니다".into();
            return;
        }
        if files.iter().any(|f| dest.starts_with(f)) {
            self.status = "폴더를 그 안으로 복사하거나 옮길 수 없습니다".into();
            return;
        }
        self.transfer_dirs.retain(|d| d != &dest);
        self.transfer_dirs.insert(0, dest.clone());
        self.transfer_dirs.truncate(TRANSFER_MAX);
        if cut {
            self.cut_paths.retain(|p| !files.contains(p));
        }
        let mut dirs = vec![dest.clone()];
        if cut {
            dirs.extend(files.iter().filter_map(|f| f.parent().map(Path::to_path_buf)));
        }
        dirs.sort();
        dirs.dedup();
        let n = files.len();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.status = format!("{}하는 중…", if cut { "옮기" } else { "복사" });
        self.transfers_running += 1;
        std::thread::spawn(move || {
            let res = if files.iter().any(|p| archive_read_only(p)) {
                archive::extract(&files).and_then(|extracted| fileops::shell_copy_move(&extracted, &dest, false))
            } else { fileops::shell_copy_move(&files, &dest, cut) };
            let status = match &res {
                Ok(true) => format!("{n}개 항목을 {}습니다", if cut { "옮겼" } else { "복사했" }),
                Ok(false) => "일부를 취소했습니다".into(),
                Err(e) => format!("{} 실패: {e}", if cut { "옮기기" } else { "복사" }),
            };
            let clear_clipboard = from_clipboard && cut && matches!(res, Ok(true));
            let _ = tx.send(Msg::OpDone { status, dirs, clear_clipboard });
            ctx.request_repaint();
        });
    }

    /// Returns whether to draw the egui fallback.
    fn native_menu(&mut self, resp: &egui::Response, targets: &[PathBuf], dir: &Path,
        folder_extras: bool, viewer: Option<(usize, usize)>) -> bool {
        if archive::is_archive(dir) && dir.is_file() { return true; }
        let key = resp.id.with("shell_fallback");
        if resp.secondary_clicked() {
            let failed = self.show_native_menu(targets, dir, folder_extras, viewer,
                resp.ctx.input(|i| i.pointer.interact_pos()));
            resp.ctx.data_mut(|d| d.insert_temp(key, failed));
        }
        resp.ctx.data(|d| d.get_temp::<bool>(key).unwrap_or(false))
    }

    fn show_native_menu(&mut self, targets: &[PathBuf], dir: &Path,
        folder_extras: bool, viewer: Option<(usize, usize)>, pos: Option<egui::Pos2>) -> bool {
        use shellmenu::{ExtraItem, Intercept, MenuResult};
        let mut actions = Vec::new();
        let mut extra = Vec::new();
        let mut add = |label: String, action: Action| {
            let id = actions.len() as u32;
            actions.push(action);
            ExtraItem { id, label, enabled: true, submenu: Vec::new() }
        };
        if folder_extras {
            let f = targets.first().cloned().unwrap_or_else(|| dir.to_path_buf());
            extra.push(if self.favorites.contains(&f) {
                add("즐겨찾기에서 제거".into(), Action::RemoveFavorite(f))
            } else {
                add("즐겨찾기에 추가".into(), Action::AddFavorite(f))
            });
            extra.push(add("새로고침".into(), Action::Refresh(dir.to_path_buf())));
        } else {
            if self.viewer.is_some() {
                extra.push(add(format!("{}\tF11", fullscreen_menu_label(self.fullscreen)), Action::ToggleFullscreen));
                extra.push(add("목록으로 돌아가기\tEsc".into(), Action::CloseViewer));
            } else if targets.len() == 1 {
                if let Some((pi, i)) = viewer {
                    extra.push(add("크게보기".into(), Action::Open(pi, i)));
                }
            }
            let eligible: Vec<_> = targets.iter().filter(|p| meta::can_strip(p)).cloned().collect();
            let enabled = !eligible.is_empty();
            let mut strip = add("개인 정보 지우기".into(), Action::StripPrivate(eligible));
            strip.enabled = enabled;
            extra.push(strip);
            for (label, mv) in [("복사할 곳", false), ("이동할 곳", true)] {
                let mut submenu = Vec::new();
                for dest in self.transfer_dirs.iter().take(TRANSFER_MAX) {
                    submenu.push(add(dest.display().to_string(), Action::Transfer(targets.to_vec(), dest.clone(), mv)));
                }
                submenu.push(add("폴더 선택…".into(), Action::PickAndTransfer(targets.to_vec(), mv)));
                extra.push(ExtraItem { id: u32::MAX, label: label.into(), enabled: true, submenu });
            }
        }
        let result = (|| {
            let pos = pos?;
            let scale = self.ctx.pixels_per_point();
            let hwnd = self.hwnd;
            if hwnd.0.is_null() { return None; }
            let mut point = windows::Win32::Foundation::POINT {
                x: (pos.x * scale).round() as i32, y: (pos.y * scale).round() as i32,
            };
            if !unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut point) }.as_bool() { return None; }
            let screen = (point.x, point.y);
            let result = if targets.is_empty() {
                shellmenu::show_for_folder_background(hwnd, screen, dir, &extra)
            } else {
                shellmenu::show_for_items(hwnd, screen, targets, &extra)
            };
            Some((result, shellmenu::failed()))
        })();
        let (result, failed) = result.unwrap_or((MenuResult::None, true));
        let paste_dir = if targets.len() == 1 && targets[0].is_dir() { targets[0].clone() } else { dir.to_path_buf() };
        let action = match result {
            MenuResult::Extra(id) => actions.into_iter().nth(id as usize),
            MenuResult::Intercepted(Intercept::Rename) => targets.first().cloned().map(Action::Rename),
            MenuResult::Intercepted(Intercept::Delete) => Some(Action::Delete(targets.to_vec())),
            MenuResult::Intercepted(Intercept::Cut) => Some(Action::Cut(targets.to_vec())),
            MenuResult::Intercepted(Intercept::Copy) => Some(Action::Copy(targets.to_vec())),
            MenuResult::Intercepted(Intercept::Paste) => Some(Action::Paste(paste_dir)),
            _ => None,
        };
        if let Some(action) = action { self.actions.push(action); }
        failed
    }

    /// Explorer-style menu. `targets` are the right-clicked items (empty = blank
    /// area); `dir` is the folder that "new folder", "paste" and "refresh" use.
    fn file_menu(&mut self, ui: &mut Ui, targets: Vec<PathBuf>, dir: PathBuf) {
        if archive::is_archive(&dir) && dir.is_file() {
            if !targets.is_empty() {
                if ui.button("복사(C)").clicked() { self.actions.push(Action::Copy(targets.clone())); ui.close_menu(); }
                ui.menu_button("복사할 곳", |ui| self.dest_menu(ui, &targets, false));
            }
            ui.add_enabled(false, egui::Button::new("개인 정보 지우기"));
            if let Some(path) = targets.first() {
                if ui.button("경로 복사").clicked() { ui.ctx().copy_text(path.display().to_string()); ui.close_menu(); }
            } else if ui.button("경로 복사").clicked() { ui.ctx().copy_text(dir.display().to_string()); ui.close_menu(); }
            return;
        }
        let one_dir = (targets.len() == 1 && targets[0].is_dir()).then(|| targets[0].clone());
        let fav_target = one_dir.clone().or_else(|| targets.is_empty().then(|| dir.clone()));
        let paste_dir = one_dir.clone().unwrap_or_else(|| dir.clone());
        let any = !targets.is_empty();
        let deletable = any && targets.iter().all(|p| p.parent().is_some());
        let mut act: Option<Action> = None;
        let ic = self.icons.clone();
        let item = |ui: &mut Ui, icon: Option<&TextureHandle>, enabled: bool, text: &str, key: &str| {
            icons::menu_item(ui, icon, text, key, enabled).clicked()
        };
        icons::style_menu(ui, 240.0);

        if item(ui, Some(&ic.folder), true, "폴더 생성(N)", "Ctrl+N") {
            act = Some(Action::NewFolder(paste_dir.clone()));
        }
        match &fav_target {
            Some(f) if self.favorites.contains(f) => {
                if item(ui, Some(&ic.star), true, "즐겨찾기에서 제거(A)", "") {
                    act = Some(Action::RemoveFavorite(f.clone()));
                }
            }
            _ => {
                if item(ui, Some(&ic.star), fav_target.is_some(), "즐겨찾기에 추가(A)", "") {
                    act = fav_target.clone().map(Action::AddFavorite);
                }
            }
        }
        icons::menu_sep(ui);
        if item(ui, None, deletable, "잘라내기(X)", "Ctrl+X") {
            act = Some(Action::Cut(targets.clone()));
        }
        if item(ui, None, any, "복사(C)", "Ctrl+C") {
            act = Some(Action::Copy(targets.clone()));
        }
        if item(ui, None, fileops::clipboard_has_files(), "붙여넣기(V)", "Ctrl+V") {
            act = Some(Action::Paste(paste_dir.clone()));
        }
        icons::menu_sep(ui);
        if item(ui, Some(&ic.delete), deletable, "삭제(D)", "Del") {
            act = Some(Action::Delete(targets.clone()));
        }
        let eligible: Vec<_> = targets.iter().filter(|p| meta::can_strip(p)).cloned().collect();
        if item(ui, None, !eligible.is_empty(), "개인 정보 지우기", "") {
            act = Some(Action::StripPrivate(eligible));
        }
        if item(ui, Some(&ic.rename), targets.len() == 1 && deletable, "이름 변경(U)", "F2") {
            act = Some(Action::Rename(targets[0].clone()));
        }
        icons::menu_sep(ui);
        if item(ui, None, true, "파일 속성(Z)", "") {
            act = Some(Action::Properties(targets.first().cloned().unwrap_or_else(|| dir.clone())));
        }
        if item(ui, Some(&ic.refresh), true, "새로고침(Y)", "F5") {
            act = Some(Action::Refresh(dir.clone()));
        }
        if let Some(a) = act {
            self.actions.push(a);
            ui.close_menu();
        }
    }

    fn show_in_explorer(&self, p: &Path) {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", p.display()))
            .spawn();
    }

    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
    }

    // ------------------------------------------------------------ tree

    fn open_tree_to(&mut self, dir: &Path) {
        for a in dir.ancestors().skip(1) {
            let mut st = CollapsingState::load_with_default_open(&self.ctx, tree_id(a), false);
            st.set_open(true);
            st.store(&self.ctx);
        }
        self.tree_scroll_pending = true;
    }

    fn tree_ui(&mut self, ui: &mut Ui) {
        let drives = self.drives.clone();
        let mut nav = None;
        for d in &drives {
            self.tree_node(ui, d, d.display().to_string(), &mut nav);
        }
        if let Some(p) = nav {
            self.navigate(self.active, p, true);
        }
    }

    fn tree_node(&mut self, ui: &mut Ui, path: &Path, label: String, nav: &mut Option<PathBuf>) {
        let state = CollapsingState::load_with_default_open(ui.ctx(), tree_id(path), false);
        let selected = self.panes[self.active].cur_dir == path;
        let folder = self.icons.folder.id();
        let header = state.show_header(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.add(egui::Image::new(SizedTexture::new(folder, vec2(16.0, 16.0))));
            // a folder can be dragged from the tree like a thumbnail (not drive roots)
            let r = ui.selectable_label(selected, label).interact(Sense::drag());
            if r.drag_started() && path.parent().is_some() {
                r.dnd_set_drag_payload(DragFiles(vec![path.to_path_buf()]));
            }
            self.drop_target(ui, &r, path);
            if r.clicked() {
                *nav = Some(path.to_path_buf());
            }
            if self.native_menu(&r, &[path.to_path_buf()], path, true, None) {
                r.context_menu(|ui| self.file_menu(ui, vec![path.to_path_buf()], path.to_path_buf()));
            }
            if selected && self.tree_scroll_pending {
                r.scroll_to_me(Some(Align::Center));
                self.tree_scroll_pending = false;
            }
        });
        header.body(|ui| {
            let kids = self
                .tree_children
                .entry(path.to_path_buf())
                .or_insert_with(|| list_subdirs(path))
                .clone();
            if kids.is_empty() {
                ui.weak("(하위 폴더 없음)");
            }
            for k in kids {
                let name = k.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                self.tree_node(ui, &k, name, nav);
            }
        });
    }

    // ------------------------------------------------------------ UI: browser

    fn modal_open(&self) -> bool {
        self.dialog.is_some() || self.show_settings || self.show_about
    }

    fn grid_keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() || self.modal_open() {
            return;
        }
        let (mods, back_btn, fwd_btn) = ctx.input(|i| {
            (
                i.modifiers,
                i.pointer.button_pressed(egui::PointerButton::Extra1),
                i.pointer.button_pressed(egui::PointerButton::Extra2),
            )
        });
        let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));
        if pressed(Key::F11) {
            self.open_selected_fullscreen();
            return;
        }
        if ctx.input_mut(|i| i.consume_key(mods, Key::Tab)) {
            let n = self.panes.len();
            self.activate((self.active + if mods.shift { n - 1 } else { 1 }) % n);
            return;
        }
        if back_btn || (mods.alt && pressed(Key::ArrowLeft)) {
            self.go_back();
            return;
        }
        if fwd_btn || (mods.alt && pressed(Key::ArrowRight)) {
            self.go_fwd();
            return;
        }
        if (mods.alt && pressed(Key::ArrowUp)) || pressed(Key::Backspace) {
            self.go_up();
            return;
        }
        if pressed(Key::F5) {
            self.actions.push(Action::Refresh(self.panes[self.active].cur_dir.clone()));
            return;
        }
        for (k, key) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4].into_iter().enumerate() {
            if mods.command && pressed(key) {
                self.set_kind_filter(KindFilter::ALL[k]);
                return;
            }
        }
        if mods.command && pressed(Key::N) {
            self.actions.push(Action::NewFolder(self.panes[self.active].cur_dir.clone()));
            return;
        }
        // egui turns Ctrl+C / Ctrl+X into these events instead of key presses
        let (copy, cut) = ctx.input(|i| {
            (
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                i.events.iter().any(|e| matches!(e, egui::Event::Cut)),
            )
        });
        if archive::is_location(&self.pane().cur_dir)
            && (cut || pressed(Key::Delete) || pressed(Key::F2)) {
            self.status = if cut { "압축 파일 안의 그림은 옮길 수 없습니다. 복사만 됩니다" }
                else { "압축 파일 안에서는 할 수 없습니다" }.into();
            return;
        }
        if copy {
            self.actions.push(Action::Copy(self.pane().selected_paths()));
        }
        if cut {
            self.actions.push(Action::Cut(self.pane().selected_paths()));
        }
        let n = self.panes[self.active].entries.len();
        if n == 0 {
            return;
        }
        let page = (self.panes[self.active].grid_view_h / (self.cell_size().y + GAP)).floor().max(1.0) as isize;
        let mut delta: Option<isize> = None; // along the list
        let mut vrows: Option<isize> = None; // rows up / down
        let mut target: Option<usize> = None;
        if pressed(Key::ArrowRight) { delta = Some(1) }
        if pressed(Key::ArrowLeft) { delta = Some(-1) }
        if pressed(Key::ArrowDown) { vrows = Some(1) }
        if pressed(Key::ArrowUp) { vrows = Some(-1) }
        if pressed(Key::PageDown) { vrows = Some(page) }
        if pressed(Key::PageUp) { vrows = Some(-page) }
        if pressed(Key::Home) { target = Some(0) }
        if pressed(Key::End) { target = Some(n - 1) }
        if let Some(d) = delta {
            target = Some(match self.panes[self.active].focus {
                None => 0,
                Some(f) => (f as isize + d).clamp(0, n as isize - 1) as usize,
            });
        }
        if let Some(k) = vrows {
            // rows can differ in width (small folder tiles), so step by geometry
            target = Some(match self.panes[self.active].focus {
                None => 0,
                Some(f) => {
                    let off = self.pane().slot_off();
                    let lay = self.pane().layout(self.panes[self.active].grid_w, self.thumb_size);
                    let slot = lay.step_rows(f + off, k).unwrap_or(f + off);
                    slot.max(off) - off // never land on ".."
                }
            });
        }
        if let Some(t) = target {
            self.move_focus(self.active, t, mods.shift);
        }
        if mods.command && pressed(Key::A) {
            self.panes[self.active].selected = (0..n).collect();
        }
        if pressed(Key::Enter) {
            if let Some(f) = self.panes[self.active].focus {
                self.open_entry(self.active, f);
            }
        }
        if pressed(Key::Space) {
            self.audio_toggle();
        }
        if pressed(Key::Delete) {
            self.actions.push(Action::Delete(self.pane().selected_paths()));
        }
        if pressed(Key::F2) {
            if let Some(p) = self.panes[self.active].focus.and_then(|i| self.panes[self.active].entries.get(i)).map(|e| e.path.clone()) {
                self.actions.push(Action::Rename(p));
            }
        }
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        use icons::tool_button;
        let ic = self.icons.clone();
        let writable = !archive::is_location(&self.pane().cur_dir);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if tool_button(ui, &ic.back, "뒤로", !self.panes[self.active].back.is_empty()).on_hover_text("Alt+←").clicked() {
                self.go_back();
            }
            if tool_button(ui, &ic.forward, "앞으로", !self.panes[self.active].fwd.is_empty()).on_hover_text("Alt+→").clicked() {
                self.go_fwd();
            }
            if tool_button(ui, &ic.up, "위로", self.panes[self.active].cur_dir.parent().is_some()).on_hover_text("Backspace").clicked() {
                self.go_up();
            }
            let r = tool_button(ui, &ic.recent, "최근", true).on_hover_text("최근에 연 폴더 10개");
            let pid = ui.make_persistent_id("recent_popup");
            // PICVIEW_SHOW_MENU=recent opens this popup once at start (screenshots)
            static RECENT_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if std::env::var("PICVIEW_SHOW_MENU").is_ok_and(|v| v == "recent") && !RECENT_SHOWN.swap(true, Ordering::Relaxed) {
                ui.memory_mut(|m| m.open_popup(pid));
            }
            if r.clicked() {
                ui.memory_mut(|m| m.toggle_popup(pid));
            }
            let mut go: Option<PathBuf> = None;
            egui::popup_below_widget(ui, pid, &r, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                // wide enough for long paths
                icons::style_menu(ui, 640.0);
                for (k, p) in self.recent.iter().enumerate() {
                    let text = egui::RichText::new(format!("{}   {}", k + 1, p.display()));
                    let text = if *p == self.panes[self.active].cur_dir { text.strong() } else { text };
                    if icons::menu_item(ui, Some(&ic.folder), text, "", true).clicked() {
                        go = Some(p.clone());
                    }
                }
                if self.recent.is_empty() {
                    ui.weak("없음");
                }
            });
            if let Some(p) = go {
                self.navigate(self.active, p, true);
            }
            if tool_button(ui, &ic.refresh, "새로고침", true).on_hover_text("F5").clicked() {
                self.actions.push(Action::Refresh(self.panes[self.active].cur_dir.clone()));
            }
            ui.separator();

            let has_img = self.panes[self.active].focus.and_then(|i| self.panes[self.active].entries.get(i)).map(|e| e.is_img()).unwrap_or(false);
            let has_sel = writable && !self.pane().selected_paths().is_empty();
            if tool_button(ui, &ic.view, "크게보기", has_img).on_hover_text("Enter").clicked() {
                if let Some(f) = self.panes[self.active].focus {
                    self.open_entry(self.active, f);
                }
            }
            if icons::fullscreen_tool_button(ui, has_img).on_hover_text("전체 화면으로 보기 (F11)").clicked() {
                self.open_selected_fullscreen();
            }
            let left = tool_button(ui, &ic.rotate_left, "-90° 회전", has_sel).on_hover_text("파일에 저장됩니다");
            if left.clicked() {
                self.rotate_current(-1);
            }
            let right = tool_button(ui, &ic.rotate_right, "90° 회전", has_sel).on_hover_text("파일에 저장됩니다");
            if right.clicked() {
                self.rotate_current(1);
            }
            let rename = tool_button(ui, &ic.rename, "이름변경", writable && self.panes[self.active].focus.is_some()).on_hover_text("F2");
            if rename.clicked() {
                self.ask_rename();
            }
            let delete = tool_button(ui, &ic.delete, "삭제", has_sel).on_hover_text("Delete (휴지통으로)");
            if delete.clicked() {
                self.actions.push(Action::Delete(self.pane().selected_paths()));
            }
            if !writable && ui.input(|i| i.pointer.primary_clicked())
                && [&left, &right, &rename, &delete].iter().any(|r| ui.rect_contains_pointer(r.rect)) {
                self.status = "압축 파일 안에서는 할 수 없습니다".into();
            }
            if tool_button(ui, &ic.explorer, "탐색기", true).on_hover_text("탐색기에서 열기").clicked() {
                let p = self.panes[self.active].focus.and_then(|i| self.panes[self.active].entries.get(i)).map(|e| e.path.clone());
                match p {
                    Some(p) => self.show_in_explorer(&p),
                    None => {
                        let _ = std::process::Command::new("explorer").arg(&self.panes[self.active].cur_dir).spawn();
                    }
                }
            }
            let r = tool_button(ui, &ic.more, "더보기", true);
            let pid = ui.make_persistent_id("more_popup");
            // PICVIEW_SHOW_MENU=1 opens this popup once at start (for screenshots of the menu style)
            static SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if std::env::var("PICVIEW_SHOW_MENU").is_ok_and(|v| v != "recent") && !SHOWN.swap(true, Ordering::Relaxed) {
                ui.memory_mut(|m| m.open_popup(pid));
            }
            if r.clicked() {
                ui.memory_mut(|m| m.toggle_popup(pid));
            }
            egui::popup_below_widget(ui, pid, &r, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                icons::style_menu(ui, 240.0);
                if icons::menu_item(ui, Some(&ic.settings), "설정…", "", true).clicked() {
                    self.open_settings();
                }
                icons::menu_sep(ui);
                if icons::menu_item(ui, Some(&ic.delete), "썸네일 캐시 비우기", "", true).clicked() {
                    self.status = match cache::clear() {
                        Ok(()) => "썸네일 캐시를 비웠습니다".into(),
                        Err(e) => format!("캐시 비우기 실패: {e}"),
                    };
                }
                if icons::menu_item(ui, Some(&ic.folder), "캐시 폴더 열기", "", true).clicked() {
                    if let Some(d) = cache::dir() {
                        let _ = std::process::Command::new("explorer").arg(d).spawn();
                    }
                }
            });
            if tool_button(ui, &ic.settings, "설정", true).clicked() {
                self.open_settings();
            }
            ui.separator();

            // sort and size controls, centred on the button row
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), icons::H),
                egui::Layout::left_to_right(Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let before = (self.sort, self.sort_desc);
                    // two rows: sort key on top, direction below
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.spacing_mut().interact_size.y = 18.0;
                        ui.spacing_mut().button_padding.y = 0.0;
                        egui::ComboBox::from_id_salt("sort")
                            .width(84.0)
                            .selected_text(match self.sort {
                                SortKey::Name => "이름순",
                                SortKey::Date => "날짜순",
                                SortKey::Size => "크기순",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.sort, SortKey::Name, "이름순");
                                ui.selectable_value(&mut self.sort, SortKey::Date, "날짜순");
                                ui.selectable_value(&mut self.sort, SortKey::Size, "크기순");
                            })
                            .response
                            .on_hover_text("정렬 기준");
                        let dir = egui::Button::new(if self.sort_desc { "↓ 내림차순" } else { "↑ 오름차순" })
                            .min_size(vec2(88.0, 18.0));
                        if ui.add(dir).on_hover_text("누르면 순서를 뒤집습니다").clicked() {
                            self.sort_desc = !self.sort_desc;
                        }
                    });
                    if before != (self.sort, self.sort_desc) {
                        for pi in 0..self.panes.len() {
                            self.sort_entries(pi);
                        }
                    }
                    ui.separator();
                    self.kind_filter_buttons(ui);
                    ui.label("패널");
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
                        ui.spacing_mut().button_padding = vec2(6.0, 0.0);
                        for row in [
                            &[(Split::One, "하나", "목록을 하나로 보기")][..],
                            &[
                                (Split::LeftRight, "좌우", "목록을 좌우 두 패널로 나누기"),
                                (Split::TopBottom, "상하", "목록을 위아래 두 패널로 나누기"),
                            ][..],
                        ] {
                            ui.horizontal(|ui| {
                                for &(split, _label, hint) in row {
                                    // drawn icon instead of text: a box, split the way the panes are
                                    let on = self.split == split;
                                    let b = egui::Button::new("").min_size(vec2(26.0, 18.0))
                                        .corner_radius(9.0)
                                        .fill(if on { Color32::from_rgb(40, 120, 220) } else { Color32::from_gray(232) });
                                    let resp = ui.add(b);
                                    let ink = if on { Color32::WHITE } else { Color32::from_gray(60) };
                                    let r = Rect::from_center_size(resp.rect.center(), vec2(14.0, 10.0));
                                    let st = Stroke::new(1.2f32, ink);
                                    ui.painter().rect_stroke(r, 1.0f32, st, egui::StrokeKind::Middle);
                                    match split {
                                        Split::LeftRight => { ui.painter().vline(r.center().x, r.y_range(), st); }
                                        Split::TopBottom => { ui.painter().hline(r.x_range(), r.center().y, st); }
                                        Split::One => {}
                                    }
                                    if resp.on_hover_text(hint).clicked() {
                                        self.set_split(split);
                                    }
                                }
                            });
                        }
                    });
                },
            );
        });
    }

    fn address_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("주소");
            let r = ui.add(egui::TextEdit::singleline(&mut self.panes[self.active].addr_text).desired_width(f32::INFINITY));
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                self.open_address();
            }
        });
    }

    /// 전체 / 그림 / 동영상 / 음성 buttons with counts (toolbar).
    fn kind_filter_buttons(&mut self, ui: &mut Ui) {
        // two rows (전체 그림 / 동영상 음성) so the toolbar stays narrow
        let mut picked = None;
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
            ui.spacing_mut().button_padding = vec2(6.0, 0.0);
            for row in KindFilter::ALL.chunks(2) {
                ui.horizontal(|ui| {
                    for f in row {
                        let k = KindFilter::ALL.iter().position(|x| x == f).unwrap_or(0);
                        let on = self.kind_filter == *f;
                        let n = self.panes[self.active].kind_counts[k];
                        let text = egui::RichText::new(format!("{} {n}", f.label())).size(12.0).color(if on {
                            Color32::WHITE
                        } else if n == 0 && k > 0 {
                            Color32::from_gray(160)
                        } else {
                            Color32::from_gray(40)
                        });
                        let b = egui::Button::new(text)
                            .min_size(vec2(68.0, 18.0))
                            .corner_radius(9.0)
                            .fill(if on { Color32::from_rgb(40, 120, 220) } else { Color32::from_gray(232) });
                        let tip = match f {
                            KindFilter::All => "폴더의 파일을 모두 보기 (Ctrl+1)".to_string(),
                            _ => format!("{}만 보기 (Ctrl+{})", f.label(), k + 1),
                        };
                        if ui.add(b).on_hover_text(tip).clicked() {
                            picked = Some(*f);
                        }
                    }
                });
            }
        });
        if let Some(f) = picked {
            self.set_kind_filter(f);
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            // thumbnail size at the right end, like Explorer's zoom slider
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button(" + ").on_hover_text("크게").clicked() {
                    self.thumb_size = (self.thumb_size + 16.0).min(256.0);
                }
                ui.spacing_mut().slider_width = 110.0;
                ui.add(egui::Slider::new(&mut self.thumb_size, 64.0..=256.0).show_value(false))
                    .on_hover_text("썸네일 크기");
                if ui.small_button(" - ").on_hover_text("작게").clicked() {
                    self.thumb_size = (self.thumb_size - 16.0).max(64.0);
                }
                ui.label("크기");
                ui.separator();
                ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| self.status_left(ui));
            });
        });
    }

    fn status_left(&mut self, ui: &mut Ui) {
        {
            match self.panes[self.active].focus_dims {
                Some([w, h]) => ui.label(format!("{w} x {h}")),
                None => ui.label("-"),
            };
            ui.separator();
            let sel_size: u64 = self.panes[self.active].selected.iter().filter_map(|&i| self.panes[self.active].entries.get(i)).map(|e| e.size).sum();
            ui.label(format!("{} 개체 선택 ({})", self.panes[self.active].selected.len(), fmt_size(sel_size)));
            ui.separator();
            let imgs = self.panes[self.active].entries.iter().filter(|e| e.is_img() || (!e.is_dir && archive::is_archive(&e.path))).count();
            let media = self.panes[self.active].entries.iter().filter(|e| e.media.is_some()).count();
            if media > 0 {
                ui.label(format!("총 {} 개체 (그림 {imgs}, 동영상·음성 {media})", self.panes[self.active].entries.len()));
            } else {
                ui.label(format!("총 {} 개체 (그림 {imgs})", self.panes[self.active].entries.len()));
            }
            if self.kind_filter != KindFilter::All {
                let hidden = self.panes[self.active].kind_counts[0].saturating_sub(self.panes[self.active].entries.iter().filter(|e| !e.is_dir).count());
                ui.separator();
                ui.colored_label(
                    Color32::from_rgb(40, 120, 220),
                    format!("{}만 보는 중 (가린 파일 {hidden}개)", self.kind_filter.label()),
                );
            }
            if !self.status.is_empty() {
                ui.separator();
                ui.colored_label(Color32::from_rgb(180, 60, 20), &self.status);
            }
        }
    }

    fn quick_access(&mut self, ui: &mut Ui) {
        let home = std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_default();
        let items = [
            ("바탕화면", home.join("Desktop")),
            ("다운로드", home.join("Downloads")),
            ("사진", home.join("Pictures")),
            ("문서", home.join("Documents")),
            ("홈", home.clone()),
        ];
        ui.horizontal_wrapped(|ui| {
            for (label, p) in items {
                if p.is_dir() && ui.small_button(label).clicked() {
                    self.navigate(self.active, p, true);
                }
            }
        });
        if !self.favorites.is_empty() {
            ui.add_space(4.0);
            ui.weak("즐겨찾기");
            for f in self.favorites.clone() {
                let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| f.display().to_string());
                let star = self.icons.star.id();
                let r = ui
                    .horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.add(egui::Image::new(SizedTexture::new(star, vec2(16.0, 16.0))));
                        ui.selectable_label(self.panes[self.active].cur_dir == f, name)
                    })
                    .inner
                    .on_hover_text(f.display().to_string());
                self.drop_target(ui, &r, &f);
                if r.clicked() {
                    self.actions.push(Action::Go(f.clone()));
                }
                if self.native_menu(&r, &[f.clone()], &f, true, None) {
                    r.context_menu(|ui| self.file_menu(ui, vec![f.clone()], f.clone()));
                }
            }
        }
    }

    fn grid(&mut self, ui: &mut Ui, pi: usize) {
        if self.panes[pi].archive_loading {
            ui.vertical_centered(|ui| { ui.spinner(); ui.label("읽는 중…"); });
            if self.panes[pi].cur_dir.parent().is_some() && ui.button("..  (상위 폴더)").clicked() {
                self.activate(pi); self.actions.push(Action::Up);
            }
            return;
        }
        // empty folder: the whole panel is blank area (see `blank_area`)
        let n = self.panes[pi].entries.len();
        let off = self.panes[pi].slot_off();
        if n + off == 0 {
            let msg = match self.kind_filter {
                KindFilter::All => "그림이 없습니다".to_string(),
                f => format!("이 폴더에 {}이 없습니다", f.label()),
            };
            let r = ui.max_rect();
            self.blank_area(ui, pi, r);
            ui.centered_and_justified(|ui| ui.weak(msg));
            return;
        }
        let avail = ui.available_width();
        self.panes[pi].grid_w = avail;
        let lay = self.panes[pi].layout(avail, self.thumb_size);
        if self.panes[pi].grid_scroll_to.is_some() {
            self.panes[pi].scroll_anim = None; // keyboard / navigation jumps win over the animation
        }
        self.wheel_rows(ui, pi, &lay);
        if let Some((from, to, start)) = self.panes[pi].scroll_anim {
            let t = (start.elapsed().as_secs_f32() / SCROLL_ANIM.as_secs_f32()).min(1.0);
            let ease = 1.0 - (1.0 - t).powi(3); // ease-out cubic
            self.panes[pi].grid_scroll_to = Some(from + (to - from) * ease);
            if t >= 1.0 {
                self.panes[pi].scroll_anim = None;
            } else {
                ui.ctx().request_repaint();
            }
        }

        let mut sa = egui::ScrollArea::vertical().auto_shrink(false).id_salt("grid");
        if let Some(off) = self.panes[pi].grid_scroll_to.take() {
            sa = sa.vertical_scroll_offset(off);
        }
        let out = sa.show_viewport(ui, |ui, vp| {
            ui.set_height(lay.total_h);
            let origin = ui.max_rect().min;
            // blank area under the cells, registered first so the cells sit on top of it
            let blank = Rect::from_min_size(origin, vec2(avail, lay.total_h.max(vp.height())));
            self.blank_area(ui, pi, blank);
            if let Some(y) = lay.split_y {
                let y = origin.y + y;
                ui.painter().hline(origin.x..=origin.x + avail, y, Stroke::new(1.0f32, Color32::from_gray(215)));
            }
            let visible: Vec<usize> = (0..lay.rows.len())
                .filter(|&r| {
                    let row = lay.rows[r];
                    row.top + row.cell.y >= vp.min.y && row.top <= vp.max.y
                })
                .collect();
            for &r in &visible {
                let row = lay.rows[r];
                for c in 0..row.count {
                    let slot = row.first + c;
                    let rect = Rect::from_min_size(origin + vec2(c as f32 * (row.cell.x + GAP), row.top), row.cell);
                    if slot < off {
                        self.up_cell(ui, pi, rect);
                    } else {
                        self.cell(ui, pi, slot - off, rect);
                    }
                }
            }
            // grid slots -> entry indices (slot 0 may be ".."); visible first, then two rows ahead
            if let (Some(&r0), Some(&r1)) = (visible.first(), visible.last()) {
                let s = lay.rows[r0].first.saturating_sub(off);
                let last = lay.rows[r1];
                let e = (last.first + last.count).saturating_sub(off).min(n);
                self.panes[pi].vis_start.store(s, Ordering::Relaxed);
                self.panes[pi].vis_end.store(e, Ordering::Relaxed);
                let ahead = lay.rows.get(r1 + 2).or(lay.rows.last()).map(|r| r.first + r.count).unwrap_or(0);
                for i in s..ahead.saturating_sub(off).min(n) {
                    self.request_thumb(pi, i);
                }
            }
        });
        self.panes[pi].grid_offset = out.state.offset.y;
        self.panes[pi].grid_view_h = out.inner_rect.height();
    }

    /// Blank part of the list: a click clears the selection, a right-click opens
    /// the folder menu (new folder, paste, refresh, properties...).
    fn blank_area(&mut self, ui: &mut Ui, pi: usize, rect: Rect) {
        let bg = ui.interact(rect, ui.id().with("grid_bg"), Sense::click());
        if bg.clicked() || bg.secondary_clicked() || bg.drag_started() { self.activate(pi); }
        if bg.clicked() {
            self.panes[pi].selected.clear();
        }
        let dir = self.panes[pi].cur_dir.clone();
        if self.native_menu(&bg, &[], &dir, true, None) {
            bg.context_menu(|ui| self.file_menu(ui, Vec::new(), dir.clone()));
        }
    }

    /// One wheel notch scrolls exactly one row of thumbnails.
    fn wheel_rows(&mut self, ui: &mut Ui, pi: usize, lay: &GridLayout) {
        if !ui.rect_contains_pointer(ui.max_rect()) {
            return;
        }
        // Ctrl+wheel: thumbnail size, 16 px per notch (like Explorer)
        let zoom: f32 = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::MouseWheel { unit, delta, modifiers } if modifiers.command => Some(match unit {
                        egui::MouseWheelUnit::Point => delta.y / 50.0,
                        _ => delta.y,
                    }),
                    _ => None,
                })
                .sum()
        });
        if zoom != 0.0 {
            self.panes[pi].wheel_acc += zoom;
            let steps = self.panes[pi].wheel_acc.trunc();
            self.panes[pi].wheel_acc -= steps;
            if steps != 0.0 {
                self.thumb_size = (self.thumb_size + steps * 16.0).clamp(64.0, 256.0);
                if let Some(f) = self.panes[pi].focus {
                    self.ensure_visible(pi, f);
                }
            }
        }
        let pitch = self.cell_size().y + GAP;
        let page_rows = (self.panes[pi].grid_view_h / pitch).floor().max(1.0);
        let notches: f32 = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::MouseWheel { unit, delta, modifiers } if !modifiers.command => Some(match unit {
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Point => delta.y / 50.0, // touchpads
                        egui::MouseWheelUnit::Page => delta.y * page_rows,
                    }),
                    _ => None,
                })
                .sum()
        });
        // keep the ScrollArea from also scrolling by pixels
        ui.ctx().input_mut(|i| {
            i.smooth_scroll_delta.y = 0.0;
            i.raw_scroll_delta.y = 0.0;
        });
        if notches == 0.0 {
            return;
        }
        self.panes[pi].wheel_acc += notches;
        let steps = self.panes[pi].wheel_acc.trunc();
        self.panes[pi].wheel_acc -= steps;
        if steps == 0.0 {
            return;
        }
        // chain from where the running animation is headed, not where it is now
        let base = match self.panes[pi].scroll_anim {
            Some((_, to, _)) => to,
            None => self.panes[pi].grid_scroll_to.unwrap_or(self.panes[pi].grid_offset),
        };
        if lay.rows.is_empty() {
            return;
        }
        // row whose top is nearest the current position, then step from it
        let cur = lay
            .rows
            .iter()
            .enumerate()
            .min_by(|a, b| (a.1.top - base).abs().total_cmp(&(b.1.top - base).abs()))
            .map(|(i, _)| i as isize)
            .unwrap_or(0);
        let row = (cur - steps as isize).clamp(0, lay.rows.len() as isize - 1) as usize; // wheel up (+) = earlier rows
        let max = (lay.total_h - self.panes[pi].grid_view_h).max(0.0);
        let to = lay.rows[row].top.clamp(0.0, max);
        self.panes[pi].grid_scroll_to = None;
        self.panes[pi].scroll_anim = Some((self.panes[pi].grid_offset, to, Instant::now()));
    }

    /// The ".." cell: double-click goes up, files dropped on it move to the parent.
    fn up_cell(&mut self, ui: &mut Ui, pi: usize, rect: Rect) {
        let Some(parent) = self.panes[pi].cur_dir.parent().map(Path::to_path_buf) else { return };
        let resp = ui.interact(rect, ui.id().with("cell_up"), Sense::click());
        if resp.clicked() || resp.secondary_clicked() || resp.drag_started() { self.activate(pi); }
        let painter = ui.painter();
        // ".." is a way out, not a folder of this list: blue icon on a grey-blue card
        let bg = if resp.hovered() { Color32::from_rgb(225, 236, 250) } else { Color32::from_rgb(240, 244, 250) };
        painter.rect_filled(rect, 4.0f32, bg);
        painter.rect_stroke(rect, 4.0f32, Stroke::new(1.0f32, Color32::from_rgb(196, 210, 230)), egui::StrokeKind::Inside);
        let ts = rect.width() - 16.0;
        let img_area = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(ts, ts));
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        let fr = Rect::from_center_size(img_area.center(), img_area.size() * 0.60);
        painter.image(self.icons.folder_up.id(), fr, uv, Color32::WHITE);
        let ar = Rect::from_center_size(fr.right_bottom() - fr.size() * 0.18, fr.size() * 0.5);
        painter.image(self.icons.up.id(), ar, uv, Color32::WHITE);
        painter.text(
            pos2(rect.center().x, rect.max.y - 11.0),
            Align2::CENTER_CENTER,
            "..  (상위 폴더)",
            FontId::proportional(13.0),
            Color32::from_gray(30),
        );
        let resp = resp.on_hover_text(parent.display().to_string());
        if resp.double_clicked() {
            self.actions.push(Action::Up);
        }
        self.drop_target(ui, &resp, &parent);
    }

    fn cell(&mut self, ui: &mut Ui, pi: usize, i: usize, rect: Rect) {
        let resp = ui.interact(rect, ui.id().with(("cell", i)), Sense::click_and_drag());
        if resp.clicked() || resp.secondary_clicked() || resp.drag_started() { self.activate(pi); }
        if resp.drag_started() {
            if !self.panes[pi].selected.contains(&i) {
                self.select_single(pi, i);
            }
            resp.dnd_set_drag_payload(DragFiles(self.panes[pi].selected_paths()));
        }
        let selected = self.panes[pi].selected.contains(&i);
        let painter = ui.painter();
        let bg = if selected {
            Color32::from_rgb(204, 228, 255)
        } else if resp.hovered() {
            Color32::from_rgb(235, 244, 255)
        } else {
            Color32::WHITE
        };
        let border = if selected { Color32::from_rgb(100, 160, 230) } else { Color32::from_gray(215) };
        painter.rect_filled(rect, 4.0f32, bg);
        painter.rect_stroke(rect, 4.0f32, Stroke::new(1.0f32, border), egui::StrokeKind::Inside);

        let ts = rect.width() - 16.0;
        let img_area = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(ts, ts));
        let e = &self.panes[pi].entries[i];
        if e.is_dir {
            let fr = Rect::from_center_size(img_area.center(), img_area.size() * 0.60);
            painter.image(self.icons.folder.id(), fr, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        } else {
            // video under the pointer: after a short rest, play its preview frames
            let mut clip = None;
            if e.media == Some(loader::Media::Video) && resp.hovered() {
                match self.panes[pi].clip_hover {
                    Some((h, since)) if h == i => {
                        if since.elapsed() >= CLIP_HOVER {
                            let path = e.path.clone();
                            self.request_clip(pi, i);
                            clip = self.clip_frame(&path);
                        } else {
                            ui.ctx().request_repaint_after(CLIP_HOVER - since.elapsed());
                        }
                    }
                    _ => {
                        self.panes[pi].clip_hover = Some((i, Instant::now()));
                        ui.ctx().request_repaint_after(CLIP_HOVER);
                    }
                }
            }
            let e = &self.panes[pi].entries[i];
            let painter = ui.painter();
            match &self.panes[pi].thumbs[i] {
                _ if clip.is_some() => {
                    let (t, k) = clip.unwrap();
                    draw_image(ui, &t, t.size_vec2(), img_area, 0, None, Vec2::ZERO);
                    clip_progress(ui, img_area, t.size_vec2(), k);
                }
                Thumb::Ready(t) => {
                    draw_image(ui, t, t.size_vec2(), img_area, self.disp_rot(&e.path, t), None, Vec2::ZERO);
                }
                Thumb::Failed => {
                    painter.text(img_area.center(), Align2::CENTER_CENTER, if archive::is_archive(&e.path) { "그림 없음" } else { "읽을 수 없음" }, FontId::proportional(12.0), Color32::GRAY);
                }
                _ => {
                    painter.rect_filled(img_area.shrink(ts * 0.3), 4.0, Color32::from_gray(238));
                }
            }
            if let Some(m) = e.media {
                let badge = Rect::from_min_size(img_area.left_bottom() - vec2(0.0, 26.0), vec2(30.0, 24.0));
                let playing = self.player.as_ref().is_some_and(|p| p.path == e.path && p.is_playing());
                let bg = if playing { Color32::from_rgb(40, 120, 220) } else { Color32::from_black_alpha(170) };
                painter.rect_filled(badge, 4.0f32, bg);
                let mark = if m == loader::Media::Video { "▶" } else { "♪" };
                painter.text(badge.center(), Align2::CENTER_CENTER, mark, FontId::proportional(15.0), Color32::WHITE);
            }
            if archive::is_archive(&e.path) {
                let badge = Rect::from_min_size(img_area.left_bottom() - vec2(0.0, 22.0), vec2(34.0, 20.0));
                painter.rect_filled(badge, 3.0, Color32::from_black_alpha(170));
                painter.text(badge.center(), Align2::CENTER_CENTER, archive::badge(&e.path), FontId::proportional(11.0), Color32::WHITE);
            }
        }

        let e = &self.panes[pi].entries[i];
        let mut job = LayoutJob::single_section(
            e.name.clone(),
            TextFormat { font_id: FontId::proportional(13.0), color: Color32::from_gray(30), ..Default::default() },
        );
        job.wrap.max_width = rect.width() - 8.0;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
        job.halign = Align::Center;
        let galley = ui.fonts(|f| f.layout_job(job));
        ui.painter().galley(pos2(rect.center().x, rect.max.y - 22.0), galley, Color32::BLACK);

        let path = e.path.clone();
        let is_img = e.is_img();
        if resp.double_clicked() {
            // opening a folder replaces `entries`; doing it mid-grid made the
            // remaining cells index past the new, shorter list
            self.panes[pi].pending_open = Some(i);
        } else if resp.clicked() {
            let mods = ui.input(|inp| inp.modifiers);
            self.click_entry(pi, i, mods);
        } else if resp.secondary_clicked() && !selected {
            self.select_single(pi, i);
        }
        if self.panes[pi].entries[i].is_dir {
            self.drop_target(ui, &resp, &path);
        }
        if self.cut_paths.contains(&path) {
            ui.painter().rect_filled(rect, 4.0f32, Color32::from_white_alpha(150));
        }
        let targets = if selected { self.panes[pi].selected_paths() } else { vec![path.clone()] };
        let is_folder = self.panes[pi].entries[i].is_dir;
        let ic = self.icons.clone();
        let dir = self.panes[pi].cur_dir.clone();
        if archive::is_archive(&dir) && dir.is_file() {
            resp.context_menu(|ui| {
                if ui.button("크게보기").clicked() { self.actions.push(Action::Open(pi, i)); ui.close_menu(); }
                if ui.button("복사(C)").clicked() { self.actions.push(Action::Copy(targets.clone())); ui.close_menu(); }
                ui.menu_button("복사할 곳", |ui| self.dest_menu(ui, &targets, false));
                if ui.button("경로 복사").clicked() { ui.ctx().copy_text(path.display().to_string()); ui.close_menu(); }
            });
            return;
        }
        if self.native_menu(&resp, &targets, &dir, false, is_img.then_some((pi, i))) {
            resp.context_menu(|ui| {
                icons::style_menu(ui, 240.0);
                let open = egui::RichText::new(if is_img { "크게보기" } else { "열기" }).strong();
                let open_icon = if is_img { Some(&ic.view) } else if is_folder { Some(&ic.folder) } else { None };
                if icons::menu_item(ui, open_icon, open, "Enter", true).clicked() {
                    self.actions.push(Action::Open(pi, i));
                    ui.close_menu();
                }
                if is_img {
                    if icons::menu_item(ui, Some(&ic.rotate_right), "시계 방향으로 회전", "R", true).clicked() {
                        self.actions.push(Action::Rotate(targets.clone(), 1));
                        ui.close_menu();
                    }
                    if icons::menu_item(ui, Some(&ic.rotate_left), "시계 반대 방향으로 회전", "L", true).clicked() {
                        self.actions.push(Action::Rotate(targets.clone(), -1));
                        ui.close_menu();
                    }
                }
                icons::menu_sep(ui);
                let dir = self.panes[pi].cur_dir.clone();
                self.file_menu(ui, targets.clone(), dir);
                icons::menu_sep(ui);
                ui.menu_button("      복사할 곳", |ui| self.dest_menu(ui, &targets, false));
                ui.menu_button("      이동할 곳", |ui| self.dest_menu(ui, &targets, true));
                icons::menu_sep(ui);
                if icons::menu_item(ui, Some(&ic.explorer), "탐색기에서 보기", "", true).clicked() {
                    self.actions.push(Action::ShowInExplorer(path.clone()));
                    ui.close_menu();
                }
                if icons::menu_item(ui, None, "경로 복사", "", true).clicked() {
                    ui.ctx().copy_text(path.display().to_string());
                    ui.close_menu();
                }
            });
        }
    }

    fn preview(&mut self, ui: &mut Ui) {
        ui.label(egui::RichText::new("미리보기").strong());
        ui.separator();
        let Some(i) = self.panes[self.active].focus else {
            ui.weak("미리 볼 파일을 고르세요.");
            return;
        };
        let Some(e) = self.panes[self.active].entries.get(i) else { return };
        if e.is_dir {
            ui.label(&e.name);
            ui.weak("폴더");
            return;
        }
        if let Some(m) = e.media {
            let reserve = if m == loader::Media::Audio { 190.0 } else { 110.0 };
            let area_h = (ui.available_height() - reserve).max(60.0);
            let (area, resp) = ui.allocate_exact_size(vec2(ui.available_width(), area_h), Sense::click());
            if m == loader::Media::Video {
                // the focus settled on a video: play its preview frames
                let el = self.panes[self.active].focus_time.elapsed();
                if el >= PREVIEW_DELAY {
                    self.request_clip(self.active, i);
                } else {
                    ui.ctx().request_repaint_after(PREVIEW_DELAY - el);
                }
            }
            let e = &self.panes[self.active].entries[i];
            if let Some((t, k)) = self.clip_frame(&e.path) {
                draw_image(ui, &t, t.size_vec2(), area, 0, None, Vec2::ZERO);
                clip_progress(ui, area, t.size_vec2(), k);
            } else if let Thumb::Ready(t) = &self.panes[self.active].thumbs[i] {
                draw_image(ui, t, t.size_vec2(), area, 0, None, Vec2::ZERO);
            }
            ui.add_space(6.0);
            ui.label(egui::RichText::new(&e.name).strong());
            ui.label(if m == loader::Media::Video { "동영상" } else { "음성" });
            ui.label(fmt_size(e.size));
            ui.label(fmt_time(e.modified));
            if m == loader::Media::Audio {
                ui.add_space(6.0);
                let path = e.path.clone();
                self.audio_controls(ui, &path);
            } else {
                ui.weak("더블 클릭하면 기본 프로그램으로 엽니다");
            }
            if resp.double_clicked() {
                self.actions.push(Action::Open(self.active, i));
            }
            return;
        }
        let path = e.path.clone();
        let is_img = e.is_img();
        let area_h = (ui.available_height() * 0.5).max(60.0);
        let (area, resp) = ui.allocate_exact_size(vec2(ui.available_width(), area_h), Sense::click());
        match (self.big.get(&e.path), &self.panes[self.active].thumbs[i]) {
            (Some(Big::Ready(t, d)), _) => {
                let rot = self.disp_rot(&e.path, t);
                let t = self.anim_frame(&e.path).unwrap_or_else(|| t.clone());
                draw_image(ui, &t, vec2(d[0] as f32, d[1] as f32), area, rot, None, Vec2::ZERO);
            }
            (Some(Big::Failed), _) => {
                ui.painter().text(area.center(), Align2::CENTER_CENTER, "열 수 없는 그림", FontId::proportional(14.0), Color32::GRAY);
            }
            (_, Thumb::Ready(t)) => {
                // thumbnail stands in until the full image arrives
                let rot = self.disp_rot(&e.path, t);
                let size = self.panes[self.active].focus_dims.map(|d| vec2(d[0] as f32, d[1] as f32)).filter(|_| rot == 0).unwrap_or(t.size_vec2());
                draw_image(ui, t, fit_aspect(size, t.size_vec2()), area, rot, None, Vec2::ZERO);
            }
            _ => {}
        }
        let open = resp.double_clicked();
        let (name, size, modified) = (e.name.clone(), e.size, e.modified);
        let key = (is_img && !model::is_model(&path)).then(|| self.request_exif(&path));
        ui.add_space(6.0);
        egui::ScrollArea::vertical().id_salt(("preview_metadata", &path))
            .auto_shrink([false, false]).max_height(ui.available_height()).show(ui, |ui| {
                ui.label(egui::RichText::new(name).strong());
                if let Some([w, h]) = self.panes[self.active].focus_dims { ui.label(format!("{w} x {h}")); }
                ui.label(fmt_size(size));
                ui.label(fmt_time(modified));
                if let Some(key) = key {
                    ui.separator();
                    ui.label(egui::RichText::new("촬영 정보").strong());
                    match self.meta_cache.get(&key) {
                        Some(MetaState::Ready(Some(s))) => {
                            let camera = [s.make.as_deref(), s.model.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" ");
                            if !camera.is_empty() { ui.label(format!("카메라 {camera}")); }
                            if let Some(lens) = &s.lens { ui.label(format!("렌즈 {lens}")); }
                            if let Some(taken) = &s.taken { ui.label(format!("찍은 때 {taken}")); }
                            let settings = [&s.f_number, &s.exposure, &s.iso, &s.focal].into_iter().filter_map(|s| s.as_deref()).collect::<Vec<_>>().join(" · ");
                            if !settings.is_empty() { ui.label(format!("설정 {settings}")); }
                            if let Some((lat, lon)) = s.gps {
                                ui.colored_label(Color32::from_rgb(200, 40, 40), format!("⚠ 위치 {lat:.5}, {lon:.5}"));
                            }
                        }
                        Some(MetaState::Ready(None)) => { ui.weak("촬영 정보 없음"); }
                        _ => { ui.weak("촬영 정보 읽는 중…"); }
                    }
                    if ui.add_enabled(meta::can_strip(&path) && !self.privacy_saving.contains(&path), egui::Button::new("개인 정보 지우기")).clicked() {
                        self.actions.push(Action::StripPrivate(vec![path.clone()]));
                    }
                    if let Some(MetaState::Ready(Some(s))) = self.meta_cache.get(&key) {
                        egui::CollapsingHeader::new("자세히").id_salt(&path).show(ui, |ui| {
                            egui::Grid::new(("exif_tags", &path)).num_columns(2).striped(true).show(ui, |ui| {
                                for (name, value) in &s.all {
                                    ui.add(egui::Label::new(name).wrap());
                                    ui.add(egui::Label::new(value).wrap());
                                    ui.end_row();
                                }
                            });
                        });
                    }
                }
            });
        if open {
            self.open_entry(self.active, i);
        }
    }

    fn pane_header(&mut self, ui: &mut Ui, pi: usize, rect: Rect) {
        let resp = ui.interact(rect, ui.id().with("header"), Sense::click_and_drag());
        if resp.clicked() || resp.secondary_clicked() || resp.drag_started() { self.activate(pi); }
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(self.panes[pi].cur_dir.parent().is_some(), egui::Button::new("▲")).clicked() {
                    self.activate(pi);
                    self.go_up();
                }
                if ui.add_enabled(!self.panes[pi].back.is_empty(), egui::Button::new("◀")).clicked() {
                    self.activate(pi);
                    self.go_back();
                }
                let dir = &self.panes[pi].cur_dir;
                let name = dir.file_name().map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string());
                ui.add(egui::Label::new(name).truncate()).on_hover_text(dir.display().to_string());
            });
        });
    }

    fn pane_frame(&mut self, ui: &mut Ui, pi: usize, rect: Rect) {
        let resp = ui.interact(rect, ui.id().with(("pane_drop", pi)), Sense::click_and_drag());
        if resp.clicked() || resp.secondary_clicked() || resp.drag_started() { self.activate(pi); }
        let drag = egui::DragAndDrop::payload::<DragFiles>(ui.ctx());
        let over = ui.rect_contains_pointer(rect);
        self.dnd_folder_hover = false;
        ui.scope_builder(egui::UiBuilder::new().id_salt(("pane", pi)).max_rect(rect), |ui| {
            ui.set_clip_rect(rect.intersect(ui.clip_rect()));
            let header = Rect::from_min_size(rect.min + vec2(2.0, 2.0), vec2(rect.width() - 4.0, 24.0));
            self.pane_header(ui, pi, header);
            let body = Rect::from_min_max(pos2(rect.left() + 4.0, header.bottom()), rect.max - vec2(4.0, 4.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
                ui.set_clip_rect(body.intersect(ui.clip_rect()));
                self.grid(ui, pi);
            });
        });
        let mut dropping = false;
        if over && archive::is_location(&self.panes[pi].cur_dir) && drag.is_some() {
            self.status = "압축 파일 안에서는 할 수 없습니다".into();
        }
        if over && !self.dnd_folder_hover && !archive::is_location(&self.panes[pi].cur_dir) {
            if let Some(drag) = drag {
                if let Some(dest) = drop_dest(&self.panes[pi].cur_dir, &drag.0) {
                    dropping = true;
                    self.dnd_target = Some(dest.clone());
                    // Cells resolve first so a folder always wins over the pane.
                    if ui.input(|i| i.pointer.any_released())
                        && egui::DragAndDrop::take_payload::<DragFiles>(ui.ctx()).is_some() {
                        let mv = self.drop_is_move(&drag.0, &dest);
                        self.actions.push(Action::Transfer(drag.0.clone(), dest, mv));
                    }
                }
            }
        }
        let stroke = if dropping || pi == self.active { Stroke::new(2.0f32, Color32::from_rgb(40, 120, 220)) }
            else { Stroke::new(1.0f32, Color32::from_gray(215)) };
        ui.painter().rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
    }

    fn browser_ui(&mut self, ctx: &egui::Context) {
        self.grid_keys(ctx);

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(2.0);
            self.address_bar(ui);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        egui::SidePanel::left("tree").resizable(true).default_width(250.0).show(ctx, |ui| {
            self.quick_access(ui);
            ui.separator();
            egui::ScrollArea::both().auto_shrink(false).id_salt("tree_scroll").show(ui, |ui| self.tree_ui(ui));
        });
        egui::SidePanel::right("preview").resizable(true).default_width(320.0).show(ctx, |ui| self.preview(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(Color32::from_gray(246)).inner_margin(8.0))
            .show(ctx, |ui| {
                if self.panes.len() == 1 {
                    self.grid(ui, 0);
                } else {
                    for (pi, rect) in pane_rects(ui.max_rect(), self.split).into_iter().enumerate() {
                        self.pane_frame(ui, pi, rect);
                    }
                }
            });
        if let Some(i) = self.panes[self.active].pending_open.take() {
            self.open_entry(self.active, i);
        }

        // load the full picture for the preview once the focus settles
        if let Some(p) = self.panes[self.active].focus.and_then(|i| self.panes[self.active].entries.get(i)).filter(|e| e.is_img()).map(|e| e.path.clone()) {
            if !self.big.contains_key(&p) {
                let el = self.panes[self.active].focus_time.elapsed();
                if el >= PREVIEW_DELAY {
                    self.request_big(&p);
                } else {
                    ctx.request_repaint_after(PREVIEW_DELAY - el);
                }
            }
        }
    }

    // ------------------------------------------------------------ UI: viewer

    fn resume_viewer(&mut self, pi: usize, idx: usize) {
        let n = self.panes[pi].entries.len();
        let next = (idx.min(n.saturating_sub(1))..n)
            .chain((0..idx.min(n)).rev())
            .find(|&i| self.panes[pi].entries[i].is_img());
        match next {
            Some(i) => {
                self.viewer = Some(Viewer { pane: pi, idx: i, zoom: None, pan: Vec2::ZERO });
                self.select_single(pi, i);
            }
            None => self.close_viewer(),
        }
    }

    fn neighbor_image(&self, pi: usize, from: usize, dir: isize) -> Option<usize> {
        let n = self.panes[pi].entries.len() as isize;
        let mut i = from as isize + dir;
        while i >= 0 && i < n {
            if self.panes[pi].entries[i as usize].is_img() {
                return Some(i as usize);
            }
            i += dir;
        }
        None
    }

    fn close_viewer(&mut self) {
        self.viewer_menu_pending = None;
        self.ctx.memory_mut(|m| {
            if m.is_popup_open(Id::new("viewer_menu")) { m.close_popup(); }
        });
        self.viewer_last_motion = None;
        self.ctx.set_cursor_icon(egui::CursorIcon::Default);
        if let Some(v) = self.viewer.take() {
            if self.panes.get(v.pane).is_some_and(|p| v.idx < p.entries.len()) {
                self.select_single(v.pane, v.idx);
            }
        }
        if self.fullscreen {
            self.toggle_fullscreen();
        }
    }

    fn viewer_ui(&mut self, ctx: &egui::Context) {
        let Some((pi, idx)) = self.viewer.as_ref().map(|v| (v.pane, v.idx)) else { return };
        if pi >= self.panes.len() || idx >= self.panes[pi].entries.len() {
            self.close_viewer();
            return;
        }

        let menu_id = Id::new("viewer_menu");
        let menu_was_open = ctx.memory(|m| m.is_popup_open(menu_id));
        let open_menu_at = self.viewer_menu_pending.take();
        // keys
        if !self.modal_open() && !menu_was_open && open_menu_at.is_none() {
            let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));
            let mods = ctx.input(|i| i.modifiers);
            let (copy, cut) = ctx.input(|i| (
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                i.events.iter().any(|e| matches!(e, egui::Event::Cut)),
            ));
            let current = &self.panes[pi].entries[idx].path;
            if copy { self.actions.push(Action::Copy(vec![current.clone()])); }
            if cut { self.actions.push(Action::Cut(vec![current.clone()])); }
            let mut go: Option<usize> = None;
            if pressed(Key::Escape) || pressed(Key::Enter) {
                self.close_viewer();
                return;
            }
            if pressed(Key::ArrowRight) || pressed(Key::ArrowDown) || pressed(Key::PageDown) || pressed(Key::Space) {
                go = self.neighbor_image(pi, idx, 1);
            }
            if pressed(Key::ArrowLeft) || pressed(Key::ArrowUp) || pressed(Key::PageUp) || pressed(Key::Backspace) {
                go = self.neighbor_image(pi, idx, -1);
            }
            if pressed(Key::Home) {
                go = (0..self.panes[pi].entries.len()).find(|&i| self.panes[pi].entries[i].is_img());
            }
            if pressed(Key::End) {
                go = (0..self.panes[pi].entries.len()).rev().find(|&i| self.panes[pi].entries[i].is_img());
            }
            if pressed(Key::R) {
                self.rotate_current(if mods.shift { -1 } else { 1 });
            }
            if pressed(Key::L) {
                self.rotate_current(-1);
            }
            if pressed(Key::F11) || pressed(Key::F) {
                self.toggle_fullscreen();
            }
            if pressed(Key::Delete) {
                let p = self.panes[pi].entries[idx].path.clone();
                self.ask_delete(vec![p]);
            }
            if pressed(Key::Num0) || pressed(Key::Num8) {
                if let Some(v) = &mut self.viewer {
                    v.zoom = None;
                    v.pan = Vec2::ZERO;
                }
            }
            if pressed(Key::Num1) {
                if let Some(v) = &mut self.viewer {
                    v.zoom = Some(1.0);
                    v.pan = Vec2::ZERO;
                }
            }
            if let Some(g) = go {
                self.viewer = Some(Viewer { pane: pi, idx: g, zoom: None, pan: Vec2::ZERO });
                self.select_single(pi, g);
            }
        }
        let Some(idx) = self.viewer.as_ref().map(|v| v.idx) else { return };

        // current + neighbours
        let cur = self.panes[pi].entries[idx].path.clone();
        for d in [1isize, -1] {
            if let Some(j) = self.neighbor_image(pi, idx, d) {
                let p = self.panes[pi].entries[j].path.clone();
                self.request_big(&p);
            }
        }
        self.request_big(&cur);

        let total = self.panes[pi].entries.iter().filter(|e| e.is_img()).count();
        let pos = self.panes[pi].entries[..=idx].iter().filter(|e| e.is_img()).count();
        let name = self.panes[pi].entries[idx].name.clone();

        // Open once after the image has loaded for capture checks.
        static MENU_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let capture_menu = std::env::var("PICVIEW_VIEWER_MENU").is_ok_and(|v| v == "1")
            && matches!(self.big.get(&cur), Some(Big::Ready(..) | Big::Failed))
            && !MENU_SHOWN.swap(true, Ordering::Relaxed);
        let now = Instant::now();
        let last_motion = self.viewer_last_motion.get_or_insert(now);
        if menu_was_open || open_menu_at.is_some() || capture_menu
            || ctx.input(|i| i.pointer.delta() != Vec2::ZERO || i.pointer.secondary_clicked()) {
            *last_motion = now;
        }
        let idle = now.saturating_duration_since(*last_motion);
        let alpha = overlay_alpha(idle);
        if idle < Duration::from_millis(1500) {
            ctx.request_repaint_after(Duration::from_millis(1500) - idle);
        } else if alpha > 0.0 {
            ctx.request_repaint_after(Duration::from_millis(16).min(Duration::from_millis(1800) - idle));
        }
        if self.fullscreen && idle < Duration::from_secs(2) {
            ctx.request_repaint_after(Duration::from_secs(2) - idle);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(if self.fullscreen { Color32::BLACK } else { Color32::from_gray(18) }))
            .show(ctx, |ui| {
                let area = ui.max_rect();
                let resp = ui.interact(area, Id::new("viewer_area"), Sense::click_and_drag());
                if resp.secondary_clicked() && !self.modal_open() {
                    self.viewer_menu_pending = resp.interact_pointer_pos();
                    ctx.request_repaint();
                }
                if capture_menu {
                    self.viewer_menu_pending = Some(area.center());
                    ctx.request_repaint();
                }
                let (zoom_delta, scroll_y, hover) =
                    ctx.input(|i| (i.zoom_delta(), i.raw_scroll_delta.y, i.pointer.hover_pos()));
                let can_interact = !self.modal_open() && !menu_was_open
                    && open_menu_at.is_none() && self.viewer_menu_pending.is_none();
                let zoom_delta = if can_interact { zoom_delta } else { 1.0 };
                let scroll_y = if can_interact { scroll_y } else { 0.0 };
                let mut info = String::new();
                let mut nav: Option<isize> = None;

                match self.big.get(&cur) {
                    Some(Big::Ready(t, d)) => {
                        let rot = self.rot_of(&cur) + self.baked.get(&t.id()).copied().unwrap_or(0);
                        let img = vec2(d[0] as f32, d[1] as f32);
                        let rimg = if rot.rem_euclid(2) == 1 { vec2(img.y, img.x) } else { img };
                        let v = self.viewer.as_mut().unwrap();
                        let fit = fit_scale(rimg, area.size());
                        let mut scale = v.zoom.unwrap_or(fit);
                        if zoom_delta != 1.0 {
                            let new = (scale * zoom_delta).clamp(0.02, 40.0);
                            if let Some(h) = hover {
                                let c = area.center() + v.pan;
                                let off = h - c;
                                v.pan += off - off * (new / scale);
                            }
                            scale = new;
                            v.zoom = Some(new);
                        } else if scroll_y != 0.0 {
                            nav = Some(if scroll_y < 0.0 { 1 } else { -1 });
                        }
                        let kz = ctx.input(|i| (i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals), i.key_pressed(Key::Minus)));
                        if can_interact && (kz.0 || kz.1) {
                            scale = (scale * if kz.0 { 1.25 } else { 0.8 }).clamp(0.02, 40.0);
                            v.zoom = Some(scale);
                        }
                        if can_interact && resp.dragged_by(egui::PointerButton::Primary) {
                            v.pan += resp.drag_delta();
                        }
                        let pan = v.pan;
                        let frame = self.anim_frame(&cur).unwrap_or_else(|| t.clone());
                        draw_image(ui, &frame, img, area, rot, Some(scale), pan);
                        info = format!("{name}   ({pos}/{total})   {} x {}   {:.0}%", d[0], d[1], scale * 100.0);
                    }
                    Some(Big::Failed) => {
                        ui.painter().text(area.center(), Align2::CENTER_CENTER, "열 수 없는 그림", FontId::proportional(20.0), Color32::LIGHT_GRAY);
                        if scroll_y != 0.0 {
                            nav = Some(if scroll_y < 0.0 { 1 } else { -1 });
                        }
                    }
                    _ => {
                        if let Thumb::Ready(t) = &self.panes[pi].thumbs[idx] {
                            let rot = self.disp_rot(&cur, t);
                            let s = t.size_vec2();
                            let big = s * (area.width() / s.x).min(area.height() / s.y) * 0.9;
                            draw_image(ui, t, big, area, rot, None, Vec2::ZERO);
                        }
                        info = format!("{name}   ({pos}/{total})   불러오는 중…");
                    }
                }
                if !info.is_empty() && alpha > 0.0 {
                    let p = ui.painter();
                    let r = p.text(area.min + vec2(12.0, 10.0), Align2::LEFT_TOP, &info, FontId::proportional(14.0), Color32::TRANSPARENT);
                    p.rect_filled(r.expand(6.0), 4.0, Color32::from_black_alpha((150.0 * alpha) as u8));
                    p.text(area.min + vec2(12.0, 10.0), Align2::LEFT_TOP, &info, FontId::proportional(14.0), Color32::from_white_alpha((255.0 * alpha) as u8));
                }
                if can_interact && resp.double_clicked() {
                    self.close_viewer();
                    return;
                }
                if let Some(d) = nav {
                    if let Some(g) = self.neighbor_image(pi, idx, d) {
                        self.viewer = Some(Viewer { pane: pi, idx: g, zoom: None, pan: Vec2::ZERO });
                        self.select_single(pi, g);
                    }
                }
                if let Some(pos) = open_menu_at {
                    let fallback = archive_read_only(&cur)
                        || self.show_native_menu(std::slice::from_ref(&cur),
                            &self.panes[pi].cur_dir.clone(), false, None, Some(pos));
                    self.viewer_last_motion = Some(Instant::now());
                    ctx.set_cursor_icon(egui::CursorIcon::Default);
                    ctx.request_repaint();
                    if fallback {
                        ctx.data_mut(|d| d.insert_temp(menu_id.with("position"), pos));
                        ctx.memory_mut(|m| m.open_popup(menu_id));
                    }
                }
                let mut anchor = resp.clone();
                let pos = ctx.data(|d| d.get_temp::<egui::Pos2>(menu_id.with("position")))
                    .unwrap_or(area.center());
                anchor.rect = Rect::from_min_size(pos, Vec2::ZERO);
                let mut action = None;
                egui::popup_below_widget(ui, menu_id, &anchor, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                    icons::style_menu(ui, 240.0);
                    if icons::menu_item(ui, None, format!("{} (F11)", fullscreen_menu_label(self.fullscreen)), "", true).clicked() {
                        action = Some(Action::ToggleFullscreen);
                    }
                    if icons::menu_item(ui, None, "경로 복사", "", true).clicked() {
                        ctx.copy_text(cur.display().to_string());
                    }
                    if icons::menu_item(ui, None, "복사(C)", "Ctrl+C", true).clicked() {
                        action = Some(Action::Copy(vec![cur.clone()]));
                    }
                    ui.menu_button("복사할 곳", |ui| self.dest_menu(ui, std::slice::from_ref(&cur), false));
                    if icons::menu_item(ui, None, "개인 정보 지우기", "", meta::can_strip(&cur)).clicked() {
                        action = Some(Action::StripPrivate(vec![cur.clone()]));
                    }
                    if icons::menu_item(ui, None, "목록으로 돌아가기 (Esc)", "", true).clicked() {
                        action = Some(Action::CloseViewer);
                    }
                });
                if let Some(action) = action { self.actions.push(action); }
            });
        if self.viewer.is_some() && alpha > 0.0 {
            egui::Area::new(Id::new("viewer_fullscreen_button"))
                .order(egui::Order::Foreground)
                .anchor(Align2::RIGHT_TOP, vec2(-12.0, 12.0))
                .movable(false)
                .show(ctx, |ui| {
                    let (rect, response) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
                    ui.painter().rect_filled(rect, 6.0, Color32::from_black_alpha((170.0 * alpha) as u8));
                    icons::fullscreen_icon(ui.painter(), Rect::from_center_size(rect.center(), vec2(20.0, 20.0)),
                        self.fullscreen, Color32::from_white_alpha((255.0 * alpha) as u8));
                    let hint = if self.fullscreen { "창으로 보기 (F11, Esc)" } else { "전체 화면 (F11)" };
                    if response.on_hover_text(hint).clicked() && !self.modal_open() {
                        self.toggle_fullscreen();
                    }
                });
        }
        let idle = self.viewer_last_motion.map(|t| t.elapsed()).unwrap_or_default();
        if self.viewer.is_some() && hide_cursor(self.fullscreen, idle) && !self.modal_open()
            && !ctx.memory(|m| m.is_popup_open(menu_id)) && self.viewer_menu_pending.is_none() {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        } else {
            ctx.set_cursor_icon(egui::CursorIcon::Default);
        }
    }

    // ------------------------------------------------------------ settings

    fn measure_cache(&self) {
        let (usage, ctx) = (self.cache_usage.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            usage.store(cache::usage(), Ordering::Relaxed);
            ctx.request_repaint();
        });
    }

    fn open_settings(&mut self) {
        self.show_settings = true;
        self.cache_usage.store(u64::MAX, Ordering::Relaxed);
        self.measure_cache();
    }

    /// Settings window. Layout and colours from a GPT (Codex) proposal; the
    /// style changes stay inside this window.
    fn settings_window(&mut self, ctx: &egui::Context) {
        use egui::{Color32 as C, CornerRadius, Frame, RichText, Stroke, vec2};
        use std::sync::atomic::Ordering;
        if !self.show_settings { return; }
        let text = C::from_rgb(36, 48, 60);
        let muted = C::from_rgb(111, 124, 140);
        let line = C::from_rgb(230, 234, 240);
        let hover = C::from_rgb(229, 241, 255);
        let blue = C::from_rgb(57, 127, 197);
        let frame = Frame::new().fill(C::WHITE)
            .stroke(Stroke::new(1.0f32, C::from_rgb(213, 218, 225)))
            .corner_radius(CornerRadius::same(8)).inner_margin(16)
            .shadow(egui::epaint::Shadow {
                offset: [0, 4], blur: 12, spread: 0, color: C::from_black_alpha(32),
            });
        let mut open = true;
        // title "설정" with the version in small grey type right after it
        let mut title = LayoutJob::default();
        title.append("설정", 0.0, TextFormat {
            font_id: egui::FontId::proportional(18.0), color: text, valign: Align::BOTTOM, ..Default::default()
        });
        title.append(concat!("v", env!("CARGO_PKG_VERSION")), 6.0, TextFormat {
            font_id: egui::FontId::proportional(12.0), color: muted, valign: Align::BOTTOM, ..Default::default()
        });
        let shown = egui::Window::new(title)
            .open(&mut open).collapsible(false).resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .default_width(420.0).frame(frame).show(ctx, |ui| {
                ui.set_width(420.0);
                for t in [egui::TextStyle::Body, egui::TextStyle::Button, egui::TextStyle::Monospace] {
                    ui.style_mut().text_styles.insert(t, egui::FontId::proportional(13.0));
                }
                let sp = ui.spacing_mut();
                sp.item_spacing = vec2(8.0, 8.0);
                sp.button_padding = vec2(10.0, 5.0);
                sp.interact_size.y = 30.0;
                sp.slider_width = 288.0;
                let v = ui.visuals_mut();
                v.override_text_color = Some(text);
                v.extreme_bg_color = line;
                v.selection.bg_fill = hover;
                v.selection.stroke = Stroke::new(1.0f32, blue);
                v.widgets.noninteractive.bg_stroke = Stroke::new(1.0f32, line);
                for w in [&mut v.widgets.inactive, &mut v.widgets.hovered,
                          &mut v.widgets.active, &mut v.widgets.open] {
                    w.corner_radius = CornerRadius::same(4);
                    w.bg_stroke = Stroke::new(1.0f32, line);
                    w.fg_stroke = Stroke::new(1.0f32, text);
                    w.expansion = 0.0;
                }
                v.widgets.inactive.weak_bg_fill = C::WHITE;
                v.widgets.hovered.weak_bg_fill = hover;
                v.widgets.active.weak_bg_fill = C::from_rgb(212, 231, 252);
                v.widgets.open.weak_bg_fill = hover;
                // short windows: scroll the sections instead of cutting them off
                let max_h = (ctx.screen_rect().height() - 120.0).max(200.0);
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid(); // a bar you can see
                egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                Frame::new().fill(C::from_rgb(248, 250, 252))
                    .stroke(Stroke::new(1.0f32, line)).corner_radius(CornerRadius::same(6))
                    .inner_margin(14).show(ui, |ui| {
                        ui.set_width(390.0);
                        ui.horizontal(|ui| {
                            ui.image((self.icons.settings.id(), vec2(16.0, 16.0)));
                            ui.label(RichText::new("썸네일 캐시").size(15.0).strong());
                        });
                        ui.add(egui::Label::new(RichText::new(
                            "목록의 작은 그림(썸네일)은 원본을 열어 줄여야 해서 시간이 듭니다. \
                             그래서 한 번 만든 것을 디스크에 저장해 두고, 폴더를 다시 열 때 읽어 씁니다. \
                             그림 127장 폴더에서 새로 만들면 0.20초, 저장한 것을 읽으면 0.02초였고, \
                             큰 사진이 많을수록 차이가 커집니다. 100장에 2MB쯤 들며, \
                             비워도 원본은 그대로이고 다음에 볼 때 다시 만듭니다."
                        ).size(12.0).color(muted)).wrap());
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("최대 크기");
                            ui.spacing_mut().slider_width = 220.0;
                            ui.add(egui::Slider::new(&mut self.cache_max_mb,
                                CACHE_MIN_MB..=CACHE_MAX_MB).show_value(false));
                            ui.scope(|ui| {
                                ui.spacing_mut().interact_size.x = 94.0;
                                ui.add(egui::DragValue::new(&mut self.cache_max_mb)
                                    .range(CACHE_MIN_MB..=CACHE_MAX_MB).speed(5).suffix(" MB"))
                                    .on_hover_text("클릭하여 직접 입력하거나 좌우로 끌어 바꿉니다");
                            });
                        });
                        ui.horizontal(|ui| {
                            for (label, mb) in [("100MB", 100), ("300MB", 300), ("500MB", 500), ("1GB", 1024)] {
                                let b = egui::Button::new(label).selected(self.cache_max_mb == mb);
                                if ui.add_sized([76.0, 30.0], b).clicked() { self.cache_max_mb = mb; }
                            }
                        });
                        ui.add(egui::Label::new(RichText::new(
                            "한도를 넘으면 다음 실행 때 오래된 항목부터 80%로 줄입니다."
                        ).size(12.0).color(muted)).wrap());
                        let limit = self.cache_max_mb * 1024 * 1024;
                        let used = self.cache_usage.load(Ordering::Relaxed);
                        let busy = used == u64::MAX;
                        let ratio = if busy { 0.0 } else { used as f32 / limit as f32 };
                        let color = if !busy && used > limit { C::from_rgb(181, 71, 53) } else { blue };
                        ui.label(RichText::new(if busy { "사용량 확인 중…".into() } else {
                            format!("사용 중: {} / {}  ({:.0}%){}", fmt_size(used),
                                fmt_size(limit), ratio * 100.0, if used > limit { " · 한도 초과" } else { "" })
                        }).color(if busy { muted } else { color }));
                        ui.add(egui::ProgressBar::new(ratio).desired_width(390.0).desired_height(8.0)
                            .fill(if busy || used == 0 { C::TRANSPARENT } else { color }));
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!busy, egui::Button::new("지금 줄이기"))
                                .on_hover_text("한도를 넘었을 때만 80%로 줄입니다").clicked() {
                                let (usage, c) = (self.cache_usage.clone(), self.ctx.clone());
                                usage.store(u64::MAX, Ordering::Relaxed);
                                std::thread::spawn(move || {
                                    cache::trim(limit);
                                    usage.store(cache::usage(), Ordering::Relaxed);
                                    c.request_repaint();
                                });
                            }
                            let clear = egui::Button::new(RichText::new("모두 비우기")
                                .color(C::from_rgb(169, 50, 38))).fill(C::from_rgb(255, 242, 240))
                                .stroke(Stroke::new(1.0f32, C::from_rgb(233, 184, 177)));
                            if ui.add_enabled(!busy, clear).clicked() {
                                self.status = match cache::clear() {
                                    Ok(()) => "썸네일 캐시를 비웠습니다".into(),
                                    Err(e) => format!("캐시 삭제 실패: {e}"),
                                };
                                self.cache_usage.store(u64::MAX, Ordering::Relaxed);
                                self.measure_cache();
                            }
                            let image = egui::Image::new((self.icons.folder.id(), vec2(16.0, 16.0)));
                            if ui.add(egui::Button::image_and_text(image, "폴더 열기")).clicked() {
                                if let Some(d) = cache::dir() {
                                    let _ = std::process::Command::new("explorer").arg(d).spawn();
                                }
                            }
                        });
                        if let Some(d) = cache::dir() {
                            ui.add(egui::Label::new(RichText::new(d.display().to_string())
                                .size(12.0).color(muted)).wrap());
                        }
                    });
                // deleting files: ask first or not
                Frame::new().fill(C::from_rgb(248, 250, 252))
                    .stroke(Stroke::new(1.0f32, line)).corner_radius(CornerRadius::same(6))
                    .inner_margin(14).show(ui, |ui| {
                        ui.set_width(390.0);
                        ui.horizontal(|ui| {
                            ui.image((self.icons.delete.id(), vec2(16.0, 16.0)));
                            ui.label(RichText::new("파일 지우기").size(15.0).strong());
                        });
                        ui.separator();
                        ui.radio_value(&mut self.confirm_delete, true, "지우기 전에 물어보기");
                        ui.radio_value(&mut self.confirm_delete, false, "묻지 않고 바로 휴지통으로 보내기");
                        ui.add(egui::Label::new(RichText::new(
                            "어느 쪽이든 휴지통으로 가므로 되살릴 수 있습니다."
                        ).size(12.0).color(muted)).wrap());
                    });
                // video previews: on / off
                Frame::new().fill(C::from_rgb(248, 250, 252))
                    .stroke(Stroke::new(1.0f32, line)).corner_radius(CornerRadius::same(6))
                    .inner_margin(14).show(ui, |ui| {
                        ui.set_width(390.0);
                        ui.horizontal(|ui| {
                            ui.image((self.icons.view.id(), vec2(16.0, 16.0)));
                            ui.label(RichText::new("동영상 미리보기").size(15.0).strong());
                        });
                        ui.separator();
                        ui.checkbox(&mut self.video_preview, "동영상에서 10장을 뽑아 돌려 보여 주기");
                        ui.add(egui::Label::new(RichText::new(
                            "미리보기 칸, 그리고 마우스를 올린 목록 칸에서 돕니다. 끄면 썸네일 한 장만 보입니다."
                        ).size(12.0).color(muted)).wrap());
                    });
                }); // ScrollArea
            });
        // "About" sits in the title bar, left of the close button; the program
        // info itself opens in its own small window
        if let Some(w) = shown {
            let at = pos2(w.response.rect.right() - 128.0, w.response.rect.top() + 19.0);
            egui::Area::new(Id::new("settings_about")).order(egui::Order::Foreground).fixed_pos(at).show(ctx, |ui| {
                let image = egui::Image::new((self.icons.view.id(), vec2(14.0, 14.0)));
                let b = egui::Button::image_and_text(image, egui::RichText::new("About").size(12.5)).corner_radius(4.0);
                if ui.add(b).on_hover_text("프로그램 정보").clicked() {
                    self.show_about = true;
                }
            });
        }
        // Esc goes back to the main screen (the About window on top closes first)
        let esc = ctx.input(|i| i.key_pressed(Key::Escape));
        self.show_settings = open && !(esc && !self.show_about);
    }

    /// "About": name, version, author, GitHub. Same frame as the settings window.
    fn about_window(&mut self, ctx: &egui::Context) {
        use egui::{Color32 as C, CornerRadius, Frame, RichText, Stroke, vec2};
        if !self.show_about {
            return;
        }
        let text = C::from_rgb(36, 48, 60);
        let muted = C::from_rgb(111, 124, 140);
        let frame = Frame::new().fill(C::WHITE)
            .stroke(Stroke::new(1.0f32, C::from_rgb(213, 218, 225)))
            .corner_radius(CornerRadius::same(8)).inner_margin(16)
            .shadow(egui::epaint::Shadow { offset: [0, 4], blur: 12, spread: 0, color: C::from_black_alpha(32) });
        let mut open = true;
        egui::Window::new(RichText::new("About").size(18.0).color(text))
            .id(Id::new("about_window"))
            .open(&mut open).collapsible(false).resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(320.0);
                ui.vertical_centered(|ui| {
                    ui.image((self.icons.view.id(), vec2(48.0, 48.0)));
                    ui.label(RichText::new("Oddity PicView").size(20.0).strong().color(text));
                    ui.label(RichText::new(format!("버전 {}", env!("CARGO_PKG_VERSION"))).color(muted));
                });
                ui.add_space(8.0);
                ui.separator();
                egui::Grid::new("about_grid").num_columns(2).spacing(vec2(16.0, 8.0)).show(ui, |ui| {
                    ui.label(RichText::new("만든 사람").color(muted));
                    ui.label(AUTHOR);
                    ui.end_row();
                    ui.label(RichText::new("GitHub").color(muted));
                    ui.hyperlink_to(REPO_URL.trim_start_matches("https://"), REPO_URL)
                        .on_hover_text("브라우저로 엽니다");
                    ui.end_row();
                    ui.label(RichText::new("라이선스").color(muted));
                    ui.label("MIT");
                    ui.end_row();
                });
            });
        if !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.show_about = false;
        }
    }

    // ------------------------------------------------------------ dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.take() else { return };
        let strip = matches!(&dialog, Dialog::StripPrivate(_));
        let enter = ctx.input(|i| i.key_pressed(Key::Enter));
        let esc = ctx.input(|i| i.key_pressed(Key::Escape));
        match dialog {
            // layout and colours from a GPT (Codex) proposal, styled only inside this window
            Dialog::Delete(paths) | Dialog::StripPrivate(paths) => {
                use egui::{Color32 as C, CornerRadius, Frame, RichText, Stroke, vec2};
                let text = C::from_rgb(36, 48, 60);
                let muted = C::from_rgb(111, 124, 140);
                let line = C::from_rgb(230, 234, 240);
                let danger = C::from_rgb(169, 50, 38);
                let metadata: Vec<_> = paths.iter().map(std::fs::metadata).collect();
                let limited = paths.len() > 1000;
                let limit = if limited { 200 } else { paths.len() };
                let bytes = metadata.iter().take(limit).filter_map(|m| m.as_ref().ok())
                    .filter(|m| m.is_file()).fold(0u64, |n, m| n.saturating_add(m.len()));
                let folders = metadata.iter().any(|m| m.as_ref().is_ok_and(|m| m.is_dir()));
                let incomplete = metadata.iter().any(|m| m.is_err());
                let mut summary = if limited {
                    format!("파일 용량: 약 {} (앞 200개 기준)", fmt_size(bytes))
                } else {
                    format!("파일 용량: {}", fmt_size(bytes))
                };
                if folders { summary.push_str(" · 폴더 포함"); }
                if incomplete { summary.push_str(" · 일부 확인 불가"); }
                let frame = Frame::new().fill(C::WHITE)
                    .stroke(Stroke::new(1.0f32, C::from_rgb(213, 218, 225)))
                    .corner_radius(CornerRadius::same(8)).inner_margin(16)
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 4], blur: 12, spread: 0, color: C::from_black_alpha(32),
                    });
                let mut result: Option<bool> = None;
                egui::Window::new(RichText::new(if strip { "개인 정보 지우기" } else { "삭제" }).size(18.0).color(text))
                    .collapsible(false).resizable(false)
                    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                    .default_width(440.0).frame(frame).show(ctx, |ui| {
                        ui.set_width(440.0);
                        for t in [egui::TextStyle::Body, egui::TextStyle::Button] {
                            ui.style_mut().text_styles.insert(t, egui::FontId::proportional(13.0));
                        }
                        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                        ui.spacing_mut().button_padding = vec2(10.0, 5.0);
                        ui.spacing_mut().interact_size.y = 30.0;
                        let v = ui.visuals_mut();
                        v.override_text_color = Some(text);
                        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered,
                                  &mut v.widgets.active, &mut v.widgets.open] {
                            w.corner_radius = CornerRadius::same(4);
                            w.bg_stroke = Stroke::new(1.0f32, line);
                            w.fg_stroke = Stroke::new(1.0f32, text);
                            w.expansion = 0.0;
                        }
                        v.widgets.inactive.weak_bg_fill = C::WHITE;
                        v.widgets.hovered.weak_bg_fill = C::from_rgb(229, 241, 255);
                        v.widgets.active.weak_bg_fill = C::from_rgb(212, 231, 252);
                        v.widgets.open.weak_bg_fill = C::from_rgb(229, 241, 255);
                        ui.horizontal_top(|ui| {
                            ui.image((self.icons.delete.id(), vec2(40.0, 40.0)));
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                ui.set_width(388.0);
                                let prompt = if strip {
                                    format!("{}개 파일의 촬영 정보와 위치를 지웁니다. 되돌릴 수 없습니다.", paths.len())
                                } else if paths.len() == 1 {
                                    "이 항목을 휴지통으로 보낼까요?".to_owned()
                                } else {
                                    format!("{}개 항목을 휴지통으로 보낼까요?", paths.len())
                                };
                                ui.label(RichText::new(prompt).size(15.0).strong());
                                Frame::new().fill(C::from_rgb(248, 250, 252))
                                    .stroke(Stroke::new(1.0f32, line))
                                    .corner_radius(CornerRadius::same(6)).inner_margin(12)
                                    .show(ui, |ui| {
                                        ui.set_width(362.0);
                                        ui.spacing_mut().item_spacing.y = 4.0;
                                        for (i, p) in paths.iter().take(3).enumerate() {
                                            let name = p.file_name().unwrap_or(p.as_os_str())
                                                .to_string_lossy().replace(['\r', '\n'], " ");
                                            let dir = metadata[i].as_ref().is_ok_and(|m| m.is_dir());
                                            ui.horizontal(|ui| {
                                                let width = ui.available_width() - if dir { 48.0 } else { 0.0 };
                                                // left-aligned, cut with "…" when too long
                                                ui.allocate_ui(vec2(width, 18.0), |ui| {
                                                    ui.add(egui::Label::new(name).truncate());
                                                });
                                                if dir { ui.label(RichText::new("(폴더)").color(muted)); }
                                            });
                                        }
                                        if paths.len() > 3 {
                                            ui.label(RichText::new(format!("외 {}개", paths.len() - 3))
                                                .color(muted));
                                        }
                                    });
                                ui.label(RichText::new(&summary).size(12.0).color(muted));
                                ui.label(RichText::new(if strip { "원본 파일에 덮어씁니다" } else { "휴지통에서 되살릴 수 있습니다" })
                                    .size(12.0).color(muted));
                            });
                        });
                        ui.add_space(8.0);
                        // fixed 30px row: a bare right_to_left layout took the whole window height
                        ui.allocate_ui_with_layout(vec2(ui.available_width(), 30.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let send = egui::Button::new(RichText::new(if strip { "지우기" } else { "휴지통으로 보내기" }).color(danger))
                                .shortcut_text(RichText::new("Enter").size(12.0).color(danger))
                                .fill(C::from_rgb(255, 242, 240))
                                .stroke(Stroke::new(1.0f32, C::from_rgb(233, 184, 177)));
                            if ui.add_sized([220.0, 30.0], send).clicked() { result = Some(true); }
                            let cancel = egui::Button::new("취소")
                                .shortcut_text(RichText::new("Esc").size(12.0).color(muted));
                            if ui.add_sized([100.0, 30.0], cancel).clicked() { result = Some(false); }
                        });
                    });
                if enter { result = Some(true); }
                if esc { result = Some(false); }
                match result {
                    Some(true) if strip => self.do_strip_private(paths),
                    Some(true) => self.do_delete(paths),
                    Some(false) => {}
                    None if strip => self.dialog = Some(Dialog::StripPrivate(paths)),
                    None => self.dialog = Some(Dialog::Delete(paths)),
                }
            }
            Dialog::Rename { path, mut text, first } => {
                let mut result: Option<bool> = None;
                egui::Window::new("이름 바꾸기")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                    .show(ctx, |ui| {
                        let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(320.0));
                        if first {
                            r.request_focus();
                        }
                        if r.lost_focus() && enter {
                            result = Some(true);
                        }
                        ui.horizontal(|ui| {
                            if ui.button("확인").clicked() {
                                result = Some(true);
                            }
                            if ui.button("취소").clicked() {
                                result = Some(false);
                            }
                        });
                    });
                if esc {
                    result = Some(false);
                }
                match result {
                    Some(true) => self.do_rename(path, &text),
                    Some(false) => {}
                    None => self.dialog = Some(Dialog::Rename { path, text, first: false }),
                }
            }
            Dialog::NewFolder { parent, mut text, first } => {
                let mut result: Option<bool> = None;
                egui::Window::new("폴더 생성")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                    .show(ctx, |ui| {
                        ui.weak(parent.display().to_string());
                        let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(320.0));
                        if first {
                            r.request_focus();
                        }
                        if r.lost_focus() && enter {
                            result = Some(true);
                        }
                        ui.horizontal(|ui| {
                            if ui.button("만들기").clicked() {
                                result = Some(true);
                            }
                            if ui.button("취소").clicked() {
                                result = Some(false);
                            }
                        });
                    });
                if esc {
                    result = Some(false);
                }
                match result {
                    Some(true) => self.do_new_folder(parent, &text),
                    Some(false) => {}
                    None => self.dialog = Some(Dialog::NewFolder { parent, text, first: false }),
                }
            }
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let s = Settings {
            last_dir: Some(self.panes[0].cur_dir.clone()).filter(|d| !d.as_os_str().is_empty()),
            pane_dirs: self.panes.iter().map(|p| p.cur_dir.clone()).collect(),
            split: self.split,
            active_pane: self.active,
            recent: self.recent.clone(),
            sort: self.sort,
            sort_desc: self.sort_desc,
            thumb_size: self.thumb_size,
            favorites: self.favorites.clone(),
            transfer_dirs: self.transfer_dirs.clone(),
            cache_max_mb: self.cache_max_mb,
            confirm_delete: self.confirm_delete,
            video_preview: self.video_preview,
        };
        eframe::set_value(storage, eframe::APP_KEY, &s);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Apply the capture override before the first browser frame.
        static SPLIT_SET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SPLIT_SET.swap(true, Ordering::Relaxed) {
            let split = match std::env::var("PICVIEW_SPLIT").as_deref() {
                Ok("lr") => Some(Split::LeftRight),
                Ok("tb") => Some(Split::TopBottom),
                _ => None,
            };
            if let Some(split) = split {
                self.set_split(split);
            }
        }
        self.poll_msgs();
        self.refresh_changed_dirs();
        // Open the first image once for the viewer menu capture switch.
        static VIEWER_MENU_OPENED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var("PICVIEW_VIEWER_MENU").is_ok_and(|v| v == "1")
            && !VIEWER_MENU_OPENED.load(Ordering::Relaxed) {
            if let Some(i) = self.pane().entries.iter().position(|e| e.is_img()) {
                VIEWER_MENU_OPENED.store(true, Ordering::Relaxed);
                self.select_single(self.active, i);
                self.open_entry(self.active, i);
            }
        }
        // Open the first listed image once for capture checks.
        static FULLSCREEN_OPENED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var("PICVIEW_OPEN_FULLSCREEN").is_ok_and(|v| v == "1")
            && !FULLSCREEN_OPENED.load(Ordering::Relaxed) {
            if let Some(i) = self.pane().entries.iter().position(|e| e.is_img()) {
                FULLSCREEN_OPENED.store(true, Ordering::Relaxed);
                self.select_single(self.active, i);
                self.open_selected_fullscreen();
            }
        }
        // Ctrl+V: egui only reports it when the clipboard holds text
        let v = ctx.input(|i| i.focused) && fileops::ctrl_v_down();
        if v && !self.ctrl_v_prev && self.viewer.is_none() && !self.modal_open() && !ctx.wants_keyboard_input() {
            self.actions.push(Action::Paste(self.panes[self.active].cur_dir.clone()));
        }
        self.ctrl_v_prev = v;
        if self.viewer.is_some() {
            self.viewer_ui(ctx);
        } else {
            self.browser_ui(ctx);
        }
        // PICVIEW_SHOW_SETTINGS=1 opens the settings window once at start (for screenshots)
        static SETTINGS_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var_os("PICVIEW_SHOW_SETTINGS").is_some() && !SETTINGS_SHOWN.swap(true, Ordering::Relaxed) {
            self.open_settings();
            self.show_about = std::env::var_os("PICVIEW_SHOW_ABOUT").is_some();
        }
        // PICVIEW_FILTER=image|video|audio picks the kind filter once at start (for screenshots)
        static FILTER_SET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if let Some(v) = std::env::var_os("PICVIEW_FILTER") {
            if !FILTER_SET.swap(true, Ordering::Relaxed) {
                let f = match v.to_str() {
                    Some("image") => KindFilter::Image,
                    Some("video") => KindFilter::Video,
                    Some("audio") => KindFilter::Audio,
                    _ => KindFilter::All,
                };
                self.set_kind_filter(f);
            }
        }
        // PICVIEW_SELECT_VIDEO=1 focuses the first video once it is listed (for screenshots)
        static VIDEO_PICKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var_os("PICVIEW_SELECT_VIDEO").is_some() && !VIDEO_PICKED.load(Ordering::Relaxed) {
            if let Some(i) = self.panes[self.active].entries.iter().position(|e| e.media == Some(loader::Media::Video)) {
                VIDEO_PICKED.store(true, Ordering::Relaxed);
                self.select_single(self.active, i);
            }
        }
        // PICVIEW_PLAY_AUDIO=1 plays the first sound file once it is listed (for screenshots)
        static AUDIO_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var_os("PICVIEW_PLAY_AUDIO").is_some() && !AUDIO_STARTED.load(Ordering::Relaxed) {
            if let Some(i) = self.panes[self.active].entries.iter().position(|e| e.media == Some(loader::Media::Audio)) {
                AUDIO_STARTED.store(true, Ordering::Relaxed);
                self.select_single(self.active, i);
                self.audio_toggle();
            }
        }
        self.dialogs(ctx);
        self.settings_window(ctx);
        self.about_window(ctx);
        self.drag_overlay(ctx);
        self.run_actions();
        self.evict_thumbs();
    }
}

// ---------------------------------------------------------------- helpers

/// Ten short bars under a playing video preview; the current frame is lit.
fn clip_progress(ui: &Ui, area: Rect, img: Vec2, k: usize) {
    let shown = Rect::from_center_size(area.center(), img * fit_scale(img, area.size()));
    let w = (shown.width() - 8.0) / CLIP_FRAMES as f32;
    let y = shown.bottom() - 6.0;
    for j in 0..CLIP_FRAMES {
        let x = shown.left() + 4.0 + j as f32 * w;
        let r = Rect::from_min_max(pos2(x + 1.0, y), pos2(x + w - 1.0, y + 3.0));
        let c = if j == k { Color32::WHITE } else { Color32::from_white_alpha(90) };
        ui.painter().rect_filled(r, 1.0f32, c);
    }
}

fn tex_opts() -> TextureOptions {
    TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..TextureOptions::LINEAR }
}

/// Same drive letter (or UNC share), compared case-insensitively.
fn same_volume(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| p.components().next().map(|c| c.as_os_str().to_string_lossy().to_lowercase());
    root(a) == root(b)
}

fn tree_id(p: &Path) -> Id {
    Id::new(("tree", p))
}

/// Scale that fits `r` into `a`, never enlarging.
fn fit_scale(r: Vec2, a: Vec2) -> f32 {
    (a.x / r.x).min(a.y / r.y).min(1.0)
}

/// Size with the aspect ratio of `tex` but the magnitude of `target`.
fn fit_aspect(target: Vec2, tex: Vec2) -> Vec2 {
    let k = (target.x / tex.x).max(target.y / tex.y);
    tex * k
}

/// Draw `tex` (logical size `img`) inside `area`, rotated by `rot` quarter turns.
fn draw_image(ui: &Ui, tex: &TextureHandle, img: Vec2, area: Rect, rot: i32, scale: Option<f32>, pan: Vec2) {
    let q = rot.rem_euclid(4);
    let rimg = if q % 2 == 1 { vec2(img.y, img.x) } else { img };
    let s = scale.unwrap_or_else(|| fit_scale(rimg, area.size()));
    let rect = Rect::from_center_size(area.center() + pan, img * s);
    egui::Image::from_texture(SizedTexture::new(tex.id(), img))
        .rotate(q as f32 * std::f32::consts::FRAC_PI_2, Vec2::splat(0.5))
        .paint_at(ui, rect);
}

fn setup_fonts(ctx: &egui::Context) {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let Ok(bytes) = std::fs::read(Path::new(&windir).join("Fonts").join("malgun.ttf")) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("malgun".into(), Arc::new(egui::FontData::from_owned(bytes)));
    if let Some(f) = fonts.families.get_mut(&FontFamily::Proportional) {
        f.insert(0, "malgun".into());
    }
    if let Some(f) = fonts.families.get_mut(&FontFamily::Monospace) {
        f.push("malgun".into());
    }
    ctx.set_fonts(fonts);
}

fn normalize(p: PathBuf) -> PathBuf {
    match std::fs::canonicalize(&p) {
        Ok(c) => {
            let s = c.to_string_lossy();
            match s.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest.to_string()),
                _ => c,
            }
        }
        Err(_) => p,
    }
}

fn is_hidden(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & (0x2 | 0x4) != 0 // HIDDEN | SYSTEM
}

fn list_dir(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut v = Vec::new();
    for de in std::fs::read_dir(dir)?.flatten() {
        let Ok(md) = de.metadata() else { continue };
        if is_hidden(&md) || de.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = de.path();
        let is_dir = md.is_dir();
        let media = if is_dir { None } else { loader::media_kind(&path) };
        if !is_dir && media.is_none() && !loader::is_image(&path) && !archive::is_archive(&path) {
            continue;
        }
        v.push(Entry {
            name: de.file_name().to_string_lossy().into_owned(),
            path,
            is_dir,
            media,
            size: if is_dir { 0 } else { md.len() },
            modified: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    Ok(v)
}

fn list_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|de| !de.file_name().to_string_lossy().starts_with('.'))
        .filter(|de| de.metadata().map(|m| m.is_dir() && !is_hidden(&m)).unwrap_or(false))
        .map(|de| de.path())
        .collect();
    v.sort_by(|a, b| {
        natural_cmp(
            &a.file_name().unwrap_or_default().to_string_lossy(),
            &b.file_name().unwrap_or_default().to_string_lossy(),
        )
    });
    v
}

fn natural_cmp(a: &str, b: &str) -> CmpOrd {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return CmpOrd::Equal,
            (None, _) => return CmpOrd::Less,
            (_, None) => return CmpOrd::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != CmpOrd::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = Iterator::cmp(x.to_lowercase(), y.to_lowercase());
                if o != CmpOrd::Equal {
                    return o;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

fn fmt_size(b: u64) -> String {
    let b = b as f64;
    if b < 1024.0 * 1024.0 {
        format!("{:.2}KB", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2}MB", b / 1024.0 / 1024.0)
    } else {
        format!("{:.2}GB", b / 1024.0 / 1024.0 / 1024.0)
    }
}

fn fmt_time(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string()
}

/// Versions before the rename kept their data under "picview"; move it once.
fn migrate_old_dirs() {
    for var in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var).map(PathBuf::from) {
            let (old, new) = (base.join("picview"), base.join(APP_ID));
            if old.is_dir() && !new.exists() {
                let _ = std::fs::rename(&old, &new);
            }
        }
    }
}

fn main() -> eframe::Result {
    migrate_old_dirs();
    let start = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title(format!("Oddity PicView {}", env!("CARGO_PKG_VERSION"))),
        // PICVIEW_DATA_DIR keeps benchmark runs away from the user's settings
        persistence_path: std::env::var_os("PICVIEW_DATA_DIR").map(|d| PathBuf::from(d).join("app.ron")),
        ..Default::default()
    };
    eframe::run_native(APP_ID, options, Box::new(move |cc| Ok(Box::new(App::new(cc, start)))))
}
