use sysinfo::System;
use tokio::fs;

use crate::kernel::models::sys_json::{MetricsJson, SysJson};

pub fn collect_sys(sys: &mut System) -> Result<SysJson, Box<dyn std::error::Error>> {
    sys.refresh_cpu();
    sys.refresh_memory();
    let kernel_version = System::kernel_version();

    Ok(SysJson {
        kernel_version: kernel_version,
        cpu_usage: sys.global_cpu_info().cpu_usage(),
        used_mem: sys.used_memory() as f64 / 1024.0,
        total_mem: sys.total_memory() as f64 / 1024.0,
    })
}

pub async fn persist(payload: &SysJson, total_agents: &i32, agents_detected: &i32, connected_agents: i32) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all("info").await?;
    let sys_json = serde_json::to_string(payload)?;
    fs::write("info/sys.json", sys_json).await?;

    // let metrics_json

    let metrics = MetricsJson {
        agents_detected: *total_agents,
        total_rules: *agents_detected,
        connected_agents: connected_agents,
    };

    let metrics_json = serde_json::to_string(&metrics)?;
    fs::write("info/metrics.json", metrics_json).await?;

    Ok(())
}