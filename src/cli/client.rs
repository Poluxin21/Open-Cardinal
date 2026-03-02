use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::cli::args::Commands;
use crate::cli::protocol::{CliRequest, CliResponse, SOCKET_ADDR};

pub async fn send_command(cmd: &Commands) -> Result<(), Box<dyn std::error::Error>> {
    let request = command_to_request(cmd);

    let mut stream = TcpStream::connect(SOCKET_ADDR).await.map_err(|_| {
        eprintln!("❌ Não foi possível conectar ao Cardinal daemon em {}.", SOCKET_ADDR);
        eprintln!("   Verifique se o daemon está rodando.");
        std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "daemon offline")
    })?;

    let bytes = serde_json::to_vec(&request)?;
    stream.write_all(&bytes).await?;
    stream.shutdown().await?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;

    let response: CliResponse = serde_json::from_slice(&buf)?;
    print_response(response);

    Ok(())
}

fn command_to_request(cmd: &Commands) -> CliRequest {
    match cmd {
        Commands::Status      => CliRequest::Status,
        Commands::Stop        => CliRequest::Stop,
        Commands::Reload      => CliRequest::Reload,
        Commands::Stats       => CliRequest::Stats,
        Commands::Exec { command, args } => CliRequest::Exec {
            command: command.clone(),
            args: args.clone(),
        },
    }
}

fn print_response(response: CliResponse) {
    match response {
        CliResponse::Ok { message } => println!("✅ {}", message),
        CliResponse::Error { message } => {
            eprintln!("❌ {}", message);
            std::process::exit(1);
        }
        CliResponse::Data { payload } => {
            if let Some(obj) = payload.as_object() {
                println!("┌─────────────────────────────────────────┐");
                for (key, value) in obj {
                    let val = match value {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b)   => b.to_string(),
                        other => other.to_string(),
                    };
                    println!("│  {:<22} {}", format!("{}:", key), val);
                }
                println!("└─────────────────────────────────────────┘");
            } else {
                println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
            }
        }
    }
}