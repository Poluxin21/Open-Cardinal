use clap::{Parser, Subcommand};
use rand::Rng;

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use std::error::Error;

pub mod cardinal_core {
    tonic::include_proto!("cardinal.core");
}

use cardinal_core::sentinel_client::SentinelClient;
use cardinal_core::Pulse;

#[derive(Parser)]
#[command(name = "Cardinal CLI")]
#[command(version = "1.0")]
#[command(about = "Cliente de teste e simulação para o Open Cardinal", long_about = None)]
struct Cli {
    #[arg(short, long, default_value = "http://[::1]:50051")]
    addr: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Ping {
        #[arg(short, long, default_value = "Test_Agent")]
        id: String,
    },
    
    Simulate {
        #[arg(short, long, default_value = "Simulated_Rocket")]
        id: String,
        #[arg(short, long, default_value_t = 1000)]
        interval: u64,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    println!("🔌 Conectando ao Cardinal em {}...", cli.addr);
    
    let mut client = match SentinelClient::connect(cli.addr.clone()).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ Falha ao conectar: {}", e);
            return Ok(());
        }
    };

    println!("✅ Conectado com sucesso!");

    match cli.command {
        Commands::Ping { id } => {
            send_pulse(&mut client, &id).await?;
        }
        Commands::Simulate { id, interval } => {
            println!("🚀 Iniciando simulação para '{}' a cada {}ms...", id, interval);
            println!("(Pressione Ctrl+C para parar)");
            
            loop {
                send_pulse(&mut client, &id).await?;
                tokio::time::sleep(tokio::time::Duration::from_millis(interval)).await;
            }
        }
    }

    Ok(())
}

async fn send_pulse(
    client: &mut SentinelClient<tonic::transport::Channel>, 
    agent_id: &str
) -> Result<(), Box<dyn Error>> {
    
    let mut rng = rand::thread_rng();
    let mut telemetry = HashMap::new();
    
    // rocket/airplane data
    let fuel = rng.gen_range(0..100);
    let altitude = rng.gen_range(0..2000);
    
    telemetry.insert("fuel".to_string(), fuel.to_string());
    telemetry.insert("altitude".to_string(), altitude.to_string());
    telemetry.insert("velocity".to_string(), format!("{:.2}", rng.gen_range(0.0..500.0)));

    // 2. create pulse (Protobuf Message)
    let request = tonic::Request::new(Pulse {
        agent_id: agent_id.to_string(),
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
        telemetry: telemetry.clone(),
    });

    // 3. wait reaction
    let response = client.sync(request).await?;
    let reaction = response.into_inner();

    // 4. just formatting
    print_reaction(agent_id, telemetry, reaction);

    Ok(())
}

fn print_reaction(agent_id: &str, input: HashMap<String, String>, reaction: cardinal_core::Reaction) {
    // decode actionType
    let action_display = match reaction.r#type {
        0 => "IDLE",
        1 => "SHUTDOWN",
        2 => "RESTART",
        3 => "CUSTOM",
        _ => "UNKNOWN",
    };

    println!("--------------------------------------------------");
    println!("📤 Enviado [{}]: Combustível: {}% | Alt: {}m", agent_id, input.get("fuel").unwrap(), input.get("altitude").unwrap());
    
    if reaction.r#type != 0 {
        // reaction
        println!("📥 REAÇÃO CARDINAL: {} | Cmd: {}", action_display, reaction.command_name);
        println!("   Params: {:?}", reaction.parameters);
        
        if reaction.command_name == "EMERGENCY_LANDING" || reaction.r#type == 1 {
            println!("⚠️  AÇÃO CRÍTICA DETECTADA PELO SCRIPT LUA!");
        }
    } else {
        println!("📥 Resposta: {}", action_display);
    }
}