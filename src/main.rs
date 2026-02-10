mod healthsys;
mod gRPC;
mod engine; 
pub use gRPC::grpc_module::cardinal_core;

use sysinfo::System;
use healthsys::cardinal_health::run;

use crate::gRPC::grpc_module::run_grpc_server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sys = System::new_all();

    // Cardinal
    println!("Inicializando Cardinal Health Systecleam");
    tokio::spawn(async move {
        if let Err(e) = run(sys).await {
            eprintln!("Cardinal Health crashed: {:?}", e);
        }
    });

    
    println!("Inicializando Cardinal GRPC System");
    tokio::spawn(async {
        if let Err(e) = run_grpc_server().await {
            eprintln!("GRPC crashed: {:?}", e);
        }
    });

    tokio::signal::ctrl_c().await?;
    println!("Shutdown signal received");
    
    Ok(())
}
