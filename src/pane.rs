use eframe::egui::{vec2, Rect, TextureHandle, Vec2};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use crate::loader;

pub const GAP: f32 = 8.0;
/// Above this thumbnail size folders get their own block of small tiles.
pub const FOLDER_THUMB: f32 = 96.0;
/// Extra space (with a thin line) between the folder block and the files.
pub const SECTION_GAP: f32 = 14.0;

/// One row of the thumbnail grid. Slots count the ".." cell first.
#[derive(Clone, Copy)]
pub struct GridRow {
    pub top: f32,
    pub first: usize,
    pub count: usize,
    pub cell: Vec2,
}

pub struct GridLayout {
    pub rows: Vec<GridRow>,
    pub total_h: f32,
    /// y of the line between folders and files
    pub split_y: Option<f32>,
}

impl GridLayout {
    pub fn row_of(&self, slot: usize) -> Option<usize> {
        self.rows.iter().position(|r| slot >= r.first && slot < r.first + r.count)
    }

    /// Slot `k` rows up (-) or down (+) from `slot`, keeping the same x where possible.
    pub fn step_rows(&self, slot: usize, k: isize) -> Option<usize> {
        let r = self.row_of(slot)?;
        let row = self.rows[r];
        let x = (slot - row.first) as f32 * (row.cell.x + GAP) + row.cell.x / 2.0;
        let nr = (r as isize + k).clamp(0, self.rows.len() as isize - 1) as usize;
        let to = self.rows[nr];
        let c = ((x / (to.cell.x + GAP)).floor().max(0.0) as usize).min(to.count - 1);
        Some(to.first + c)
    }
}

pub fn cell_of(ts: f32) -> Vec2 {
    vec2(ts + 16.0, ts + 16.0 + 22.0)
}
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum Split {
    #[default]
    One,
    LeftRight,
    TopBottom,
}

impl Split {
    pub fn len(self) -> usize {
        match self {
            Self::One => 1,
            Self::LeftRight | Self::TopBottom => 2,
        }
    }
}

pub fn pane_rects(area: Rect, split: Split) -> Vec<Rect> {
    let gap = 6.0;
    let (size, step) = match split {
        Split::One => return vec![area],
        Split::LeftRight => {
            let w = (area.width() - gap) / 2.0;
            (vec2(w, area.height()), vec2(w + gap, 0.0))
        }
        Split::TopBottom => {
            let h = (area.height() - gap) / 2.0;
            (vec2(area.width(), h), vec2(0.0, h + gap))
        }
    };
    vec![
        Rect::from_min_size(area.min, size),
        Rect::from_min_size(area.min + step, size),
    ]
}
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    /// video / audio: shell thumbnail, opened in the default program
    pub media: Option<loader::Media>,
    pub size: u64,
    pub modified: SystemTime,
}

impl Entry {
    pub fn is_img(&self) -> bool {
        !self.is_dir && self.media.is_none()
    }
}

pub enum Thumb {
    None,
    Pending,
    Ready(TextureHandle),
    Failed,
}

pub struct Pane {
    pub cur_dir: PathBuf,
    pub addr_text: String,
    pub entries: Vec<Entry>,
    pub thumbs: Vec<Thumb>,
    pub thumbs_loaded: usize,
    pub gen: Arc<AtomicU64>,
    pub vis_start: Arc<AtomicUsize>,
    pub vis_end: Arc<AtomicUsize>,
    pub selected: BTreeSet<usize>,
    pub anchor: Option<usize>,
    pub focus: Option<usize>,
    pub focus_time: Instant,
    pub focus_dims: Option<[u32; 2]>,
    pub back: Vec<PathBuf>,
    pub fwd: Vec<PathBuf>,
    /// Grid width from the last frame.
    pub grid_w: f32,
    pub grid_offset: f32,
    pub grid_view_h: f32,
    pub grid_scroll_to: Option<f32>,
    pub wheel_acc: f32,
    pub scroll_anim: Option<(f32, f32, Instant)>, // from, to, start
    /// File counts before filtering: all, image, video, audio.
    pub kind_counts: [usize; 4],
    pub pending_open: Option<usize>,
    /// Video thumbnail under the pointer and since when.
    pub clip_hover: Option<(usize, Instant)>,
}

