mod kernel;
mod g_rpc;
mod engine; 

use tracing::{info, error};

pub use g_rpc::grpc_module::cardinal_core;

use sysinfo::System;
use kernel::cardinal_kernel::run;

use crate::g_rpc::grpc_module::run_grpc_server;
use crate::kernel::log::log_sys::init_logger;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sys = System::new_all();
    let _log_guard = init_logger().await;

    info!("Inicializando Cardinal General System");
    tokio::spawn(async move {
        if let Err(e) = run(sys).await {
            error!("Cardinal crashed: {:?}", e);
        }
    });

    
    info!("Inicializando Cardinal GRPC System");
    tokio::spawn(async {
        if let Err(e) = run_grpc_server().await {
            error!("GRPC crashed: {:?}", e);
        }
    });

    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received");
    
    Ok(())
}
