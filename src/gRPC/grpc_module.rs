use tonic::{transport::Server, Request, Response, Status};
use crate::engine::engine;

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
        
        let pulse = request.into_inner();
        
        println!("📡 Recebido de {}: {:?}", pulse.agent_id, pulse.telemetry);

        let reply = RuleEngine::process(&pulse).await;

        Ok(Response::new(reply))
    }
}

pub async fn run_grpc_server() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051".parse()?;
    let service = CardinalService::default();

    println!("🚀 Cardinal gRPC Server ouvindo em {}", addr);

    Server::builder()
        .add_service(SentinelServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}