mod kernel;
mod g_rpc;
mod engine; 
mod http_server;
mod utils;
mod cli;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use clap::Parser;
use tracing::{info, error};

pub use g_rpc::g_rpc::cardinal_core;

use sysinfo::System;
use kernel::kernel::run;

use crate::cli::args::Cli;
use crate::cli::command_server::run_command_server;
use crate::g_rpc::g_rpc::run_grpc_server;
use crate::http_server::http_server::run_http_server;
use crate::kernel::log::log::init_logger;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli_args = Cli::parse();

    if let Some(cmd) = &cli_args.command {
        return cli::client::send_command(cmd).await;
    }

    let sys = System::new_all();
    let _log_guard = init_logger().await;
    let active_connections = Arc::new(AtomicUsize::new(0));
    let cmd_counter      = active_connections.clone();
    let grpc_counter = active_connections.clone();
    let monitor_counter = active_connections.clone();

    info!("Started Cardinal General System");
    tokio::spawn(async move {
        if let Err(e) = run(sys, monitor_counter).await {
            error!("Cardinal crashed: {:?}", e);
        }
    });

    
    info!("Started Cardinal GRPC System");
    tokio::spawn(async {
        if let Err(e) = run_grpc_server(grpc_counter).await {
            error!("GRPC crashed: {:?}", e);
        }
    });

    info!("Started Cardinal Http System");
    tokio::spawn(async {
        if let Err(e) = run_http_server().await {
            error!("Http crashed: {:?}", e);
        }
    });

    info!("Started Cardinal Command Server");
    tokio::spawn(async move {
        if let Err(e) = run_command_server(cmd_counter).await {
            error!("Command server crashed: {:?}", e);
        }
    });

    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received");
    
    Ok(())
}

#[cfg(unix)]
async fn wait_sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut s = signal(SignalKind::terminate()).expect("Failed SIGTERM handler");
    s.recv().await;
}

#[cfg(not(unix))]
async fn wait_sigterm() {
    std::future::pending::<()>().await
}