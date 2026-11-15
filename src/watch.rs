use crate::Msg;
use eframe::egui;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const QUIET: Duration = Duration::from_millis(300);
const MAX_WAIT: Duration = Duration::from_millis(1500);

pub struct DirWatch {
    watcher: RecommendedWatcher,
    dirs: Arc<Mutex<Vec<PathBuf>>>,
    watched: HashSet<PathBuf>,
    invalidated: Arc<Mutex<HashSet<PathBuf>>>,
}

fn path_key(path: &Path) -> PathBuf {
    let s = path
        .to_string_lossy()
        .replace('/', &char::from(92).to_string())
        .to_lowercase();
    let s = s
        .strip_prefix(r"\\?\unc\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned());
    PathBuf::from(s)
}

impl DirWatch {
    pub fn new(tx: mpsc::Sender<Msg>, ctx: egui::Context) -> Option<Self> {
        let dirs = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let targets = dirs.clone();
        let invalidated = Arc::new(Mutex::new(HashSet::new()));
        let changed_watches = invalidated.clone();
        let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let Ok(event) = result else { return };
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            let structural = matches!(
                event.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
            );
            let Ok(dirs) = targets.lock() else { return };
            for dir in dirs.iter() {
                let key = path_key(dir);
                let affected = event.need_rescan()
                    || event.paths.iter().any(|path| {
                        let path = path_key(path);
                        if structural && key.starts_with(&path) {
                            // Windows can silently drop a removed directory's watch.
                            if let Ok(mut changed) = changed_watches.lock() {
                                changed.insert(path);
                            }
                            return true;
                        }
                        path.parent() == Some(key.as_path()) && (structural || !path.is_dir())
                    });
                if affected {
                    let _ = tx.send(Msg::DirChanged { dir: dir.clone() });
                    ctx.request_repaint();
                }
            }
        })
        .ok()?;
        Some(Self {
            watcher,
            dirs,
            watched: HashSet::new(),
            invalidated,
        })
    }

    pub fn set_dirs(&mut self, dirs: &[PathBuf]) {
        let mut seen = HashSet::new();
        let dirs: Vec<_> = dirs
            .iter()
            .filter(|d| !d.as_os_str().is_empty() && seen.insert(path_key(d)))
            .cloned()
            .collect();
        let wanted: HashSet<_> = dirs
            .iter()
            .flat_map(|d| d.ancestors().map(Path::to_path_buf))
            .collect();
        if let Ok(mut targets) = self.dirs.lock() {
            *targets = dirs;
        }
        let invalidated = self
            .invalidated
            .lock()
            .map(|mut changed| std::mem::take(&mut *changed))
            .unwrap_or_default();
        let removed: Vec<_> = self
            .watched
            .iter()
            .filter(|dir| {
                !wanted.contains(*dir)
                    || invalidated
                        .iter()
                        .any(|path| path_key(dir).starts_with(path))
            })
            .cloned()
            .collect();
        for dir in removed {
            let _ = self.watcher.unwatch(&dir);
            self.watched.remove(&dir);
        }
        for dir in wanted {
            if !self.watched.contains(&dir)
                && self
                    .watcher
                    .watch(&dir, RecursiveMode::NonRecursive)
                    .is_ok()
            {
                self.watched.insert(dir);
            }
        }
    }
}

pub fn due(first: Instant, last: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last) >= QUIET || now.saturating_duration_since(first) >= MAX_WAIT
}

pub fn wait_for(first: Instant, last: Instant, now: Instant) -> Duration {
    (last + QUIET)
        .min(first + MAX_WAIT)
        .saturating_duration_since(now)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn due_after_quiet_300ms() {
        let t = Instant::now();
        assert!(!due(t, t, t + Duration::from_millis(299)));
        assert!(due(t, t, t + Duration::from_millis(300)));
    }
    #[test]
    fn due_while_busy_after_1500ms() {
        let t = Instant::now();
        let last = t + Duration::from_millis(1400);
        assert!(!due(t, last, t + Duration::from_millis(1499)));
        assert!(due(t, last, t + Duration::from_millis(1500)));
    }
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "picview-watch-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn path_key(p: &std::path::Path) -> String {
        p.to_string_lossy()
            .replace('/', &char::from(92).to_string())
            .trim_end_matches(char::from(92))
            .to_lowercase()
    }
    fn receives_dir(rx: &mpsc::Receiver<Msg>, dir: &Path) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while let Ok(msg) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if let Msg::DirChanged { dir: changed } = msg {
                if path_key(&changed) == path_key(dir) {
                    return true;
                }
            }
        }
        false
    }
    #[test]
    fn watch_reports_removed_folder() {
        let root = TempDir::new();
        let dir = root.0.join("removed");
        std::fs::create_dir(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut watch = DirWatch::new(tx, egui::Context::default()).unwrap();
        watch.set_dirs(std::slice::from_ref(&dir));
        std::fs::remove_dir(&dir).unwrap();
        assert!(receives_dir(&rx, &dir));
    }
    #[test]
    fn watch_reports_renamed_folder() {
        let root = TempDir::new();
        let parent = root.0.join("ancestor");
        let dir = parent.join("child");
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut watch = DirWatch::new(tx, egui::Context::default()).unwrap();
        watch.set_dirs(std::slice::from_ref(&dir));
        std::fs::rename(&dir, parent.join("moved")).unwrap();
        assert!(receives_dir(&rx, &dir));
    }
    #[test]
    fn watch_switches_dirs_and_ignores_nested_files() {
        let root = TempDir::new();
        let old = root.0.join("old");
        let dir = root.0.join("current");
        let nested = dir.join("nested");
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut watch = DirWatch::new(tx, egui::Context::default()).unwrap();
        watch.set_dirs(std::slice::from_ref(&old));
        watch.set_dirs(std::slice::from_ref(&dir));
        std::fs::write(old.join("old.jpg"), b"old").unwrap();
        std::fs::write(nested.join("nested.jpg"), b"nested").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        std::fs::write(dir.join("new.jpg"), b"new").unwrap();
        assert!(receives_dir(&rx, &dir));
    }
    #[test]
    fn watch_rearms_recreated_folder() {
        let root = TempDir::new();
        let dir = root.0.join("recreated");
        std::fs::create_dir(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut watch = DirWatch::new(tx, egui::Context::default()).unwrap();
        watch.set_dirs(std::slice::from_ref(&dir));
        std::fs::remove_dir(&dir).unwrap();
        assert!(receives_dir(&rx, &dir));
        std::fs::create_dir(&dir).unwrap();
        assert!(receives_dir(&rx, &dir));
        watch.set_dirs(std::slice::from_ref(&dir));
        while rx.try_recv().is_ok() {}
        std::fs::write(dir.join("after.jpg"), b"new").unwrap();
        assert!(
            receives_dir(&rx, &dir),
            "replacement folder must stay watched"
        );
    }
    #[test]
    fn watch_reports_new_file() {
        let dir = TempDir::new();
        let (tx, rx) = mpsc::channel();
        let mut watch = DirWatch::new(tx, egui::Context::default()).expect("watcher");
        watch.set_dirs(&[dir.0.clone(), dir.0.clone()]);
        std::fs::write(dir.0.join("new.jpg"), b"image").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut reported = false;
        while let Ok(msg) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if let Msg::DirChanged { dir: changed } = msg {
                if path_key(&changed) == path_key(&dir.0) {
                    reported = true;
                    break;
                }
            }
        }
        drop(watch);
        assert!(reported, "new file was not reported within two seconds");
    }
}
