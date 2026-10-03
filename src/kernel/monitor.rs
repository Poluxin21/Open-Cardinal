//! System metrics sampled once per second and kept in memory.
//!
//! The original design wrote `info/sys.json` + `info/metrics.json` every second and had
//! the HTTP server poll those files. Now the HTTP server reads this in-memory snapshot;
//! the files are still written (when `export_info_files` is on) for legacy consumers.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use serde::Serialize;
use sysinfo::System;

use crate::app::App;
use crate::util;

/// Shape of the legacy `info/sys.json` / `GET /info`.
#[derive(Serialize, Debug, Clone, Default)]
pub struct SysJson {
    pub kernel_version: Option<String>,
    pub cpu_usage: f32,
    /// KiB (kept as the original value, despite the misleading name).
    pub used_mem: f64,
    pub total_mem: f64,
}

/// Shape of the legacy `info/metrics.json` / `GET /metrics`.
#[derive(Serialize, Debug, Clone, Default)]
pub struct MetricsJson {
    pub total_rules: i32,
    pub agents_detected: i32,
    pub connected_agents: i32,
}

pub struct SysMonitor {
    snap: ArcSwap<SysJson>,
}

impl SysMonitor {
    pub fn new() -> Self {
        Self { snap: ArcSwap::from_pointee(SysJson::default()) }
    }

    pub fn snapshot(&self) -> Arc<SysJson> {
        self.snap.load_full()
    }

    fn sample(&self, sys: &mut System) {
        sys.refresh_cpu();
        sys.refresh_memory();
        self.snap.store(Arc::new(SysJson {
            kernel_version: System::kernel_version(),
            cpu_usage: sys.global_cpu_info().cpu_usage(),
            used_mem: sys.used_memory() as f64 / 1024.0,
            total_mem: sys.total_memory() as f64 / 1024.0,
        }));
    }
}

impl Default for SysMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn legacy_metrics(&self) -> MetricsJson {
        let set = self.engine.registry().current();
        MetricsJson {
            total_rules: set.total_rules() as i32,
            agents_detected: set.total_agent_dirs() as i32,
            connected_agents: self.metrics.in_flight() as i32,
        }
    }
}

/// Sample every second until shutdown.
pub async fn run(app: Arc<App>) {
    // `System::new()` instead of `new_all()`: we only need CPU and memory, and scanning
    // every process at startup is wasted work.
    let mut sys = System::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = app.shutdown.wait() => break,
        }
        app.sys.sample(&mut sys);

        if app.settings.config.export_info_files {
            let info_dir = app.settings.paths.info_dir();
            let sys_json = serde_json::to_vec(&*app.sys.snapshot());
            let metrics_json = serde_json::to_vec(&app.legacy_metrics());
            if let (Ok(a), Ok(b)) = (sys_json, metrics_json) {
                let res = tokio::task::spawn_blocking(move || {
                    util::write_atomic(&info_dir.join("sys.json"), &a, false)?;
                    util::write_atomic(&info_dir.join("metrics.json"), &b, false)
                })
                .await;
                if let Ok(Err(e)) = res {
                    tracing::warn!("cannot write info files: {e}");
                }
            }
        }
    }
}
