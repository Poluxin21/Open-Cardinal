//! Hot reload: watch rule trees and tenant/model configuration.

use std::path::Path;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::config::Paths;

/// Keeps the OS watcher alive; dropping it stops the notifications.
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    pub rx: mpsc::Receiver<()>,
}

const WATCHED_CONFIG_FILES: [&str; 2] = ["tenants.json", "models.json"];

pub fn watch(paths: &Paths) -> notify::Result<ConfigWatcher> {
    // capacity 1: a burst of events collapses into a single pending notification
    let (tx, rx) = mpsc::channel::<()>(1);
    let config_dir = paths.config_dir();
    let tx2 = tx.clone();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        let relevant = event.paths.iter().any(|p| is_relevant(p, &config_dir));
        if relevant {
            let _ = tx2.try_send(());
        }
    })?;

    for dir in [paths.rules_dir(), paths.tenants_dir()] {
        if dir.exists() {
            watcher.watch(&dir, RecursiveMode::Recursive)?;
        }
    }
    watcher.watch(&paths.config_dir(), RecursiveMode::NonRecursive)?;
    drop(tx);
    Ok(ConfigWatcher { _watcher: watcher, rx })
}

fn is_relevant(path: &Path, config_dir: &Path) -> bool {
    if path.parent() == Some(config_dir) {
        return path.file_name().and_then(|n| n.to_str()).is_some_and(|n| WATCHED_CONFIG_FILES.contains(&n));
    }
    // ignore editor swap/temp files and our own atomic-write temporaries
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    !(name.starts_with('.') || name.ends_with('~') || name.contains(".tmp-"))
}

impl ConfigWatcher {
    /// Wait for the next change, then absorb the burst that usually follows (editors write
    /// several times). Returns `false` when the watcher is gone.
    ///
    /// This is a method (not a free function over `rx`) on purpose: with disjoint closure
    /// captures (edition 2024) an `async move` block using only `w.rx` would drop the OS
    /// watcher stored next to it, silently disabling hot reload.
    pub async fn next_change(&mut self) -> bool {
        if self.rx.recv().await.is_none() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        while self.rx.try_recv().is_ok() {}
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevance_filter() {
        let cfg = Path::new("/h/config");
        assert!(is_relevant(Path::new("/h/config/tenants.json"), cfg));
        assert!(!is_relevant(Path::new("/h/config/admin.token"), cfg));
        assert!(!is_relevant(Path::new("/h/config/config.json"), cfg));
        assert!(is_relevant(Path::new("/h/rules/A/x.lua"), cfg));
        assert!(!is_relevant(Path::new("/h/rules/A/.x.lua.swp"), cfg));
        assert!(!is_relevant(Path::new("/h/rules/A/x.lua~"), cfg));
        assert!(!is_relevant(Path::new("/h/tenants/a/rules/x.tmp-ab12"), cfg));
    }

    #[tokio::test]
    async fn file_change_triggers_a_single_notification() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.rules_dir().join("A")).unwrap();
        std::fs::create_dir_all(paths.tenants_dir()).unwrap();
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let mut w = watch(&paths).unwrap();
        std::fs::write(paths.rules_dir().join("A/x.lua"), "return nil").unwrap();
        std::fs::write(paths.rules_dir().join("A/x.lua"), "return nil -- v2").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), w.next_change()).await;
        assert_eq!(got, Ok(true));
    }
}
