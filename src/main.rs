#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod cache;
mod fileops;
mod icons;
mod loader;
mod rotate;

use eframe::egui::{
    self, collapsing_header::CollapsingState, load::SizedTexture, pos2, text::LayoutJob, vec2,
    Align, Align2, Color32, ColorImage, FontFamily, FontId, Id, Key, Rect, Sense, Stroke,
    TextFormat, TextureHandle, TextureOptions, Ui, Vec2,
};
use std::cmp::Ordering as CmpOrd;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};

/// Folder name for settings (%APPDATA%) and the thumbnail cache (%LOCALAPPDATA%).
pub const APP_ID: &str = "oddity-picview";
const AUTHOR: &str = "destory1984";
const REPO_URL: &str = "https://github.com/destory1984/oddity-picview";

const GAP: f32 = 8.0;
/// Above this thumbnail size folders get their own block of small tiles.
const FOLDER_THUMB: f32 = 96.0;
/// Extra space (with a thin line) between the folder block and the files.
const SECTION_GAP: f32 = 14.0;

/// One row of the thumbnail grid. Slots count the ".." cell first.
#[derive(Clone, Copy)]
struct GridRow {
    top: f32,
    first: usize,
    count: usize,
    cell: Vec2,
}

struct GridLayout {
    rows: Vec<GridRow>,
    total_h: f32,
    /// y of the line between folders and files
    split_y: Option<f32>,
}

impl GridLayout {
    fn row_of(&self, slot: usize) -> Option<usize> {
        self.rows.iter().position(|r| slot >= r.first && slot < r.first + r.count)
    }

    /// Slot `k` rows up (-) or down (+) from `slot`, keeping the same x where possible.
    fn step_rows(&self, slot: usize, k: isize) -> Option<usize> {
        let r = self.row_of(slot)?;
        let row = self.rows[r];
        let x = (slot - row.first) as f32 * (row.cell.x + GAP) + row.cell.x / 2.0;
        let nr = (r as isize + k).clamp(0, self.rows.len() as isize - 1) as usize;
        let to = self.rows[nr];
        let c = ((x / (to.cell.x + GAP)).floor().max(0.0) as usize).min(to.count - 1);
        Some(to.first + c)
    }
}

fn cell_of(ts: f32) -> Vec2 {
    vec2(ts + 16.0, ts + 16.0 + 22.0)
}
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

// ---------------------------------------------------------------- data types

struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    /// video / audio: shell thumbnail, opened in the default program
    media: Option<loader::Media>,
    size: u64,
    modified: SystemTime,
}

impl Entry {
    fn is_img(&self) -> bool {
        !self.is_dir && self.media.is_none()
    }
}

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
                KindFilter::Image => e.is_img(),
                KindFilter::Video => e.media == Some(loader::Media::Video),
                KindFilter::Audio => e.media == Some(loader::Media::Audio),
            }
    }
}

enum Thumb {
    None,
    Pending,
    Ready(TextureHandle),
    Failed,
}

enum Big {
    Pending,
    Ready(TextureHandle, [u32; 2]),
    Failed,
}

enum Msg {
    Thumb { gen: u64, idx: usize, mtime: SystemTime, tex: TextureHandle },
    ThumbFail { gen: u64, idx: usize },
    Skip { gen: u64, idx: usize },
    Big { path: PathBuf, mtime: Option<SystemTime>, res: Result<(TextureHandle, [u32; 2]), String> },
    Rotated { path: PathBuf, turns: i32, res: Result<(), String> },
    /// frames of an animated GIF, sent after its first frame (`Big`)
    Anim { path: PathBuf, frames: Vec<(TextureHandle, Duration)> },
    /// video preview frames (None: Windows could not decode it)
    Clip { path: PathBuf, frames: Option<Vec<TextureHandle>> },
    OpDone { status: String, dirs: Vec<PathBuf>, clear_clipboard: bool },
}

/// Commands from menus and shortcuts. Queued and run after the frame is drawn,
/// because most of them change `entries` or the folder tree.
enum Action {
    Open(usize),
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            last_dir: None,
            recent: Vec::new(),
            sort: SortKey::Name,
            sort_desc: false,
            thumb_size: 160.0,
            favorites: Vec::new(),
            transfer_dirs: Vec::new(),
            cache_max_mb: cache::DEFAULT_MAX_MB,
            confirm_delete: true,
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
    idx: usize,
    zoom: Option<f32>, // None = fit to window
    pan: Vec2,
}

enum Dialog {
    Delete(Vec<PathBuf>),
    Rename { path: PathBuf, text: String, first: bool },
    NewFolder { parent: PathBuf, text: String, first: bool },
}

// ---------------------------------------------------------------- app

struct App {
    ctx: egui::Context,
    cur_dir: PathBuf,
    addr_text: String,
    entries: Vec<Entry>,
    thumbs: Vec<Thumb>,
    thumbs_loaded: usize,

    gen: Arc<AtomicU64>,
    vis_start: Arc<AtomicUsize>,
    vis_end: Arc<AtomicUsize>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,

    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    focus: Option<usize>,
    focus_time: Instant,
    focus_dims: Option<[u32; 2]>,

    back: Vec<PathBuf>,
    fwd: Vec<PathBuf>,

    thumb_size: f32,
    /// grid width from the last frame, for layout outside of drawing
    grid_w: f32,
    grid_offset: f32,
    grid_view_h: f32,
    grid_scroll_to: Option<f32>,
    wheel_acc: f32,
    scroll_anim: Option<(f32, f32, Instant)>, // from, to, start

    sort: SortKey,
    sort_desc: bool,

    big: HashMap<PathBuf, Big>,
    big_order: VecDeque<PathBuf>,
    anims: HashMap<PathBuf, Anim>,
    rot: HashMap<PathBuf, i32>,

    viewer: Option<Viewer>,
    fullscreen: bool,
    dialog: Option<Dialog>,
    status: String,

