# Multi-pane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the central thumbnail grid into 1 → 4 panes, each showing its own folder, with drag-and-drop between panes for sorting photos.

**Architecture:** Per-folder state moves out of `App` into a `Pane` struct in a new `src/pane.rs`; `App` holds `panes: Vec<Pane>` and `active: usize`. Shared state (thumbnail size, sort, kind filter, caches, dialogs, viewer, settings) stays in `App`. Methods that act on "the folder" take a pane index or use `self.active`.

**Tech Stack:** Rust 2021, eframe/egui 0.31, existing crates only.

**Spec:** `docs/superpowers/specs/2026-11-15-multi-pane-design.md`

## Global Constraints

- No new crates.
- With 1 pane the screen must look exactly as today: no pane header, no pane border.
- Pane layouts: 1 = full, 2 = left/right, 3 = three columns, 4 = 2x2. Panes are equal size.
- Pane header 24 px: folder name, back (◀), up (▲). Active pane border: blue `Color32::from_rgb(40, 120, 220)` 2 px; others grey `from_gray(215)` 1 px.
- Thumbnail size, sort and kind filter are shared by all panes.
- Activation: left click, right click, or drag start inside a pane. Wheel scrolling does not activate. Tab / Shift+Tab cycle panes (not while a text field has focus).
- Drop rule: on a folder cell → that subfolder; on ".." → that pane's parent; anywhere else in another pane → that pane's folder; own pane blank area → nothing. Move/copy rule unchanged (`drop_is_move`).
- Thumbnails kept in memory: 800 total across all panes (`MAX_THUMBS_LOADED`).
- Saved: pane count, each pane's folder, active pane. A start-up path argument opens in the active pane only.
- Missing saved folder → nearest existing ancestor; none → the active pane's folder. Closing the active pane → pane 0 becomes active.
- `PICVIEW_PANES=2|3|4` env switch opens that many panes on the start folder (screenshots).
- Build while PicView is running: rename `target/release/oddity-picview.exe` (and its hard-linked `target/release/build/oddity-picview/*/out/oddity_picview.exe`) to `.old` first, or build with `--target-dir target/alt`.
- Commits: author `13573570+destory1984@users.noreply.github.com`, message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Never push.
- Screenshots only via input-free capture (`bench/capture.py`, posted window messages). Never move the user's mouse.

## Review Focus

1. Pane count reduced while thumbnail jobs for the removed pane are in flight → messages are dropped, no index panic. (Task 2 test `route_drops_closed_pane`.)
2. Viewer opened from pane 2, then pane count set to 1 → viewer closes instead of indexing pane 0 with pane 2's index. (Task 3 test `shrinking_closes_viewer_of_removed_pane`.)
3. Two panes show the same folder; a file is moved or deleted from one → both panes reload. (Task 4 test `panes_showing_finds_every_pane`.)
4. Dragging onto the blank area of the pane the files came from → no transfer. (Task 4 test `drop_on_own_folder_is_none`.)
5. A saved pane folder was deleted or its drive unplugged → that pane opens an existing ancestor, never fails to start. (Task 5 test `restore_falls_back_to_ancestor`.)

---

### Task 1: `pane.rs` with layout helpers

**Files:**
- Create: `src/pane.rs`
- Modify: `src/main.rs` (move `GridRow`, `GridLayout`, `cell_of`, `FOLDER_THUMB`, `SECTION_GAP`, `GAP` users import from `pane`)

**Interfaces:**
- Produces: `pub fn pane_rects(area: Rect, n: usize) -> Vec<Rect>`; `pub struct GridRow`, `pub struct GridLayout` (moved unchanged, with `row_of`, `step_rows`); `pub fn cell_of(ts: f32) -> Vec2`; `pub const GAP: f32 = 8.0`, `FOLDER_THUMB`, `SECTION_GAP`.

- [ ] **Step 1: Write the failing tests in `src/pane.rs`**

```rust
#[test] fn one_pane_is_whole_area()   { assert_eq!(pane_rects(r(0.,0.,1000.,600.), 1), vec![r(0.,0.,1000.,600.)]); }
#[test] fn two_panes_split_left_right() { let v = pane_rects(r(0.,0.,1000.,600.), 2); assert_eq!(v.len(), 2); assert!(v[0].max.x <= v[1].min.x); assert_eq!(v[0].height(), 600.); assert!((v[0].width() - v[1].width()).abs() < 0.5); }
#[test] fn three_panes_are_columns()  { let v = pane_rects(r(0.,0.,900.,600.), 3); assert!(v.iter().all(|p| p.height() == 600.)); assert!(v[0].max.x <= v[1].min.x && v[1].max.x <= v[2].min.x); }
#[test] fn four_panes_are_2x2()       { let v = pane_rects(r(0.,0.,1000.,600.), 4); assert_eq!(v[0].min.y, v[1].min.y); assert!(v[0].max.y <= v[2].min.y); assert_eq!(v[0].min.x, v[2].min.x); }
#[test] fn pane_count_is_clamped()    { assert_eq!(pane_rects(r(0.,0.,100.,100.), 0).len(), 1); assert_eq!(pane_rects(r(0.,0.,100.,100.), 9).len(), 4); }
```
(`r(x0,y0,x1,y1)` = `Rect::from_min_max(pos2(x0,y0), pos2(x1,y1))`.) Panes are separated by a 6 px gap.

