use std::{path::Path, sync::mpsc::channel};

use notify::{Result, Watcher};
use tracing::{error, info};
     
pub async fn watch_file() -> Result<()> {
    let (tx, rx) = channel();

    let rules = Path::new("rules");

    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(rules, notify::RecursiveMode::Recursive)?;

    for res in rx {
        match res {
            Ok(event) => {
                for path in event.paths {
                    if path.extension().and_then(|e| e.to_str()) == Some("lua") {
                        info!("Rule updated: {:?}", path);
                    }
                }
            }
            Err(err) => {
                error!("watcher error: {:?}", err);
            }
        }
    }

    Ok(())
}