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
        let total_agents = get_total_agents().await?;
        let agents_detected = get_total_rules().await?;

        let payload = monitor::collect_sys(&mut sys)?;
        monitor::persist(&payload, &total_agents, &agents_detected).await?;

        sleep(Duration::from_secs(1)).await;
    }
}

pub async fn get_total_rules() -> Result<i32, Box<dyn std::error::Error>> {
    let mut rules_dir = fs::read_dir("rules").await?;
    let mut total = 0;

    while let Some(entry) = rules_dir.next_entry().await? {
        let path = entry.path();

        if path.is_dir() {
            let mut sub_dir = fs::read_dir(path).await?;

            while let Some(file) = sub_dir.next_entry().await? {
                let file_path = file.path();

                if file_path.is_file()
                    && file_path.extension().map(|e| e == "lua").unwrap_or(false)
                {
                    total += 1;
                }
            }
        }
    }

    Ok(total)
}

pub async fn get_total_agents() -> Result<i32, Box<dyn std::error::Error>> {
    let mut rules_dir= fs::read_dir("rules").await?;
    let mut agentsint = 0;
    while let Some(entry) = rules_dir.next_entry().await? {
        if entry.path().is_dir() {
            if entry.path().display().to_string() == "default" {
                continue;
            }

            agentsint+=1;
        }
    }

    Ok(agentsint)

}