- [ ] **Step 2: Run** `cargo test pane::` — Expected: FAIL (`pane_rects` not defined).
- [ ] **Step 3: Implement `pane_rects`, move the grid layout types into `pane.rs`, add `mod pane;` and `use pane::*` in `main.rs`.**
- [ ] **Step 4: Run** `cargo test` — Expected: all tests pass (existing cache/fileops/loader/rotate tests too), `cargo build --release --target-dir target/alt` has no warnings.
- [ ] **Step 5: Commit** `git add src/pane.rs src/main.rs && git commit` ("pane.rs: pane rectangles, grid layout moved").

### Task 2: Move per-folder state into `Pane` (still one pane)

**Files:**
- Modify: `src/pane.rs`, `src/main.rs` (every use of the fields below)

**Interfaces:**
- Consumes: Task 1 types.
- Produces:
  - `pub struct Pane { cur_dir, addr_text, entries: Vec<Entry>, thumbs: Vec<Thumb>, thumbs_loaded, gen: Arc<AtomicU64>, vis_start, vis_end, selected, anchor, focus, focus_time, focus_dims, back, fwd, grid_w, grid_offset, grid_view_h, grid_scroll_to, wheel_acc, scroll_anim, kind_counts, pending_open, clip_hover }` (types as in `App` today; `Entry`, `Thumb` move to `pane.rs` as `pub`).
  - `impl Pane { pub fn new() -> Pane; pub fn index_of(&self, p: &Path) -> Option<usize>; pub fn slot_off(&self) -> usize; pub fn selected_paths(&self) -> Vec<PathBuf>; pub fn layout(&self, w: f32, thumb_size: f32) -> GridLayout }`
  - `App { panes: Vec<Pane>, active: usize, .. }` with `fn pane(&self) -> &Pane` and `fn pane_mut(&mut self) -> &mut Pane` returning the active pane.
  - `Msg::Thumb { pane: usize, gen, idx, mtime, tex }`, `Msg::ThumbFail { pane, gen, idx }`, `Msg::Skip { pane, gen, idx }`.
  - `pub fn route_ok(panes: &[Pane], pane: usize, gen: u64, idx: usize) -> bool` — true only when `pane < panes.len()`, `panes[pane].gen` equals `gen`, and `idx < panes[pane].thumbs.len()`.
  - `Viewer { pane: usize, idx, zoom, pan }`; `Action::Open(pane, idx)`.
  - Shared settings apply to every pane: `set_kind_filter` reloads all panes; a sort change re-sorts all panes; `evict_thumbs` keeps at most 800 loaded thumbnails in total, dropping those farthest from each pane's visible range first.
  - Grid drawing takes the pane: `fn grid(&mut self, ui: &mut Ui, pi: usize)`, `fn cell(&mut self, ui, pi, i, rect)`, `fn up_cell(&mut self, ui, pi, rect)`, `fn wheel_rows(&mut self, ui, pi, lay)`, `fn request_thumb(&mut self, pi, i)`, `fn blank_area(&mut self, ui, pi, rect)`. Everything else (keys, toolbar, preview, tree, address bar, status bar, file ops) uses the active pane.

- [ ] **Step 1: Write the failing tests in `src/pane.rs`**

```rust
#[test] fn route_accepts_current_gen()   { let p = pane_with(3, 7); assert!(route_ok(&[p], 0, 7, 2)); }
#[test] fn route_rejects_old_gen()       { let p = pane_with(3, 7); assert!(!route_ok(&[p], 0, 6, 0)); }
#[test] fn route_drops_closed_pane()     { let p = pane_with(3, 7); assert!(!route_ok(&[p], 1, 7, 0)); }
#[test] fn route_rejects_index_past_end(){ let p = pane_with(3, 7); assert!(!route_ok(&[p], 0, 7, 3)); }
```
(`pane_with(n, gen)` = `Pane::new()` with `n` `Thumb::None` slots and `gen` stored.)

