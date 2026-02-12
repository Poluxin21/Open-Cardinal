use tonic::{transport::Server, Request, Response, Status};
use crate::engine::engine;
use tracing::info;
use std::time::Instant;

pub mod cardinal_core {
    tonic::include_proto!("cardinal.core");
}

use cardinal_core::sentinel_server::{Sentinel, SentinelServer};
use cardinal_core::{Pulse, Reaction};
use engine::RuleEngine;

#[derive(Debug, Default)]
pub struct CardinalService;

#[tonic::async_trait]
impl Sentinel for CardinalService {
    async fn sync(
        &self,
        request: Request<Pulse>,
    ) -> Result<Response<Reaction>, Status> {
        
        let recieved = Instant::now();

        let pulse = request.into_inner();
        
        let reply = RuleEngine::process(&pulse).await;
        
        let end = recieved.elapsed();
        info!("📡 Recebido de {}: {:?}  {:.2?}", pulse.agent_id, pulse.telemetry, end);

        Ok(Response::new(reply))
    }
}

pub async fn run_grpc_server() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051".parse()?;
    let service = CardinalService::default();

    println!("🚀 Cardinal gRPC Server listening em {}", addr);
    info!("🚀 Cardinal gRPC Server listening em {}", addr);

    Server::builder()
        .add_service(SentinelServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}