    drives: Vec<PathBuf>,
    tree_children: HashMap<PathBuf, Vec<PathBuf>>,
    tree_scroll_pending: bool,
    pending_open: Option<usize>,
    recent: Vec<PathBuf>,
    favorites: Vec<PathBuf>,
    actions: Vec<Action>,
    /// files cut to the clipboard by us, drawn faded until pasted
    cut_paths: HashSet<PathBuf>,
    ctrl_v_prev: bool,
    /// rotation (quarter turns) being written to each file right now
    rot_saving: HashMap<PathBuf, i32>,
    /// rotation already written to the file but not to this loaded texture
    baked: HashMap<egui::TextureId, i32>,
    icons: Arc<icons::Icons>,
    transfer_dirs: Vec<PathBuf>,
    /// folder under the pointer while dragging files (set by drop targets each frame)
    dnd_target: Option<PathBuf>,
    cache_max_mb: u64,
    confirm_delete: bool,
    show_settings: bool,
    show_about: bool,
    /// bytes in the thumbnail cache, measured in the background (u64::MAX = not yet)
    cache_usage: Arc<AtomicU64>,
    /// audio preview (Space on a sound file); stops when the focus moves
    player: Option<audio::Player>,
    /// video previews by path, most recently used last in `clip_order`
    clips: HashMap<PathBuf, Clip>,
    clip_order: VecDeque<PathBuf>,
    /// video thumbnail under the pointer and since when
    clip_hover: Option<(usize, Instant)>,
    /// file kind shown in the list (address bar buttons)
    kind_filter: KindFilter,
    /// files of each kind in the folder before filtering: all, image, video, audio
    kind_counts: [usize; 4],
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, start: Option<PathBuf>) -> Self {
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
            ctx: cc.egui_ctx.clone(),
            cur_dir: PathBuf::new(),
            addr_text: String::new(),
            entries: Vec::new(),
            thumbs: Vec::new(),
            thumbs_loaded: 0,
            gen: Arc::new(AtomicU64::new(0)),
            vis_start: Arc::new(AtomicUsize::new(0)),
            vis_end: Arc::new(AtomicUsize::new(0)),
            tx,
            rx,
            selected: BTreeSet::new(),
            anchor: None,
            focus: None,
            focus_time: Instant::now(),
            focus_dims: None,
            back: Vec::new(),
            fwd: Vec::new(),
            thumb_size: saved.thumb_size.clamp(64.0, 256.0),
            grid_w: 800.0,
            grid_offset: 0.0,
            grid_view_h: 600.0,
            grid_scroll_to: None,
            wheel_acc: 0.0,
            scroll_anim: None,
            sort: saved.sort,
            sort_desc: saved.sort_desc,
            big: HashMap::new(),
            big_order: VecDeque::new(),
            anims: HashMap::new(),
            rot: HashMap::new(),
            viewer: None,
            fullscreen: false,
            dialog: None,
            status: String::new(),
            drives,
            tree_children: HashMap::new(),
            tree_scroll_pending: false,
            pending_open: None,
            favorites: saved.favorites.clone(),
            actions: Vec::new(),
            cut_paths: HashSet::new(),
            ctrl_v_prev: false,
            rot_saving: HashMap::new(),
            baked: HashMap::new(),
            icons: Arc::new(icons::Icons::new(&cc.egui_ctx)),
            transfer_dirs: saved.transfer_dirs.iter().filter(|p| p.is_dir()).take(TRANSFER_MAX).cloned().collect(),
            dnd_target: None,
            cache_max_mb: saved.cache_max_mb.clamp(CACHE_MIN_MB, CACHE_MAX_MB),
            confirm_delete: saved.confirm_delete,
            show_settings: false,
            show_about: false,
            cache_usage: Arc::new(AtomicU64::new(u64::MAX)),
            player: None,
            clips: HashMap::new(),
            clip_order: VecDeque::new(),
            clip_hover: None,
            kind_filter: KindFilter::All,
            kind_counts: [0; 4],
            recent: saved.recent.into_iter().filter(|p| p.is_dir()).take(RECENT_MAX).collect(),
        };

        let home = std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("C:\\"));
        match start {
            Some(p) if p.is_file() => {
                let p = normalize(p);
                if let Some(parent) = p.parent() {
                    app.navigate(parent.to_path_buf(), false);
                }
                if let Some(i) = app.index_of(&p) {
                    app.select_single(i);
                    app.viewer = Some(Viewer { idx: i, zoom: None, pan: Vec2::ZERO });
                }
            }
            Some(p) if p.is_dir() => app.navigate(p, false),
            _ => match saved.last_dir.filter(|d| d.is_dir()) {
                Some(d) => app.navigate(d, false),
                None => {
                    let pics = home.join("Pictures");
                    app.navigate(if pics.is_dir() { pics } else { home }, false);
                }
            },
        }
        app
    }

    // ------------------------------------------------------------ navigation

    fn navigate(&mut self, dir: PathBuf, push_history: bool) {
        self.player = None;
        let dir = normalize(dir);
        let entries = match self.list_filtered(&dir) {
            Ok(e) => e,
            Err(e) => {
                self.status = format!("폴더를 열 수 없습니다: {e}");
                return;
            }
        };
        if push_history && !self.cur_dir.as_os_str().is_empty() && self.cur_dir != dir {
            self.back.push(self.cur_dir.clone());
            self.fwd.clear();
        }
        self.status.clear();
        self.recent.retain(|p| p != &dir);
        self.recent.insert(0, dir.clone());
        self.recent.truncate(RECENT_MAX);
        self.cur_dir = dir.clone();
        self.addr_text = dir.display().to_string();
        self.entries = entries;
        self.thumbs = (0..self.entries.len()).map(|_| Thumb::None).collect();
        self.thumbs_loaded = 0;
        self.gen.fetch_add(1, Ordering::SeqCst);
        self.sort_entries();
        self.selected.clear();
        self.anchor = None;
        self.focus = None;
        self.focus_dims = None;
        self.grid_scroll_to = Some(0.0);
        self.open_tree_to(&dir);
    }

    /// List a folder, count each kind, and keep only the kind picked in the filter.
    fn list_filtered(&mut self, dir: &Path) -> std::io::Result<Vec<Entry>> {
        let mut v = list_dir(dir)?;
        let files = v.iter().filter(|e| !e.is_dir);
        self.kind_counts = [0; 4];
        for e in files {
            self.kind_counts[0] += 1;
            for (k, f) in KindFilter::ALL.iter().enumerate().skip(1) {
                if f.keeps(e) {
                    self.kind_counts[k] += 1;
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
            self.reload(None);
            self.grid_scroll_to = Some(0.0);
        }
    }

    /// Re-read the current folder, keeping already made thumbnails.
    fn reload(&mut self, focus_path: Option<PathBuf>) {
        let Ok(entries) = self.list_filtered(&self.cur_dir.clone()) else { return };
        let mut old: HashMap<PathBuf, TextureHandle> = HashMap::new();
        for (e, t) in self.entries.iter().zip(self.thumbs.drain(..)) {
            if let Thumb::Ready(tex) = t {
                old.insert(e.path.clone(), tex);
            }
        }
        // remember the focused file by path: the index means nothing in the new list
        let old_focus = self.focus;
        let old_focus_path = old_focus.and_then(|i| self.entries.get(i)).map(|e| e.path.clone());
        self.focus = None;
        self.entries = entries;
        self.thumbs = self
            .entries
            .iter()
            .map(|e| old.remove(&e.path).map(Thumb::Ready).unwrap_or(Thumb::None))
            .collect();
        self.thumbs_loaded = self.thumbs.iter().filter(|t| matches!(t, Thumb::Ready(_))).count();
        self.gen.fetch_add(1, Ordering::SeqCst);
        self.sort_entries();
        self.selected.clear();
        let n = self.entries.len();
        let explicit = focus_path.is_some();
        let target = focus_path
            .and_then(|p| self.index_of(&p))
            .or_else(|| old_focus_path.and_then(|p| self.index_of(&p)))
            .or_else(|| old_focus.filter(|_| n > 0).map(|f| f.min(n - 1)));
        match target {
            // a file we were asked to show (rename, new folder): scroll to it
            Some(i) if explicit => self.select_single(i),
            // after a move / paste / refresh: keep the view where it is
            Some(i) => {
                self.selected.insert(i);
                self.anchor = Some(i);
                self.set_focus(i);
            }
            None => {
                self.focus = None;
                self.anchor = None;
            }
        }
        self.tree_children.remove(&self.cur_dir);
    }

    fn go_back(&mut self) {
        if let Some(p) = self.back.pop() {
            self.fwd.push(self.cur_dir.clone());
            self.navigate(p, false);
        }
    }

    fn go_fwd(&mut self) {
        if let Some(p) = self.fwd.pop() {
            self.back.push(self.cur_dir.clone());
            self.navigate(p, false);
        }
    }

    fn go_up(&mut self) {
        let cur = self.cur_dir.clone();
        if let Some(parent) = cur.parent() {
            self.navigate(parent.to_path_buf(), true);
            if let Some(i) = self.index_of(&cur) {
                self.select_single(i);
            }
        }
    }

    fn open_address(&mut self) {
        let p = PathBuf::from(self.addr_text.trim().trim_matches('"'));
        if p.is_file() {
            let p = normalize(p);
            if let Some(parent) = p.parent() {
                self.navigate(parent.to_path_buf(), true);
            }
            if let Some(i) = self.index_of(&p) {
                self.select_single(i);
            }
        } else {
            self.navigate(p, true);
        }
    }

    /// Space on a sound file: start, pause or resume the preview.
    fn audio_toggle(&mut self) {
        let Some(e) = self.focus.and_then(|i| self.entries.get(i)) else { return };
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

    fn open_entry(&mut self, i: usize) {
        let Some(e) = self.entries.get(i) else { return };
        if e.is_dir {
            let p = e.path.clone();
            self.navigate(p, true);
        } else if e.media.is_some() {
            fileops::open_default(&e.path);
        } else {
            self.viewer = Some(Viewer { idx: i, zoom: None, pan: Vec2::ZERO });
        }
    }

    fn index_of(&self, p: &Path) -> Option<usize> {
        self.entries.iter().position(|e| e.path == p)
    }

    fn sort_entries(&mut self) {
        let focus_path = self.focus.and_then(|i| self.entries.get(i)).map(|e| e.path.clone());
        let thumbs = std::mem::take(&mut self.thumbs);
        let mut v: Vec<(Entry, Thumb)> = self.entries.drain(..).zip(thumbs).collect();
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
            self.thumbs.push(if matches!(t, Thumb::Pending) { Thumb::None } else { t });
            self.entries.push(e);
        }
        self.gen.fetch_add(1, Ordering::SeqCst);
        if let Some(p) = focus_path {
            if let Some(i) = self.index_of(&p) {
                self.select_single(i);
            }
        }
    }

    // ------------------------------------------------------------ selection

    fn set_focus(&mut self, i: usize) {
        if self.player.as_ref().is_some_and(|p| self.entries.get(i).map(|e| &e.path) != Some(&p.path)) {
            self.player = None;
        }
        if self.focus != Some(i) {
            self.focus = Some(i);
            self.focus_time = Instant::now();
            self.focus_dims = self
                .entries
                .get(i)
                .filter(|e| e.is_img())
                .and_then(|e| image::image_dimensions(&e.path).ok())
                .map(|(w, h)| [w, h]);
        }
    }

    fn select_single(&mut self, i: usize) {
        self.selected.clear();
        self.selected.insert(i);
        self.anchor = Some(i);
        self.set_focus(i);
        self.ensure_visible(i);
    }

    fn move_focus(&mut self, t: usize, shift: bool) {
        if shift {
            let a = self.anchor.unwrap_or(t);
            self.selected = (a.min(t)..=a.max(t)).collect();
            self.set_focus(t);
            self.ensure_visible(t);
        } else {
            self.select_single(t);
        }
    }

    fn click_entry(&mut self, i: usize, mods: egui::Modifiers) {
        if mods.command {
            if !self.selected.insert(i) {
                self.selected.remove(&i);
            }
            self.anchor = Some(i);
            self.set_focus(i);
        } else if mods.shift {
            self.move_focus(i, true);
        } else {
            self.select_single(i);
        }
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.selected.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.path.clone()).collect();
        if v.is_empty() {
            if let Some(e) = self.focus.and_then(|i| self.entries.get(i)) {
                v.push(e.path.clone());
            }
        }
        v
    }

    fn cell_size(&self) -> Vec2 {
        vec2(self.thumb_size + 16.0, self.thumb_size + 16.0 + 22.0)
    }

    /// 1 when the grid starts with the ".." (parent folder) cell.
    fn slot_off(&self) -> usize {
        self.cur_dir.parent().is_some() as usize
    }

    /// Rows of the grid for width `w`. Folders come first (sorting keeps them
    /// there); with big thumbnails they get small tiles in a block of their own.
    fn layout(&self, w: f32) -> GridLayout {
        let off = self.slot_off();
        let total = self.entries.len() + off;
        let ndirs = self.entries.iter().take_while(|e| e.is_dir).count();
        let nfs = if self.thumb_size > FOLDER_THUMB { off + ndirs } else { 0 };
        let mut rows = Vec::new();
        let mut y = 0.0;
        let section = |rows: &mut Vec<GridRow>, y: &mut f32, start: usize, end: usize, cell: Vec2| {
            let cols = (((w + GAP) / (cell.x + GAP)).floor() as usize).max(1);
            let mut s = start;
            while s < end {
                let count = cols.min(end - s);
                rows.push(GridRow { top: *y, first: s, count, cell });
                *y += cell.y + GAP;
                s += count;
            }
        };
        section(&mut rows, &mut y, 0, nfs, cell_of(FOLDER_THUMB));
        let mut split_y = None;
        if nfs > 0 && nfs < total {
            split_y = Some(y - GAP / 2.0 + SECTION_GAP / 2.0);
            y += SECTION_GAP;
        }
        section(&mut rows, &mut y, nfs, total, self.cell_size());
        GridLayout { rows, total_h: (y - GAP).max(0.0), split_y }
    }

    fn ensure_visible(&mut self, i: usize) {
        let lay = self.layout(self.grid_w);
        let Some(r) = lay.row_of(i + self.slot_off()).map(|r| lay.rows[r]) else { return };
        let (top, cell) = (r.top, r.cell);
        if top < self.grid_offset {
            self.grid_scroll_to = Some(top);
        } else if top + cell.y > self.grid_offset + self.grid_view_h {
            self.grid_scroll_to = Some(top + cell.y - self.grid_view_h);
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
        match self.clips.get(p)? {
            Clip::Ready(a) => self.anim_pick(a),
            _ => None,
        }
    }

    /// Start making the video preview of entry `i` (from the disk cache if it is there).
    fn request_clip(&mut self, i: usize) {
        let Some(e) = self.entries.get(i).filter(|e| e.media == Some(loader::Media::Video)) else { return };
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
            let _ = tx.send(Msg::Clip { path, frames });
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
            Some(v) => self.entries.get(v.idx).map(|e| vec![e.path.clone()]).unwrap_or_default(),
            None => self.selected_paths(),
        };
        self.rotate_paths(targets, d);
    }

    /// Show the turn at once, then write it to the file in the background.
    fn rotate_paths(&mut self, paths: Vec<PathBuf>, d: i32) {
        for p in paths {
            if !loader::is_image(&p) {
                continue;
            }
            let r = self.rot.entry(p.clone()).or_insert(0);
            *r = (*r + d).rem_euclid(4);
            self.save_rotation(&p);
        }
    }

    fn save_rotation(&mut self, p: &Path) {
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
        if let Some(i) = self.index_of(&path) {
            if let Thumb::Ready(t) = &self.thumbs[i] {
                texs.push(t.id());
            }
            if matches!(self.thumbs[i], Thumb::Pending) {
                self.thumbs[i] = Thumb::None;
            }
            // new mtime = new thumbnail cache key
            if let Ok(md) = std::fs::metadata(&path) {
                self.entries[i].size = md.len();
                self.entries[i].modified = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            }
        }
        for id in texs {
            *self.baked.entry(id).or_insert(0) += turns;
        }
        self.status = "회전을 파일에 저장했습니다".into();
        self.save_rotation(&path); // more turns pressed while saving
    }

    // ------------------------------------------------------------ loading

    fn request_thumb(&mut self, i: usize) {
        let Some(e) = self.entries.get(i) else { return };
        if e.is_dir || !matches!(self.thumbs[i], Thumb::None) {
            return;
        }
        self.thumbs[i] = Thumb::Pending;
        // video, audio, HEIC and RAW: the shell's thumbnail (RAW has a fast embedded preview)
        let shell = e.media.is_some() || loader::is_wic(&e.path);
        let (path, size, modified, media) = (e.path.clone(), e.size, e.modified, shell);
        let gen = self.gen.load(Ordering::SeqCst);
        let (gen_now, vs, ve) = (self.gen.clone(), self.vis_start.clone(), self.vis_end.clone());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        rayon::spawn(move || {
            if gen_now.load(Ordering::SeqCst) != gen {
                return;
            }
            // user scrolled away before we got here: give up, it will be asked again
            let (s, e) = (vs.load(Ordering::Relaxed), ve.load(Ordering::Relaxed));
            if i + 120 < s || i > e + 120 {
                let _ = tx.send(Msg::Skip { gen, idx: i });
                ctx.request_repaint();
                return;
            }
            let made = match cache::load(&path, size, modified) {
                Some(img) => Ok(img),
                None => {
                    let made = if media {
                        fileops::shell_thumbnail(&path, loader::THUMB_MAX as i32)
                    } else {
                        loader::make_thumb(&path)
                    };
                    made.inspect(|img| cache::store(&path, size, modified, img))
                }
            };
            let msg = match made {
                Ok(img) => {
                    let ci = ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                    let tex = ctx.load_texture(format!("t{gen}/{i}"), ci, tex_opts());
                    Msg::Thumb { gen, idx: i, mtime: modified, tex }
                }
                Err(_) => Msg::ThumbFail { gen, idx: i },
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
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
        let mtime = self.index_of(path).map(|i| self.entries[i].modified);
        let path = path.to_path_buf();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let max_side = ctx.input(|i| i.max_texture_side).min(16384) as u32;
        std::thread::spawn(move || {
            let decoded = if loader::is_wic(&path) {
                fileops::wic_decode(&path, max_side).map(|(img, dims, o)| loader::orient(img, dims, o))
            } else {
                loader::load_full(&path, max_side)
            };
            let res = decoded.map(|(img, dims)| {
                let ci = ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
                (ctx.load_texture(path.display().to_string(), ci, tex_opts()), dims)
            });
            let ok = res.is_ok();
            let _ = tx.send(Msg::Big { path: path.clone(), mtime, res });
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
                    let _ = tx.send(Msg::Anim { path, frames });
                    ctx.request_repaint();
                }
            }
        });
    }

    fn poll_msgs(&mut self) {
        let gen = self.gen.load(Ordering::SeqCst);
        let n = self.thumbs.len();
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Thumb { gen: g, idx, mtime, tex } if g == gen && idx < n => {
                    if self.entries[idx].modified == mtime {
                        self.thumbs[idx] = Thumb::Ready(tex);
                        self.thumbs_loaded += 1;
                    } else {
                        self.thumbs[idx] = Thumb::None; // file changed while decoding
                    }
                }
                Msg::ThumbFail { gen: g, idx } if g == gen && idx < n => self.thumbs[idx] = Thumb::Failed,
                Msg::Skip { gen: g, idx } if g == gen && idx < n => {
                    if matches!(self.thumbs[idx], Thumb::Pending) {
                        self.thumbs[idx] = Thumb::None;
                    }
                }
                Msg::Rotated { path, turns, res } => self.on_rotated(path, turns, res),
                Msg::Clip { path, frames } => {
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
                Msg::Anim { path, frames } => {
                    if self.big.contains_key(&path) {
                        let total = frames.iter().map(|f| f.1).sum();
                        self.anims.insert(path, Anim { frames, total, start: Instant::now() });
                    }
                }
                Msg::OpDone { status, dirs, clear_clipboard } => {
                    self.status = status;
                    if clear_clipboard {
                        fileops::clipboard_clear();
                        self.cut_paths.clear();
                    }
                    for d in dirs {
                        self.refresh_dir(&d);
                    }
                }
                Msg::Big { path, mtime, res } => {
                    let now = self.index_of(&path).map(|i| self.entries[i].modified);
                    if mtime != now {
                        self.big.remove(&path); // file changed while decoding
                        continue;
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
        if self.thumbs_loaded <= MAX_THUMBS_LOADED {
            return;
        }
        let s = self.vis_start.load(Ordering::Relaxed).saturating_sub(300);
        let e = self.vis_end.load(Ordering::Relaxed) + 300;
        for (i, t) in self.thumbs.iter_mut().enumerate() {
            if (i < s || i >= e) && matches!(t, Thumb::Ready(_)) {
                *t = Thumb::None;
                self.thumbs_loaded -= 1;
            }
        }
    }

    // ------------------------------------------------------------ file ops

    fn ask_delete(&mut self, paths: Vec<PathBuf>) {
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
        // the folder being shown (or one above it) was deleted
        if let Some(gone) = paths.iter().find(|p| self.cur_dir.starts_with(p)).cloned() {
            if let Some(parent) = gone.parent() {
                self.navigate(parent.to_path_buf(), false);
            }
            return;
        }
        let viewer_idx = self.viewer.as_ref().map(|v| v.idx);
        self.reload(None);
        if let Some(idx) = viewer_idx {
            let n = self.entries.len();
            let next = (idx.min(n.saturating_sub(1))..n)
                .chain((0..idx.min(n)).rev())
                .find(|&i| self.entries[i].is_img());
            match next {
                Some(i) => {
                    self.viewer = Some(Viewer { idx: i, zoom: None, pan: Vec2::ZERO });
                    self.select_single(i);
                }
                None => self.viewer = None,
            }
        }
    }

    fn ask_rename(&mut self) {
        if let Some(p) = self.focus.and_then(|i| self.entries.get(i)).map(|e| e.path.clone()) {
            self.ask_rename_path(p);
        }
    }

    fn ask_rename_path(&mut self, path: PathBuf) {
        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) {
            self.dialog = Some(Dialog::Rename { path, text: name, first: true });
        }
    }

    fn ask_new_folder(&mut self, parent: PathBuf) {
        let mut name = "새 폴더".to_string();
        let mut k = 2;
        while parent.join(&name).exists() {
            name = format!("새 폴더 ({k})");
            k += 1;
        }
        self.dialog = Some(Dialog::NewFolder { parent, text: name, first: true });
    }

    fn do_new_folder(&mut self, parent: PathBuf, name: &str) {
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
                if parent == self.cur_dir {
                    self.reload(Some(target));
                }
            }
            Err(e) => self.status = format!("폴더 만들기 실패: {e}"),
        }
    }

    /// Forget cached tree children under `d` and re-read the grid if it shows `d`.
    fn refresh_dir(&mut self, d: &Path) {
        self.tree_children.retain(|k, _| !k.starts_with(d));
        if self.cur_dir == d {
            self.reload(None);
        }
    }

    fn do_rename(&mut self, path: PathBuf, new_name: &str) {
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
                // renamed the folder being shown, or one above it
                if let Ok(rest) = self.cur_dir.clone().strip_prefix(&path) {
                    let new = if rest.as_os_str().is_empty() { to.clone() } else { to.join(rest) };
                    self.navigate(new, false);
                } else if path.parent() == Some(self.cur_dir.as_path()) {
                    self.reload(Some(to));
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
        match a {
            Action::Open(i) => self.open_entry(i),
            Action::Go(p) => self.navigate(p, true),
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
            Action::Copy(ps) => match fileops::clipboard_set_files(&ps, false) {
                Ok(()) => {
                    self.status = format!("{}개 항목을 복사했습니다", ps.len());
                    self.cut_paths.clear();
                }
                Err(e) => self.status = format!("복사 실패: {e}"),
            },
            Action::Paste(dest) => self.paste(dest),
            Action::Delete(ps) => self.ask_delete(ps),
            Action::Rename(p) => self.ask_rename_path(p),
            Action::Properties(p) => fileops::show_properties(&p),
            Action::Refresh(d) => self.refresh_dir(&d),
            Action::Rotate(ps, d) => self.rotate_paths(ps, d),
            Action::ShowInExplorer(p) => self.show_in_explorer(&p),
            Action::Transfer(files, dest, mv) => self.transfer(files, dest, mv, false),
            Action::PickAndTransfer(files, mv) => {
                let title = if mv { "옮길 폴더 선택" } else { "복사할 폴더 선택" };
                if let Some(dest) = rfd::FileDialog::new().set_title(title).set_directory(&self.cur_dir).pick_folder() {
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
        } else {
            files.first().is_some_and(|f| same_volume(f, dest))
        }
    }

    /// Make `resp` (a folder) accept dragged files.
    fn drop_target(&mut self, ui: &Ui, resp: &egui::Response, dir: &Path) {
        if let Some(drag) = resp.dnd_hover_payload::<DragFiles>() {
            let valid = !drag.0.iter().any(|f| dir.starts_with(f)) // not into itself
                && !drag.0.iter().all(|f| f.parent() == Some(dir)); // not where they already are
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
            match fileops::drag_out(&files) {
                Ok(0) => {}
                Ok(_) => self.status = format!("{n}개 항목을 다른 프로그램에 넘겼습니다"),
                Err(e) => self.status = format!("끌어놓기 실패: {e}"),
            }
            fileops::post_left_button_up();
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
                format!("{}개 항목 (Ctrl: 복사, Shift: 이동)", drag.0.len())
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
        let Some((files, cut)) = fileops::clipboard_get_files() else {
            self.status = "붙여넣을 파일이 없습니다".into();
            return;
        };
        self.transfer(files, dest, cut, cut);
    }

    /// Copy or move `files` into `dest` on a worker thread with Explorer's dialogs.
    fn transfer(&mut self, files: Vec<PathBuf>, dest: PathBuf, cut: bool, from_clipboard: bool) {
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
        std::thread::spawn(move || {
            let res = fileops::shell_copy_move(&files, &dest, cut);
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

    /// Explorer-style menu. `targets` are the right-clicked items (empty = blank
    /// area); `dir` is the folder that "new folder", "paste" and "refresh" use.
    fn file_menu(&mut self, ui: &mut Ui, targets: Vec<PathBuf>, dir: PathBuf) {
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
            self.navigate(p, true);
        }
    }

    fn tree_node(&mut self, ui: &mut Ui, path: &Path, label: String, nav: &mut Option<PathBuf>) {
        let state = CollapsingState::load_with_default_open(ui.ctx(), tree_id(path), false);
        let selected = self.cur_dir == path;
        let folder = self.icons.folder.id();
        let header = state.show_header(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.add(egui::Image::new(SizedTexture::new(folder, vec2(16.0, 16.0))));
            let r = ui.selectable_label(selected, label);
            self.drop_target(ui, &r, path);
            if r.clicked() {
                *nav = Some(path.to_path_buf());
            }
            r.context_menu(|ui| self.file_menu(ui, vec![path.to_path_buf()], path.to_path_buf()));
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
            self.actions.push(Action::Refresh(self.cur_dir.clone()));
            return;
        }
        for (k, key) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4].into_iter().enumerate() {
            if mods.command && pressed(key) {
                self.set_kind_filter(KindFilter::ALL[k]);
                return;
            }
        }
        if mods.command && pressed(Key::N) {
            self.actions.push(Action::NewFolder(self.cur_dir.clone()));
            return;
        }
        // egui turns Ctrl+C / Ctrl+X into these events instead of key presses
        let (copy, cut) = ctx.input(|i| {
            (
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                i.events.iter().any(|e| matches!(e, egui::Event::Cut)),
            )
        });
        if copy {
            self.actions.push(Action::Copy(self.selected_paths()));
        }
        if cut {
            self.actions.push(Action::Cut(self.selected_paths()));
        }
        let n = self.entries.len();
        if n == 0 {
            return;
        }
        let page = (self.grid_view_h / (self.cell_size().y + GAP)).floor().max(1.0) as isize;
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
            target = Some(match self.focus {
                None => 0,
                Some(f) => (f as isize + d).clamp(0, n as isize - 1) as usize,
            });
        }
        if let Some(k) = vrows {
            // rows can differ in width (small folder tiles), so step by geometry
            target = Some(match self.focus {
                None => 0,
                Some(f) => {
                    let off = self.slot_off();
                    let lay = self.layout(self.grid_w);
                    let slot = lay.step_rows(f + off, k).unwrap_or(f + off);
                    slot.max(off) - off // never land on ".."
                }
            });
        }
        if let Some(t) = target {
            self.move_focus(t, mods.shift);
        }
        if mods.command && pressed(Key::A) {
            self.selected = (0..n).collect();
        }
        if pressed(Key::Enter) {
            if let Some(f) = self.focus {
                self.open_entry(f);
            }
        }
        if pressed(Key::Space) {
            self.audio_toggle();
        }
        if pressed(Key::Delete) {
            self.actions.push(Action::Delete(self.selected_paths()));
        }
        if pressed(Key::F2) {
            if let Some(p) = self.focus.and_then(|i| self.entries.get(i)).map(|e| e.path.clone()) {
                self.actions.push(Action::Rename(p));
            }
        }
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        use icons::tool_button;
        let ic = self.icons.clone();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if tool_button(ui, &ic.back, "뒤로", !self.back.is_empty()).on_hover_text("Alt+←").clicked() {
                self.go_back();
            }
            if tool_button(ui, &ic.forward, "앞으로", !self.fwd.is_empty()).on_hover_text("Alt+→").clicked() {
                self.go_fwd();
            }
            if tool_button(ui, &ic.up, "위로", self.cur_dir.parent().is_some()).on_hover_text("Backspace").clicked() {
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
                    let text = if *p == self.cur_dir { text.strong() } else { text };
                    if icons::menu_item(ui, Some(&ic.folder), text, "", true).clicked() {
                        go = Some(p.clone());
                    }
                }
                if self.recent.is_empty() {
                    ui.weak("없음");
                }
            });
            if let Some(p) = go {
                self.navigate(p, true);
            }
            if tool_button(ui, &ic.refresh, "새로고침", true).on_hover_text("F5").clicked() {
                self.actions.push(Action::Refresh(self.cur_dir.clone()));
            }
            ui.separator();

            let has_img = self.focus.and_then(|i| self.entries.get(i)).map(|e| e.is_img()).unwrap_or(false);
            let has_sel = !self.selected_paths().is_empty();
            if tool_button(ui, &ic.view, "크게보기", has_img).on_hover_text("Enter").clicked() {
                if let Some(f) = self.focus {
                    self.open_entry(f);
                }
            }
            if tool_button(ui, &ic.rotate_left, "-90° 회전", has_sel).on_hover_text("파일에 저장됩니다").clicked() {
                self.rotate_current(-1);
            }
            if tool_button(ui, &ic.rotate_right, "90° 회전", has_sel).on_hover_text("파일에 저장됩니다").clicked() {
                self.rotate_current(1);
            }
            if tool_button(ui, &ic.rename, "이름변경", self.focus.is_some()).on_hover_text("F2").clicked() {
                self.ask_rename();
            }
            if tool_button(ui, &ic.delete, "삭제", has_sel).on_hover_text("Delete (휴지통으로)").clicked() {
                self.actions.push(Action::Delete(self.selected_paths()));
            }
            if tool_button(ui, &ic.explorer, "탐색기", true).on_hover_text("탐색기에서 열기").clicked() {
                let p = self.focus.and_then(|i| self.entries.get(i)).map(|e| e.path.clone());
                match p {
                    Some(p) => self.show_in_explorer(&p),
                    None => {
                        let _ = std::process::Command::new("explorer").arg(&self.cur_dir).spawn();
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
                        self.sort_entries();
                    }
                    ui.separator();
                    self.kind_filter_buttons(ui);
                },
            );
        });
    }

    fn address_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("주소");
            let r = ui.add(egui::TextEdit::singleline(&mut self.addr_text).desired_width(f32::INFINITY));
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
                        let n = self.kind_counts[k];
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
            match self.focus_dims {
                Some([w, h]) => ui.label(format!("{w} x {h}")),
                None => ui.label("-"),
            };
            ui.separator();
            let sel_size: u64 = self.selected.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.size).sum();
            ui.label(format!("{} 개체 선택 ({})", self.selected.len(), fmt_size(sel_size)));
            ui.separator();
            let imgs = self.entries.iter().filter(|e| e.is_img()).count();
            let media = self.entries.iter().filter(|e| e.media.is_some()).count();
            if media > 0 {
                ui.label(format!("총 {} 개체 (그림 {imgs}, 동영상·음성 {media})", self.entries.len()));
            } else {
                ui.label(format!("총 {} 개체 (그림 {imgs})", self.entries.len()));
            }
            if self.kind_filter != KindFilter::All {
                let hidden = self.kind_counts[0] - self.entries.iter().filter(|e| !e.is_dir).count();
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
                    self.navigate(p, true);
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
                        ui.selectable_label(self.cur_dir == f, name)
                    })
                    .inner
                    .on_hover_text(f.display().to_string());
                self.drop_target(ui, &r, &f);
                if r.clicked() {
                    self.actions.push(Action::Go(f.clone()));
                }
                let ic = self.icons.clone();
                r.context_menu(|ui| {
                    icons::style_menu(ui, 200.0);
                    if icons::menu_item(ui, Some(&ic.folder), "열기", "", true).clicked() {
                        self.actions.push(Action::Go(f.clone()));
                        ui.close_menu();
                    }
                    if icons::menu_item(ui, Some(&ic.star), "즐겨찾기에서 제거", "", true).clicked() {
                        self.actions.push(Action::RemoveFavorite(f.clone()));
                        ui.close_menu();
                    }
                });
            }
        }
    }

    fn grid(&mut self, ui: &mut Ui) {
        // empty folder: the whole panel is blank area (see `blank_area`)
        let n = self.entries.len();
        let off = self.slot_off();
        if n + off == 0 {
            let msg = match self.kind_filter {
                KindFilter::All => "그림이 없습니다".to_string(),
                f => format!("이 폴더에 {}이 없습니다", f.label()),
            };
            let r = ui.max_rect();
            self.blank_area(ui, r);
            ui.centered_and_justified(|ui| ui.weak(msg));
            return;
        }
        let avail = ui.available_width();
        self.grid_w = avail;
        let lay = self.layout(avail);
        if self.grid_scroll_to.is_some() {
            self.scroll_anim = None; // keyboard / navigation jumps win over the animation
        }
        self.wheel_rows(ui, &lay);
        if let Some((from, to, start)) = self.scroll_anim {
            let t = (start.elapsed().as_secs_f32() / SCROLL_ANIM.as_secs_f32()).min(1.0);
            let ease = 1.0 - (1.0 - t).powi(3); // ease-out cubic
            self.grid_scroll_to = Some(from + (to - from) * ease);
            if t >= 1.0 {
                self.scroll_anim = None;
            } else {
                ui.ctx().request_repaint();
            }
        }

        let mut sa = egui::ScrollArea::vertical().auto_shrink(false).id_salt("grid");
        if let Some(off) = self.grid_scroll_to.take() {
            sa = sa.vertical_scroll_offset(off);
        }
        let out = sa.show_viewport(ui, |ui, vp| {
            ui.set_height(lay.total_h);
            let origin = ui.max_rect().min;
            // blank area under the cells, registered first so the cells sit on top of it
            let blank = Rect::from_min_size(origin, vec2(avail, lay.total_h.max(vp.height())));
            self.blank_area(ui, blank);
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
                        self.up_cell(ui, rect);
                    } else {
                        self.cell(ui, slot - off, rect);
                    }
                }
            }
            // grid slots -> entry indices (slot 0 may be ".."); visible first, then two rows ahead
            if let (Some(&r0), Some(&r1)) = (visible.first(), visible.last()) {
                let s = lay.rows[r0].first.saturating_sub(off);
                let last = lay.rows[r1];
                let e = (last.first + last.count).saturating_sub(off).min(n);
                self.vis_start.store(s, Ordering::Relaxed);
                self.vis_end.store(e, Ordering::Relaxed);
                let ahead = lay.rows.get(r1 + 2).or(lay.rows.last()).map(|r| r.first + r.count).unwrap_or(0);
                for i in s..ahead.saturating_sub(off).min(n) {
                    self.request_thumb(i);
                }
            }
        });
        self.grid_offset = out.state.offset.y;
        self.grid_view_h = out.inner_rect.height();
    }

    /// Blank part of the list: a click clears the selection, a right-click opens
    /// the folder menu (new folder, paste, refresh, properties...).
    fn blank_area(&mut self, ui: &mut Ui, rect: Rect) {
        let bg = ui.interact(rect, Id::new("grid_bg"), Sense::click());
        if bg.clicked() {
            self.selected.clear();
        }
        let dir = self.cur_dir.clone();
        bg.context_menu(|ui| self.file_menu(ui, Vec::new(), dir.clone()));
    }

    /// One wheel notch scrolls exactly one row of thumbnails.
    fn wheel_rows(&mut self, ui: &mut Ui, lay: &GridLayout) {
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
            self.wheel_acc += zoom;
            let steps = self.wheel_acc.trunc();
            self.wheel_acc -= steps;
            if steps != 0.0 {
                self.thumb_size = (self.thumb_size + steps * 16.0).clamp(64.0, 256.0);
                if let Some(f) = self.focus {
                    self.ensure_visible(f);
                }
            }
        }
        let pitch = self.cell_size().y + GAP;
        let page_rows = (self.grid_view_h / pitch).floor().max(1.0);
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
        self.wheel_acc += notches;
        let steps = self.wheel_acc.trunc();
        self.wheel_acc -= steps;
        if steps == 0.0 {
            return;
        }
        // chain from where the running animation is headed, not where it is now
        let base = match self.scroll_anim {
            Some((_, to, _)) => to,
            None => self.grid_scroll_to.unwrap_or(self.grid_offset),
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
        let max = (lay.total_h - self.grid_view_h).max(0.0);
        let to = lay.rows[row].top.clamp(0.0, max);
        self.grid_scroll_to = None;
        self.scroll_anim = Some((self.grid_offset, to, Instant::now()));
    }

    /// The ".." cell: double-click goes up, files dropped on it move to the parent.
    fn up_cell(&mut self, ui: &mut Ui, rect: Rect) {
        let Some(parent) = self.cur_dir.parent().map(Path::to_path_buf) else { return };
        let resp = ui.interact(rect, Id::new("cell_up"), Sense::click());
        let painter = ui.painter();
        let bg = if resp.hovered() { Color32::from_rgb(235, 244, 255) } else { Color32::WHITE };
        painter.rect_filled(rect, 4.0f32, bg);
        painter.rect_stroke(rect, 4.0f32, Stroke::new(1.0f32, Color32::from_gray(215)), egui::StrokeKind::Inside);
        let ts = rect.width() - 16.0;
        let img_area = Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(ts, ts));
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        let fr = Rect::from_center_size(img_area.center(), img_area.size() * 0.60);
        painter.image(self.icons.folder.id(), fr, uv, Color32::WHITE);
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

    fn cell(&mut self, ui: &mut Ui, i: usize, rect: Rect) {
        let resp = ui.interact(rect, Id::new(("cell", i)), Sense::click_and_drag());
        if resp.drag_started() {
            if !self.selected.contains(&i) {
                self.select_single(i);
            }
            resp.dnd_set_drag_payload(DragFiles(self.selected_paths()));
        }
        let selected = self.selected.contains(&i);
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
        let e = &self.entries[i];
        if e.is_dir {
            let fr = Rect::from_center_size(img_area.center(), img_area.size() * 0.60);
            painter.image(self.icons.folder.id(), fr, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        } else {
            // video under the pointer: after a short rest, play its preview frames
            let mut clip = None;
            if e.media == Some(loader::Media::Video) && resp.hovered() {
                match self.clip_hover {
                    Some((h, since)) if h == i => {
                        if since.elapsed() >= CLIP_HOVER {
                            let path = e.path.clone();
                            self.request_clip(i);
                            clip = self.clip_frame(&path);
                        } else {
                            ui.ctx().request_repaint_after(CLIP_HOVER - since.elapsed());
                        }
                    }
                    _ => {
                        self.clip_hover = Some((i, Instant::now()));
                        ui.ctx().request_repaint_after(CLIP_HOVER);
                    }
                }
            }
            let e = &self.entries[i];
            let painter = ui.painter();
            match &self.thumbs[i] {
                _ if clip.is_some() => {
                    let (t, k) = clip.unwrap();
                    draw_image(ui, &t, t.size_vec2(), img_area, 0, None, Vec2::ZERO);
                    clip_progress(ui, img_area, t.size_vec2(), k);
                }
                Thumb::Ready(t) => {
                    draw_image(ui, t, t.size_vec2(), img_area, self.disp_rot(&e.path, t), None, Vec2::ZERO);
                }
                Thumb::Failed => {
                    painter.text(img_area.center(), Align2::CENTER_CENTER, "읽을 수 없음", FontId::proportional(12.0), Color32::GRAY);
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
        }

        let e = &self.entries[i];
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
            self.pending_open = Some(i);
        } else if resp.clicked() {
            let mods = ui.input(|inp| inp.modifiers);
            self.click_entry(i, mods);
        } else if resp.secondary_clicked() && !selected {
            self.select_single(i);
        }
        if self.entries[i].is_dir {
            self.drop_target(ui, &resp, &path);
        }
        if self.cut_paths.contains(&path) {
            ui.painter().rect_filled(rect, 4.0f32, Color32::from_white_alpha(150));
        }
        let targets = if selected { self.selected_paths() } else { vec![path.clone()] };
        let is_folder = self.entries[i].is_dir;
        let ic = self.icons.clone();
        resp.context_menu(|ui| {
            icons::style_menu(ui, 240.0);
            let open = egui::RichText::new(if is_img { "크게보기" } else { "열기" }).strong();
            let open_icon = if is_img { Some(&ic.view) } else if is_folder { Some(&ic.folder) } else { None };
            if icons::menu_item(ui, open_icon, open, "Enter", true).clicked() {
                self.actions.push(Action::Open(i));
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
            let dir = self.cur_dir.clone();
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

    fn preview(&mut self, ui: &mut Ui) {
        ui.label(egui::RichText::new("미리보기").strong());
        ui.separator();
        let Some(i) = self.focus else {
            ui.weak("미리 볼 파일을 고르세요.");
            return;
        };
        let Some(e) = self.entries.get(i) else { return };
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
                let el = self.focus_time.elapsed();
                if el >= PREVIEW_DELAY {
                    self.request_clip(i);
                } else {
                    ui.ctx().request_repaint_after(PREVIEW_DELAY - el);
                }
            }
            let e = &self.entries[i];
            if let Some((t, k)) = self.clip_frame(&e.path) {
                draw_image(ui, &t, t.size_vec2(), area, 0, None, Vec2::ZERO);
                clip_progress(ui, area, t.size_vec2(), k);
            } else if let Thumb::Ready(t) = &self.thumbs[i] {
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
                self.actions.push(Action::Open(i));
            }
            return;
        }
        let area_h = (ui.available_height() - 90.0).max(60.0);
        let (area, resp) = ui.allocate_exact_size(vec2(ui.available_width(), area_h), Sense::click());
        match (self.big.get(&e.path), &self.thumbs[i]) {
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
                let size = self.focus_dims.map(|d| vec2(d[0] as f32, d[1] as f32)).filter(|_| rot == 0).unwrap_or(t.size_vec2());
                draw_image(ui, t, fit_aspect(size, t.size_vec2()), area, rot, None, Vec2::ZERO);
            }
            _ => {}
        }
        let open = resp.double_clicked();
        ui.add_space(6.0);
        ui.label(egui::RichText::new(&e.name).strong());
        if let Some([w, h]) = self.focus_dims {
            ui.label(format!("{w} x {h}"));
        }
        ui.label(fmt_size(e.size));
        ui.label(fmt_time(e.modified));
        if open {
            self.open_entry(i);
        }
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
            .show(ctx, |ui| self.grid(ui));
        if let Some(i) = self.pending_open.take() {
            self.open_entry(i);
        }

        // load the full picture for the preview once the focus settles
        if let Some(p) = self.focus.and_then(|i| self.entries.get(i)).filter(|e| e.is_img()).map(|e| e.path.clone()) {
            if !self.big.contains_key(&p) {
                let el = self.focus_time.elapsed();
                if el >= PREVIEW_DELAY {
                    self.request_big(&p);
                } else {
                    ctx.request_repaint_after(PREVIEW_DELAY - el);
                }
            }
        }
    }

    // ------------------------------------------------------------ UI: viewer

    fn neighbor_image(&self, from: usize, dir: isize) -> Option<usize> {
        let n = self.entries.len() as isize;
        let mut i = from as isize + dir;
        while i >= 0 && i < n {
            if self.entries[i as usize].is_img() {
                return Some(i as usize);
            }
            i += dir;
        }
        None
    }

    fn close_viewer(&mut self) {
        if let Some(v) = self.viewer.take() {
            self.select_single(v.idx);
        }
        if self.fullscreen {
            self.toggle_fullscreen();
        }
    }

    fn viewer_ui(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.viewer.as_ref().map(|v| v.idx) else { return };
        if idx >= self.entries.len() {
            self.viewer = None;
            return;
        }

        // keys
        if !self.modal_open() {
            let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));
            let mods = ctx.input(|i| i.modifiers);
            let mut go: Option<usize> = None;
            if pressed(Key::Escape) || pressed(Key::Enter) {
                self.close_viewer();
                return;
            }
            if pressed(Key::ArrowRight) || pressed(Key::ArrowDown) || pressed(Key::PageDown) || pressed(Key::Space) {
                go = self.neighbor_image(idx, 1);
            }
            if pressed(Key::ArrowLeft) || pressed(Key::ArrowUp) || pressed(Key::PageUp) || pressed(Key::Backspace) {
                go = self.neighbor_image(idx, -1);
            }
            if pressed(Key::Home) {
                go = (0..self.entries.len()).find(|&i| self.entries[i].is_img());
            }
            if pressed(Key::End) {
                go = (0..self.entries.len()).rev().find(|&i| self.entries[i].is_img());
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
                let p = self.entries[idx].path.clone();
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
                self.viewer = Some(Viewer { idx: g, zoom: None, pan: Vec2::ZERO });
                self.select_single(g);
            }
        }
        let Some(idx) = self.viewer.as_ref().map(|v| v.idx) else { return };

        // current + neighbours
        let cur = self.entries[idx].path.clone();
        for d in [1isize, -1] {
            if let Some(j) = self.neighbor_image(idx, d) {
                let p = self.entries[j].path.clone();
                self.request_big(&p);
            }
        }
        self.request_big(&cur);

        let total = self.entries.iter().filter(|e| e.is_img()).count();
        let pos = self.entries[..=idx].iter().filter(|e| e.is_img()).count();
        let name = self.entries[idx].name.clone();

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(Color32::from_gray(18)))
            .show(ctx, |ui| {
                let area = ui.max_rect();
                let resp = ui.interact(area, Id::new("viewer_area"), Sense::click_and_drag());
                let (zoom_delta, scroll_y, hover) =
                    ctx.input(|i| (i.zoom_delta(), i.raw_scroll_delta.y, i.pointer.hover_pos()));
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
                        if kz.0 || kz.1 {
                            scale = (scale * if kz.0 { 1.25 } else { 0.8 }).clamp(0.02, 40.0);
                            v.zoom = Some(scale);
                        }
                        if resp.dragged() {
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
                        if let Thumb::Ready(t) = &self.thumbs[idx] {
                            let rot = self.disp_rot(&cur, t);
                            let s = t.size_vec2();
                            let big = s * (area.width() / s.x).min(area.height() / s.y) * 0.9;
                            draw_image(ui, t, big, area, rot, None, Vec2::ZERO);
                        }
                        info = format!("{name}   ({pos}/{total})   불러오는 중…");
                    }
                }
                if !info.is_empty() {
                    let p = ui.painter();
                    let r = p.text(area.min + vec2(12.0, 10.0), Align2::LEFT_TOP, &info, FontId::proportional(14.0), Color32::TRANSPARENT);
                    p.rect_filled(r.expand(6.0), 4.0, Color32::from_black_alpha(150));
                    p.text(area.min + vec2(12.0, 10.0), Align2::LEFT_TOP, &info, FontId::proportional(14.0), Color32::WHITE);
                }
                if resp.double_clicked() {
                    self.close_viewer();
                    return;
                }
                if let Some(d) = nav {
                    if let Some(g) = self.neighbor_image(idx, d) {
                        self.viewer = Some(Viewer { idx: g, zoom: None, pan: Vec2::ZERO });
                        self.select_single(g);
                    }
                }
            });
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
        egui::Window::new(title)
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
                Frame::new().fill(C::from_rgb(248, 250, 252))
                    .stroke(Stroke::new(1.0f32, line)).corner_radius(CornerRadius::same(6))
                    .inner_margin(14).show(ui, |ui| {
                        ui.set_width(390.0);
                        ui.horizontal(|ui| {
                            ui.image((self.icons.settings.id(), vec2(16.0, 16.0)));
                            ui.label(RichText::new("썸네일 캐시").size(15.0).strong());
                        });
                        ui.add(egui::Label::new(RichText::new(
                            "목록의 작은 그림을 디스크에 저장해 두고, 폴더를 다시 열 때 바로 보여 줍니다. \
                             그림 100장에 2MB쯤 듭니다. 비워도 원본은 그대로이고 다시 만듭니다."
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
                // program info lives in its own small window
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    let image = egui::Image::new((self.icons.view.id(), vec2(16.0, 16.0)));
                    if ui.add(egui::Button::image_and_text(image, "About")).on_hover_text("프로그램 정보").clicked() {
                        self.show_about = true;
                    }
                });
            });
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
        let enter = ctx.input(|i| i.key_pressed(Key::Enter));
        let esc = ctx.input(|i| i.key_pressed(Key::Escape));
        match dialog {
            // layout and colours from a GPT (Codex) proposal, styled only inside this window
            Dialog::Delete(paths) => {
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
                egui::Window::new(RichText::new("삭제").size(18.0).color(text))
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
                                let prompt = if paths.len() == 1 {
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
                                ui.label(RichText::new("휴지통에서 되살릴 수 있습니다")
                                    .size(12.0).color(muted));
                            });
                        });
                        ui.add_space(8.0);
                        // fixed 30px row: a bare right_to_left layout took the whole window height
                        ui.allocate_ui_with_layout(vec2(ui.available_width(), 30.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let send = egui::Button::new(RichText::new("휴지통으로 보내기").color(danger))
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
                    Some(true) => self.do_delete(paths),
                    Some(false) => {}
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
            last_dir: Some(self.cur_dir.clone()).filter(|d| !d.as_os_str().is_empty()),
            recent: self.recent.clone(),
            sort: self.sort,
            sort_desc: self.sort_desc,
            thumb_size: self.thumb_size,
            favorites: self.favorites.clone(),
            transfer_dirs: self.transfer_dirs.clone(),
            cache_max_mb: self.cache_max_mb,
            confirm_delete: self.confirm_delete,
        };
        eframe::set_value(storage, eframe::APP_KEY, &s);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_msgs();
        // Ctrl+V: egui only reports it when the clipboard holds text
        let v = ctx.input(|i| i.focused) && fileops::ctrl_v_down();
        if v && !self.ctrl_v_prev && self.viewer.is_none() && !self.modal_open() && !ctx.wants_keyboard_input() {
            self.actions.push(Action::Paste(self.cur_dir.clone()));
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
            if let Some(i) = self.entries.iter().position(|e| e.media == Some(loader::Media::Video)) {
                VIDEO_PICKED.store(true, Ordering::Relaxed);
                self.select_single(i);
            }
        }
        // PICVIEW_PLAY_AUDIO=1 plays the first sound file once it is listed (for screenshots)
        static AUDIO_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if std::env::var_os("PICVIEW_PLAY_AUDIO").is_some() && !AUDIO_STARTED.load(Ordering::Relaxed) {
            if let Some(i) = self.entries.iter().position(|e| e.media == Some(loader::Media::Audio)) {
                AUDIO_STARTED.store(true, Ordering::Relaxed);
                self.select_single(i);
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
        if !is_dir && media.is_none() && !loader::is_image(&path) {
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