- [ ] **Step 2: Run** `cargo test pane::` — Expected: FAIL.
- [ ] **Step 3: Implement `Pane`, `route_ok`, and move the fields.** `App::new` creates `panes: vec![Pane::new()]`, `active: 0`, then navigates as today. `poll_msgs` uses `route_ok` for the three thumbnail messages. Behaviour must not change.
- [ ] **Step 4: Run** `cargo test` — Expected: PASS. Then capture before/after with the same inputs and compare:
  `python bench/capture.py target/alt/release/oddity-picview.exe <scratch>/gridtest <scratch>/t2.png` (and with `PICVIEW_SELECT_VIDEO=1` on a video folder, `PICVIEW_FILTER=video` on a mixed folder, `PICVIEW_SHOW_SETTINGS=1`). Expected: same pictures as the build before this task. Also post a right-click on the blank area (as in the earlier `rclick_cap.py`) — the folder menu opens.
- [ ] **Step 5: Commit** ("Move per-folder state into Pane").

### Task 3: Draw 1 → 4 panes, activation, pane count buttons

**Files:**
- Modify: `src/main.rs` (`browser_ui`, `toolbar`, `grid_keys`, new `pane_frame`), `src/pane.rs`

**Interfaces:**
- Consumes: `pane_rects`, `Pane`, grid methods taking `pi`.
- Produces:
  - `fn set_pane_count(&mut self, n: usize)` — clamps to 1..=4; new panes open the active pane's folder; removing panes drops them from the end; if `active >= n` then `active = 0`; if the viewer's `pane >= n` the viewer closes.
  - `fn activate(&mut self, pi: usize)` — sets `active`, stops the audio player if the pane changed, points `addr_text` / tree at that pane's folder.
  - Toolbar control "칸 1 2 3 4" after the kind filter buttons, same chip style as the filter (selected = blue fill, white text), tooltip "칸 N개로 나누기".
  - `fn pane_header(&mut self, ui, pi, rect)` — folder name, ◀ (`go_back` on that pane, disabled when its `back` is empty), ▲ (`go_up`), click activates.

- [ ] **Step 1: Write the failing test** (in `src/pane.rs`, on a plain helper so it needs no egui context):

```rust
#[test] fn shrinking_closes_viewer_of_removed_pane() {
    let r = shrink_panes(PaneCountChange { active: 2, viewer_pane: Some(2), n: 1 });
    assert_eq!(r.active, 0); assert!(r.close_viewer);
}
#[test] fn shrinking_keeps_viewer_of_kept_pane() {
    let r = shrink_panes(PaneCountChange { active: 1, viewer_pane: Some(0), n: 2 });
    assert_eq!(r.active, 1); assert!(!r.close_viewer);
}
```
(`pub struct PaneCountChange { pub active: usize, pub viewer_pane: Option<usize>, pub n: usize }`, `pub struct PaneCountResult { pub active: usize, pub close_viewer: bool }`, `pub fn shrink_panes(c: PaneCountChange) -> PaneCountResult`. `set_pane_count` uses it.)

- [ ] **Step 2: Run** `cargo test pane::` — Expected: FAIL.
- [ ] **Step 3: Implement** `shrink_panes`, `set_pane_count`, `activate`, the toolbar control, and `browser_ui`'s central panel: split with `pane_rects`; for each pane draw header (only when n > 1), border (only when n > 1), then `grid(ui, pi)` inside a child ui clipped to the pane rect. Activation hooks: in `cell`, `up_cell`, `blank_area` and the header, call `activate(pi)` on `clicked() || secondary_clicked() || drag_started()`. `grid_keys`: Tab / Shift+Tab call `activate((active ± 1) mod n)` unless `ctx.wants_keyboard_input()`. Mouse back/forward buttons act on the active pane. Add `PICVIEW_PANES` env switch (once, at the first frame).
- [ ] **Step 4: Run** `cargo test` (PASS), then captures: `PICVIEW_PANES=2`, `3`, `4` on `<scratch>/gridtest` at 1200x800. Expected: 2 / 3 / 4 bordered panes with headers, pane 0 border blue, thumbnails drawn in every pane. Capture with no env: identical to Task 2's picture (no header, no border). Post a left click (WM_MOUSEMOVE + WM_LBUTTONDOWN/UP) on pane 1's blank area with `PICVIEW_PANES=2`: pane 1 gets the blue border and the address bar shows its folder.
- [ ] **Step 5: Commit** ("Split the list into 1-4 panes").

### Task 4: Drag and drop between panes

