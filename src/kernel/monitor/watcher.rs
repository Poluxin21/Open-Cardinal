use notify::{RecursiveMode, Watcher};
use std::path::Path;
use tokio::sync::mpsc;

pub struct LuaWatcher {
    _watcher: notify::RecommendedWatcher,
    pub rx: mpsc::Receiver<()>,
}

pub fn watch_file() -> notify::Result<LuaWatcher> {
    let (tx_async, rx_async) = mpsc::channel(10);
    let (tx, rx) = std::sync::mpsc::channel();

    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(Path::new("rules"), RecursiveMode::Recursive)?;

    std::thread::spawn(move || {
        for res in rx {
            if let Ok(event) = res {
                if event.paths.iter().any(|p| {
                    p.extension().and_then(|e| e.to_str()) == Some("lua")
                }) {
                    let _ = tx_async.blocking_send(());
                }
            }
        }
    });

    Ok(LuaWatcher {
        _watcher: watcher,
        rx: rx_async,
    })
}