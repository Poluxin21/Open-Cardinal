mod kernel;
mod g_rpc;
mod engine; 
mod http_server;
use tracing::{info, error};

pub use g_rpc::g_rpc::cardinal_core;

use sysinfo::System;
use kernel::kernel::run;

use crate::g_rpc::g_rpc::run_grpc_server;
use crate::http_server::http_server::run_http_server;
use crate::kernel::log::log::init_logger;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sys = System::new_all();
    let _log_guard = init_logger().await;

    info!("Started Cardinal General System");
    tokio::spawn(async move {
        if let Err(e) = run(sys).await {
            error!("Cardinal crashed: {:?}", e);
        }
    });

    
    info!("Started Cardinal GRPC System");
    tokio::spawn(async {
        if let Err(e) = run_grpc_server().await {
            error!("GRPC crashed: {:?}", e);
        }
    });

    info!("Started Cardinal Http System");
    tokio::spawn(async {
        if let Err(e) = run_http_server().await {
            error!("Http crashed: {:?}", e);
        }
    });

    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received");
    
    Ok(())
}