**Files:**
- Modify: `src/main.rs` (`drop_target`, `blank_area`, `pane_header`, `drag_overlay`, `refresh_dir`, `run` for `Action::Transfer` / `OpDone`), `src/pane.rs`

**Interfaces:**
- Consumes: Task 3 panes.
- Produces:
  - `pub fn drop_dest(pane_dir: &Path, dragged: &[PathBuf]) -> Option<PathBuf>` — `None` when every dragged file's parent is `pane_dir` (own folder) or `pane_dir` is inside a dragged folder; else `Some(pane_dir)`.
  - `pub fn panes_showing(panes: &[Pane], dir: &Path) -> Vec<usize>`.
  - A pane-wide drop target registered on the pane rect *before* the cells (so folder cells, drawn later, win). Hovering paints the whole pane border blue 2 px; folder cells keep their own blue outline and the pane border is not drawn while a folder cell is the target (`dnd_target` set by the cell).
  - `refresh_dir(d)` reloads every pane in `panes_showing(&self.panes, d)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn drop_on_own_folder_is_none()    { assert_eq!(drop_dest(p("C:/a"), &[pb("C:/a/1.jpg")]), None); }
#[test] fn drop_on_other_folder()          { assert_eq!(drop_dest(p("C:/b"), &[pb("C:/a/1.jpg")]), Some(pb("C:/b"))); }
#[test] fn drop_folder_into_itself_is_none(){ assert_eq!(drop_dest(p("C:/a/sub"), &[pb("C:/a")]), None); }
#[test] fn panes_showing_finds_every_pane() { /* panes with cur_dir a, b, a → panes_showing(a) == vec![0, 2] */ }
```

- [ ] **Step 2: Run** `cargo test pane::` — Expected: FAIL.
- [ ] **Step 3: Implement** `drop_dest`, `panes_showing`, the pane-wide target, overlay text unchanged ("3개 → 정리됨 (이동)"), and multi-pane refresh. Transfers go through the existing `Action::Transfer(files, dest, mv)`.
- [ ] **Step 4: Run** `cargo test` (PASS) and the Task 3 captures again (unchanged). A real drag needs the user's mouse: do not simulate it; say in the report that dragging between panes was checked by unit tests only.
- [ ] **Step 5: Commit** ("Drag files between panes").

### Task 5: Remember panes, start-up path, docs

**Files:**
- Modify: `src/main.rs` (`Settings`, `App::new`, `save`), `src/pane.rs`, `README.md`, `CHANGELOG.md`

**Interfaces:**
- Consumes: `set_pane_count`, `activate`.
- Produces:
  - `Settings { pane_dirs: Vec<PathBuf>, pane_count: usize, active_pane: usize, .. }` (serde default: empty, 1, 0; old settings files load as 1 pane on `last_dir`).
  - `pub fn restore_dir(saved: &Path, fallback: &Path) -> PathBuf` — `saved` if it is a dir, else its nearest existing ancestor, else `fallback`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn restore_keeps_existing_dir()      { let t = temp_dir(); assert_eq!(restore_dir(&t, Path::new("C:/")), t); }
#[test] fn restore_falls_back_to_ancestor()  { let t = temp_dir(); assert_eq!(restore_dir(&t.join("gone/deeper"), Path::new("C:/")), t); }
#[test] fn restore_uses_fallback_when_none() { assert_eq!(restore_dir(Path::new("Q:/no/such"), Path::new("C:/")), pb("C:/")); }
```
(`temp_dir()` = `std::env::temp_dir()` joined with a unique name, created.)

- [ ] **Step 2: Run** `cargo test pane::` — Expected: FAIL.
- [ ] **Step 3: Implement** save/restore. Start-up path argument replaces only the active pane's folder. README: new section "여러 칸으로 보기" (layouts, header, activation, Tab, drop rules, remembered state) and Tab in the key table; "기억하는 것" gains pane count / folders. CHANGELOG 0.0.11 gains one entry. Plain "다" style, numbers, no tilde.
- [ ] **Step 4: Run** `cargo test` (PASS); capture with a data dir whose saved state has 2 panes on two test folders → both restored; start with a path argument → only the active pane changes.
- [ ] **Step 5: Commit** ("Remember panes; README for panes").

### Task 6: Whole check

- [ ] **Step 1:** Re-run every capture from Tasks 2-5 plus: audio preview (`PICVIEW_PLAY_AUDIO=1`), video preview (`PICVIEW_SELECT_VIDEO=1`), settings window, kind filter, right-click menu on blank area in pane 1 of 2. Expected: all as before; nothing appears in the wrong pane.
- [ ] **Step 2:** Build `target/release` (rename trick) and report to the user what was machine-checked and what needs their mouse (drag between panes, hover video preview in a side pane).