impl Pane {
    pub fn new() -> Pane {
        Pane {
            cur_dir: PathBuf::new(),
            addr_text: String::new(),
            entries: Vec::new(),
            thumbs: Vec::new(),
            thumbs_loaded: 0,
            gen: Arc::new(AtomicU64::new(0)),
            vis_start: Arc::new(AtomicUsize::new(0)),
            vis_end: Arc::new(AtomicUsize::new(0)),
            selected: BTreeSet::new(),
            anchor: None,
            focus: None,
            focus_time: Instant::now(),
            focus_dims: None,
            back: Vec::new(),
            fwd: Vec::new(),
            grid_w: 800.0,
            grid_offset: 0.0,
            grid_view_h: 600.0,
            grid_scroll_to: None,
            wheel_acc: 0.0,
            scroll_anim: None,
            kind_counts: [0; 4],
            pending_open: None,
            clip_hover: None,
        }
    }

    pub fn index_of(&self, p: &Path) -> Option<usize> {
        self.entries.iter().position(|e| e.path == p)
    }

    /// 1 when the grid starts with the ".." (parent folder) cell.
    pub fn slot_off(&self) -> usize {
        self.cur_dir.parent().is_some() as usize
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.selected.iter().filter_map(|&i| self.entries.get(i)).map(|e| e.path.clone()).collect();
        if v.is_empty() {
            if let Some(e) = self.focus.and_then(|i| self.entries.get(i)) {
                v.push(e.path.clone());
            }
        }
        v
    }

    /// Rows of the grid for width `w`. Folders come first (sorting keeps them
    /// there); with big thumbnails they get small tiles in a block of their own.
    pub fn layout(&self, w: f32, thumb_size: f32) -> GridLayout {
        let off = self.slot_off();
        let total = self.entries.len() + off;
        let ndirs = self.entries.iter().take_while(|e| e.is_dir).count();
        let nfs = if thumb_size > FOLDER_THUMB { off + ndirs } else { 0 };
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
        section(&mut rows, &mut y, nfs, total, cell_of(thumb_size));
        GridLayout { rows, total_h: (y - GAP).max(0.0), split_y }
    }
}

pub fn route_ok(panes: &[Pane], pane: usize, gen: u64, idx: usize) -> bool {
    panes.get(pane).is_some_and(|p| p.gen.load(Ordering::SeqCst) == gen && idx < p.thumbs.len())
}

pub struct PaneCountChange {
    pub active: usize,
    pub viewer_pane: Option<usize>,
    pub n: usize,
}

pub struct PaneCountResult {
    pub active: usize,
    pub close_viewer: bool,
}

pub fn shrink_panes(c: PaneCountChange) -> PaneCountResult {
    let n = c.n.clamp(1, 2);
    PaneCountResult {
        active: if c.active < n { c.active } else { 0 },
        close_viewer: c.viewer_pane.is_some_and(|pi| pi >= n),
    }
}

pub fn drop_dest(pane_dir: &Path, dragged: &[PathBuf]) -> Option<PathBuf> {
    if dragged.iter().all(|f| f.parent() == Some(pane_dir))
        || dragged.iter().any(|f| pane_dir.starts_with(f)) {
        None
    } else {
        Some(pane_dir.to_path_buf())
    }
}

pub fn panes_showing(panes: &[Pane], dir: &Path) -> Vec<usize> {
    panes.iter().enumerate().filter_map(|(pi, p)| (p.cur_dir == dir).then_some(pi)).collect()
}

