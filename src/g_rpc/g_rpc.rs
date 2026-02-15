use tonic::{transport::Server, Request, Response, Status};
use crate::engine::engine;
use tracing::info;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

pub mod cardinal_core {
    tonic::include_proto!("cardinal.core");
}

use cardinal_core::sentinel_server::{Sentinel, SentinelServer};
use cardinal_core::{Pulse, Reaction};
use engine::RuleEngine;

#[derive(Debug, Default)]
pub struct CardinalService {
    pub active_connections: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl Sentinel for CardinalService {
    async fn sync(
        &self,
        request: Request<Pulse>,
    ) -> Result<Response<Reaction>, Status> {
        
        self.active_connections.fetch_add(1, Ordering::Relaxed);

        let recieved = Instant::now();

        let pulse = request.into_inner();
        
        let reply = RuleEngine::process(&pulse).await;
        
        let end = recieved.elapsed();
        info!("Recieved from {}: {:?}  {:.2?}", pulse.agent_id, pulse.telemetry, end);
        info!("Reaction to {} => type: {}, command: {}", reply.trace_id, reply.r#type, reply.command_name);
        
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
        Ok(Response::new(reply))
    }
}

pub async fn run_grpc_server(active_connections_grpc: Arc<AtomicUsize>) -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051".parse()?;

    let service = CardinalService {
        active_connections: active_connections_grpc,
    };

    println!("Cardinal gRPC Server listening em {}", addr);
    info!("Cardinal gRPC Server listening em {}", addr);

    Server::builder()
        .add_service(SentinelServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}