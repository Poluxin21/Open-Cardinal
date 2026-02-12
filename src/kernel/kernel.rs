use std::path::Path;

use sysinfo::System;
use tokio::{fs, time::{Duration, sleep}};
use crate::kernel::monitor::monitor;
pub async fn run(mut sys: System) -> Result<(), Box<dyn std::error::Error>> {
    let dirs = [
        "logs",
        "rules/default",
    ];

    for dir in dirs {
        fs::create_dir_all(dir).await?;
    }

    let files = [
        "rules/default/default.lua"
    ];
    
    for file in files {
        if !Path::new(file).exists() {
            fs::File::create(file).await?;
        }
    }

    loop {
        let payload = monitor::collect(&mut sys)?;
        monitor::persist(&payload).await?;

        sleep(Duration::from_secs(1)).await;
    }
}
