use sysinfo::System;
use tokio::fs;

use crate::kernel::models::sys_json::SysJson;

pub fn collect(sys: &mut System) -> Result<SysJson, Box<dyn std::error::Error>> {
    sys.refresh_cpu();
    sys.refresh_memory();

    Ok(SysJson {
        cpu_usage: sys.global_cpu_info().cpu_usage(),
        used_mem: sys.used_memory() as f64 / 1024.0,
        total_mem: sys.total_memory() as f64 / 1024.0,
    })
}

pub async fn persist(payload: &SysJson) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all("info").await?;
    let json = serde_json::to_string(payload)?;
    fs::write("info/sys.json", json).await?;
    Ok(())
}