pub fn restore_dir(saved: &Path, fallback: &Path) -> PathBuf {
    saved.ancestors().find(|p| p.is_dir()).unwrap_or(fallback).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{self, pos2};
    fn r(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect { Rect::from_min_max(pos2(x0, y0), pos2(x1, y1)) }

    #[test] fn one_is_whole_area() { assert_eq!(pane_rects(r(0.,0.,1000.,600.), Split::One), vec![r(0.,0.,1000.,600.)]); }
    #[test] fn left_right_splits_width() {
        let v = pane_rects(r(0.,0.,1000.,600.), Split::LeftRight);
        assert_eq!(v.len(), 2);
        assert!(v[0].max.x <= v[1].min.x);
        assert!(v.iter().all(|p| p.height() == 600.));
        assert!((v[0].width() - v[1].width()).abs() < 0.5);
        assert_eq!(v[1].min.x - v[0].max.x, 6.);
    }
    #[test] fn top_bottom_splits_height() {
        let v = pane_rects(r(0.,0.,1000.,600.), Split::TopBottom);
        assert_eq!(v.len(), 2);
        assert!(v[0].max.y <= v[1].min.y);
        assert!(v.iter().all(|p| p.width() == 1000.));
        assert!((v[0].height() - v[1].height()).abs() < 0.5);
        assert_eq!(v[1].min.y - v[0].max.y, 6.);
    }
    #[test] fn switching_direction_keeps_two_panes() {
        assert_eq!(pane_rects(r(0.,0.,1000.,600.), Split::LeftRight).len(), 2);
        assert_eq!(pane_rects(r(0.,0.,1000.,600.), Split::TopBottom).len(), 2);
    }
    fn pane_with(n: usize, gen: u64) -> Pane {
        let mut p = Pane::new();
        p.thumbs = (0..n).map(|_| Thumb::None).collect();
        p.gen.store(gen, Ordering::SeqCst);
        p
    }

    #[test] fn route_accepts_current_gen()   { let p = pane_with(3, 7); assert!(route_ok(&[p], 0, 7, 2)); }
    #[test] fn route_rejects_old_gen()       { let p = pane_with(3, 7); assert!(!route_ok(&[p], 0, 6, 0)); }
    #[test] fn route_drops_closed_pane()     { let p = pane_with(3, 7); assert!(!route_ok(&[p], 1, 7, 0)); }
    #[test] fn route_rejects_index_past_end(){ let p = pane_with(3, 7); assert!(!route_ok(&[p], 0, 7, 3)); }
    #[test] fn shrinking_closes_viewer_of_removed_pane() {
        let r = shrink_panes(PaneCountChange { active: 1, viewer_pane: Some(1), n: 1 });
        assert_eq!(r.active, 0); assert!(r.close_viewer);
    }
    #[test] fn shrinking_keeps_viewer_of_kept_pane() {
        let r = shrink_panes(PaneCountChange { active: 1, viewer_pane: Some(0), n: 2 });
        assert_eq!(r.active, 1); assert!(!r.close_viewer);
    }
    fn p(s: &str) -> &Path { Path::new(s) }
    fn pb(s: &str) -> PathBuf { PathBuf::from(s) }
    #[test] fn drop_on_own_folder_is_none() { assert_eq!(drop_dest(p("C:/a"), &[pb("C:/a/1.jpg")]), None); }
    #[test] fn drop_on_other_folder() { assert_eq!(drop_dest(p("C:/b"), &[pb("C:/a/1.jpg")]), Some(pb("C:/b"))); }
    #[test] fn drop_folder_into_itself_is_none() { assert_eq!(drop_dest(p("C:/a/sub"), &[pb("C:/a")]), None); }
    #[test] fn panes_showing_finds_every_pane() {
        let mut panes = vec![Pane::new(), Pane::new()];
        for (pane, dir) in panes.iter_mut().zip(["C:/a", "C:/a"]) { pane.cur_dir = pb(dir); }
        assert_eq!(panes_showing(&panes, p("C:/a")), vec![0, 1]);
    }

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let t = std::env::temp_dir().join(format!("picview-restore-{}-{}-{}", std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&t).unwrap();
        t
    }
    #[test] fn restore_keeps_existing_dir() {
        let t = temp_dir();
        assert_eq!(restore_dir(&t, Path::new("C:/")), t);
        std::fs::remove_dir(&t).unwrap();
    }
    #[test] fn restore_falls_back_to_ancestor() {
        let t = temp_dir();
        assert_eq!(restore_dir(&t.join("gone/deeper"), Path::new("C:/")), t);
        std::fs::remove_dir(&t).unwrap();
    }
    #[test] fn restore_uses_fallback_when_none() {
        assert_eq!(restore_dir(Path::new("Q:/no/such"), Path::new("C:/")), pb("C:/"));
    }

    fn test_app(dir: &Path) -> crate::App {
        use crate::*;
        let ctx = egui::Context::default();
        let saved = Settings::default();
        let drives = Vec::new();
        let (tx, rx) = mpsc::channel();
        let mut app = App {
            panes: vec![Pane::new()],
            split: Split::One,
            active: 0,
            ctx: ctx.clone(),
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
            dialog: None,
            status: String::new(),
            drives,
            tree_children: HashMap::new(),
            tree_scroll_pending: false,
            favorites: saved.favorites.clone(),
            actions: Vec::new(),
            cut_paths: HashSet::new(),
            ctrl_v_prev: false,
            rot_saving: HashMap::new(),
            baked: HashMap::new(),
            icons: Arc::new(icons::Icons::new(&ctx)),
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


        app.navigate(0, dir.to_path_buf(), false);
        app.set_split(Split::LeftRight);
        app
    }

    #[test] fn changing_split_keeps_pane_state() {
        let t = temp_dir();
        std::fs::create_dir(t.join("other")).unwrap();
        let mut app = test_app(&t);
        app.navigate(1, t.join("other"), false);
        app.activate(1);
        for (pi, pane) in app.panes.iter_mut().enumerate() {
            pane.selected.insert(pi);
            pane.grid_offset = 100.0 + pi as f32;
        }
        let generations: Vec<_> = app.panes.iter().map(|p| p.gen.load(Ordering::SeqCst)).collect();
        for split in [Split::TopBottom, Split::LeftRight] {
            app.set_split(split);
            assert_eq!(app.panes.len(), 2);
            assert_eq!(app.active, 1);
            assert_eq!(app.panes[0].cur_dir, t);
            assert_eq!(app.panes[1].cur_dir, t.join("other"));
            for (pi, pane) in app.panes.iter().enumerate() {
                assert_eq!(pane.selected, BTreeSet::from([pi]));
                assert_eq!(pane.grid_offset, 100.0 + pi as f32);
                assert_eq!(pane.gen.load(Ordering::SeqCst), generations[pi]);
            }
        }
        std::fs::remove_dir_all(&t).unwrap();
    }

    #[test] fn one_closes_second_pane_and_reopening_uses_active_folder() {
        let t = temp_dir();
        let mut app = test_app(&t);
        let generation = app.panes[1].gen.clone();
        let before = generation.load(Ordering::SeqCst);
        app.activate(1);
        app.viewer = Some(crate::Viewer { pane: 1, idx: 0, zoom: None, pan: Vec2::ZERO });
        app.set_split(Split::One);
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.active, 0);
        assert!(app.viewer.is_none());
        assert_ne!(generation.load(Ordering::SeqCst), before);
        app.set_split(Split::TopBottom);
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.panes[1].cur_dir, t);
        assert_ne!(app.panes[1].gen.load(Ordering::SeqCst), before);
        std::fs::remove_dir_all(&t).unwrap();
    }

    #[test] fn toolbar_fits_1200_pixels_without_a_window() {
        let t = temp_dir();
        let mut app = test_app(&t);
        let ctx = app.ctx.clone();
        crate::setup_fonts(&ctx);
        let _ = ctx.run(egui::RawInput {
            screen_rect: Some(r(0., 0., 1200., 800.)),
            ..Default::default()
        }, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                app.toolbar(ui);
                assert!(ui.min_rect().max.x <= 1200., "toolbar right edge: {}", ui.min_rect().max.x);
            });
        });
        std::fs::remove_dir_all(&t).unwrap();
    }
    #[test] fn new_folder_updates_every_matching_pane() {
        let t = temp_dir();
        let mut app = test_app(&t);
        app.do_new_folder(t.clone(), "child");
        let seen: Vec<bool> = app.panes.iter().map(|p| p.index_of(&t.join("child")).is_some()).collect();
        std::fs::remove_dir_all(&t).unwrap();
        assert_eq!(seen, vec![true, true]);
    }

    #[test] fn rename_updates_every_matching_pane() {
        let t = temp_dir();
        std::fs::create_dir(t.join("old")).unwrap();
        let mut app = test_app(&t);
        app.do_rename(t.join("old"), "new");
        let seen: Vec<bool> = app.panes.iter().map(|p| p.index_of(&t.join("new")).is_some()).collect();
        std::fs::remove_dir_all(&t).unwrap();
        assert_eq!(seen, vec![true, true]);
    }

    #[test] fn rotation_updates_every_loaded_thumbnail() {
        use crate::*;
        let t = temp_dir();
        let path = t.join("image.png");
        image::RgbaImage::new(2, 3).save(&path).unwrap();
        let mut app = test_app(&t);
        let mut ids = Vec::new();
        for pi in 0..2 {
            let tex = app.ctx.load_texture(format!("rotation-{pi}"), ColorImage::new([2, 3], Color32::WHITE), tex_opts());
            ids.push(tex.id());
            app.panes[pi].thumbs[0] = Thumb::Ready(tex);
        }
        app.rot.insert(path.clone(), 1);
        app.on_rotated(path, 1, Ok(()));
        let turns: Vec<_> = ids.iter().map(|id| app.baked.get(id).copied()).collect();
        std::fs::remove_dir_all(&t).unwrap();
        assert_eq!(turns, vec![Some(1), Some(1)]);
    }

}
