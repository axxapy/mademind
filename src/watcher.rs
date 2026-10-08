//! Filesystem watcher: a debounced re-index whenever a tracked file under the
//! data roots changes (inotify on Linux; works through bind mounts).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use notify::{EventKind, Watcher};

use crate::metrics::metric_add;

/// Debounce state machine: an event arms `dirty`; after `window` of quiet,
/// exactly one tick reports "fire". A new event resets the quiet window.
struct Debounce {
    quiet_since: Option<Instant>,
    dirty: bool,
}

impl Debounce {
    fn new() -> Self {
        Debounce {
            quiet_since: None,
            dirty: false,
        }
    }

    fn on_event(&mut self) {
        self.quiet_since = None;
        self.dirty = true;
    }

    /// Returns true when the quiet window elapsed and an update should run.
    fn tick(&mut self, now: Instant, window: Duration) -> bool {
        if self.quiet_since.is_none() && self.dirty {
            self.quiet_since = Some(now);
            return false;
        }
        if let Some(since) = self.quiet_since {
            if since.elapsed() >= window && self.dirty {
                self.quiet_since = None;
                self.dirty = false;
                return true;
            }
        }
        false
    }
}

fn matches_ext(p: &Path, exts: &[String]) -> bool {
    let Some(ext) = p.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    exts.iter().any(|w| ext.eq_ignore_ascii_case(w))
}

/// Forward tracked paths from an fs event (filter: event kind + extension).
fn on_fs_event(res: notify::Result<notify::Event>, exts: &[String], tx: &mpsc::Sender<PathBuf>) {
    let Ok(ev) = res else { return };
    if matches!(
        ev.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        for p in ev.paths.iter().filter(|p| matches_ext(p, exts)) {
            let _ = tx.send(p.clone());
        }
    }
}

/// Watch `roots` and call `on_fire` after each quiet `window` following
/// changes, plus once at start (after the watches are in place) to catch up
/// on edits made while mademind was down. Blocks the calling thread;
/// `on_fire` runs on it too, so changes that land during an update queue up
/// and trigger the next one.
pub fn run_watcher(
    roots: &[PathBuf],
    window: Duration,
    exts: Vec<String>,
    mut on_fire: impl FnMut(),
) {
    let (tx, rx) = mpsc::channel::<PathBuf>();
    let exts_w = exts.clone();
    let mut watcher = match notify::recommended_watcher(move |res| on_fs_event(res, &exts_w, &tx)) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("mademind: watcher init failed: {e}");
            return;
        }
    };
    for root in roots {
        if !root.exists() {
            eprintln!("mademind: root {root:?} does not exist, skipping");
        } else if let Err(e) = watcher.watch(root, notify::RecursiveMode::Recursive) {
            eprintln!("mademind: watch {root:?} failed: {e}");
        }
    }
    eprintln!(
        "mademind: watching {} roots (debounce {window:?}, extensions: {})",
        roots.len(),
        exts.join(",")
    );

    on_fire();
    let mut debounce = Debounce::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(p) => {
                debounce.on_event();
                metric_add("mademind_watcher_events_total", &[], 1.0);
                eprintln!("mademind: event: {p:?}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                thread::sleep(Duration::from_secs(3600));
                continue;
            }
        }
        if debounce.tick(Instant::now(), window) {
            on_fire();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RemoveKind};

    const WINDOW: Duration = Duration::from_millis(400);

    #[test]
    fn matches_ext_default_and_custom() {
        let exts: Vec<String> = vec!["md".into(), "markdown".into()];
        assert!(matches_ext(Path::new("a.md"), &exts));
        assert!(matches_ext(Path::new("dir/A.MD"), &exts));
        assert!(matches_ext(Path::new("a.markdown"), &exts));
        assert!(!matches_ext(Path::new("a.txt"), &exts));
        assert!(!matches_ext(Path::new("a"), &exts));
        assert!(!matches_ext(Path::new("a.md.bak"), &exts));
        assert!(!matches_ext(Path::new(".hidden"), &exts));
        assert!(matches_ext(Path::new("a.toml"), &["toml".to_string()]));
    }

    #[test]
    fn debounce_fires_once_after_quiet_window() {
        let mut d = Debounce::new();
        assert!(!d.tick(Instant::now(), WINDOW)); // nothing happened
        d.on_event();
        assert!(!d.tick(Instant::now(), WINDOW)); // window just started
        thread::sleep(WINDOW + Duration::from_millis(100));
        assert!(d.tick(Instant::now(), WINDOW)); // fires once
        assert!(!d.tick(Instant::now(), WINDOW)); // no double-fire
    }

    #[test]
    fn debounce_new_event_resets_window() {
        let mut d = Debounce::new();
        d.on_event();
        d.tick(Instant::now(), WINDOW);
        thread::sleep(Duration::from_millis(300));
        d.on_event(); // resets the quiet window
        assert!(!d.tick(Instant::now(), WINDOW));
        thread::sleep(Duration::from_millis(200));
        assert!(!d.tick(Instant::now(), WINDOW)); // 0.2s since reset
        thread::sleep(Duration::from_millis(300));
        assert!(d.tick(Instant::now(), WINDOW));
    }

    #[test]
    fn on_fs_event_filters_by_ext_and_kind() {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let exts: Vec<String> = vec!["md".into()];
        let ev = |k, p: &str| Ok(notify::Event::new(k).add_path(PathBuf::from(p)));
        on_fs_event(ev(EventKind::Create(CreateKind::Any), "a.md"), &exts, &tx);
        on_fs_event(ev(EventKind::Modify(ModifyKind::Any), "b.txt"), &exts, &tx);
        on_fs_event(ev(EventKind::Remove(RemoveKind::Any), "c.md"), &exts, &tx);
        on_fs_event(
            ev(EventKind::Access(notify::event::AccessKind::Any), "d.md"),
            &exts,
            &tx,
        );
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![PathBuf::from("a.md"), PathBuf::from("c.md")]
        );
    }
}
