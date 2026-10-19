//! Test and simulation client for Open Cardinal.
//!
//! * `ping`      — send one pulse
//! * `simulate`  — send pulses forever (the original rocket demo)
//! * `bench`     — measure latency/throughput with N concurrent agents

use std::collections::HashMap;
use std::error::Error;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use rand::Rng;
use tonic::metadata::MetadataValue;

use open_cardinal::pb::core::sentinel_client::SentinelClient;
use open_cardinal::pb::core::{Pulse, Reaction};

#[derive(Parser)]
#[command(name = "Cardinal CLI")]
#[command(version = "1.0")]
#[command(about = "Cliente de teste e simulação para o Open Cardinal", long_about = None)]
struct Cli {
    #[arg(short, long, global = true, default_value = "http://[::1]:50051")]
    addr: String,

    /// CA certificate (PEM) that signed the server certificate; enables TLS (use an https:// address).
    #[arg(long, global = true)]
    ca: Option<std::path::PathBuf>,

    /// Client certificate and key (PEM) for mutual TLS.
    #[arg(long, global = true, requires = "key")]
    cert: Option<std::path::PathBuf>,
    #[arg(long, global = true, requires = "cert")]
    key: Option<std::path::PathBuf>,

    /// Name to verify the server certificate against (default: the host of --addr).
    #[arg(long, global = true)]
    server_name: Option<String>,

    /// API key of a tenant (`Authorization: Bearer ...`).
    #[arg(long, global = true, env = "CARDINAL_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Send a single pulse. Without --telemetry the pulse carries random rocket data.
    Ping {
        #[arg(short, long, default_value = "Test_Agent")]
        id: String,
        /// key=value, repeatable (e.g. --telemetry cpu_temp=95)
        #[arg(short, long = "telemetry")]
        telemetry: Vec<String>,
    },

    /// Send pulses continuously.
    Simulate {
        #[arg(short, long, default_value = "Simulated_Rocket")]
        id: String,

        #[arg(long, default_value_t = 1000)]
        interval: u64,
    },

    /// Load test: `concurrency` agents send `requests` pulses in total.
    Bench {
        #[arg(short, long, default_value_t = 8)]
        concurrency: usize,
        #[arg(short, long, default_value_t = 2000)]
        requests: usize,
        #[arg(short, long, default_value = "Bench")]
        id: String,
        /// Open one TCP connection per worker instead of multiplexing all of them on one.
        #[arg(long)]
        connection_per_worker: bool,
    },
}

type Client = SentinelClient<tonic::transport::Channel>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    println!("🔌 Conectando ao Cardinal em {}...", cli.addr);
    let target = endpoint(&cli)?;
    let channel = match target.connect().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ Falha ao conectar: {e}");
            std::process::exit(1);
        }
    };
    println!("✅ Conectado com sucesso!");
    let key = cli.api_key.clone();

    match cli.command {
        Commands::Ping { id, telemetry } => {
            let mut client = SentinelClient::new(channel);
            let t = if telemetry.is_empty() { random_rocket() } else { parse_telemetry(&telemetry)? };
            send_pulse(&mut client, &key, &id, t).await?;
        }
        Commands::Simulate { id, interval } => {
            let mut client = SentinelClient::new(channel);
            println!("🚀 Iniciando simulação para '{id}' a cada {interval}ms...");
            println!("(Pressione Ctrl+C para parar)");
            loop {
                send_pulse(&mut client, &key, &id, random_rocket()).await?;
                tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
            }
        }
        Commands::Bench { concurrency, requests, id, connection_per_worker } => {
            bench(channel, connection_per_worker.then_some(target), key, concurrency.max(1), requests, id).await?
        }
    }
    Ok(())
}

fn parse_telemetry(items: &[String]) -> Result<HashMap<String, String>, Box<dyn Error>> {
    let mut m = HashMap::new();
    for kv in items {
        let (k, v) = kv.split_once('=').ok_or_else(|| format!("--telemetry expects key=value, got '{kv}'"))?;
        m.insert(k.to_string(), v.to_string());
    }
    Ok(m)
}

fn random_rocket() -> HashMap<String, String> {
    let mut rng = rand::thread_rng();
    let mut t = HashMap::new();
    t.insert("fuel".to_string(), rng.gen_range(0..100).to_string());
    t.insert("altitude".to_string(), rng.gen_range(0..2000).to_string());
    t.insert("velocity".to_string(), format!("{:.2}", rng.gen_range(0.0..500.0)));
    t
}

fn request_for(key: &Option<String>, pulse: Pulse) -> Result<tonic::Request<Pulse>, Box<dyn Error>> {
    let mut req = tonic::Request::new(pulse);
    if let Some(k) = key {
        req.metadata_mut().insert("authorization", MetadataValue::try_from(format!("Bearer {k}"))?);
    }
    Ok(req)
}

async fn send_pulse(
    client: &mut Client,
    key: &Option<String>,
    agent_id: &str,
    telemetry: HashMap<String, String>,
) -> Result<(), Box<dyn Error>> {
    let pulse = Pulse {
        agent_id: agent_id.to_string(),
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
        telemetry: telemetry.clone(),
    };
    let reaction = client.sync(request_for(key, pulse)?).await?.into_inner();
    print_reaction(agent_id, &telemetry, reaction);
    Ok(())
}

fn print_reaction(agent_id: &str, input: &HashMap<String, String>, reaction: Reaction) {
    let action = match reaction.r#type {
        0 => "IDLE",
        1 => "SHUTDOWN",
        2 => "RESTART",
        3 => "CUSTOM",
        _ => "UNKNOWN",
    };
    let shown = |k: &str| input.get(k).map(String::as_str).unwrap_or("-");
    println!("--------------------------------------------------");
    println!("📤 Enviado [{agent_id}]: Combustível: {}% | Alt: {}m", shown("fuel"), shown("altitude"));

    if reaction.r#type != 0 {
        println!("📥 REAÇÃO CARDINAL: {action} | Cmd: {}", reaction.command_name);
        println!("   Params: {:?}", reaction.parameters);
        if reaction.command_name == "EMERGENCY_LANDING" || reaction.r#type == 1 {
            println!("⚠️  AÇÃO CRÍTICA DETECTADA PELO SCRIPT LUA!");
        }
    } else {
        println!("📥 Resposta: {action}");
    }
}

async fn bench(
    channel: tonic::transport::Channel,
    own_connections: Option<tonic::transport::Endpoint>,
    key: Option<String>,
    concurrency: usize,
    requests: usize,
    id: String,
) -> Result<(), Box<dyn Error>> {
    let per_worker = (requests / concurrency).max(1);
    let started = Instant::now();
    let mut workers = Vec::new();
    for w in 0..concurrency {
        let shared = channel.clone();
        let own = own_connections.clone();
        let key = key.clone();
        let agent = format!("{id}_{w}");
        workers.push(tokio::spawn(async move {
            let channel = match own {
                Some(ep) => match ep.connect().await {
                    Ok(c) => c,
                    Err(_) => return (Vec::new(), per_worker),
                },
                None => shared,
            };
            let mut client = SentinelClient::new(channel);
            let (mut latencies, mut errors) = (Vec::with_capacity(per_worker), 0usize);
            for _ in 0..per_worker {
                let pulse = Pulse {
                    agent_id: agent.clone(),
                    timestamp: 0,
                    telemetry: HashMap::from([("fuel".to_string(), "100".to_string())]),
                };
                let req = match request_for(&key, pulse) {
                    Ok(r) => r,
                    Err(_) => {
                        errors += 1;
                        continue;
                    }
                };
                let t = Instant::now();
                match client.sync(req).await {
                    Ok(_) => latencies.push(t.elapsed().as_micros() as u64),
                    Err(_) => errors += 1,
                }
            }
            (latencies, errors)
        }));
    }
    let (mut all, mut errors) = (Vec::new(), 0usize);
    for w in workers {
        let (l, e) = w.await?;
        all.extend(l);
        errors += e;
    }
    let wall = started.elapsed().as_secs_f64();
    all.sort_unstable();
    let pct = |q: f64| all.get(((all.len() as f64) * q) as usize).copied().unwrap_or(0);
    println!(
        "conc={concurrency} sent={} ok={} errors={errors} wall={wall:.2}s throughput={:.0} req/s p50={}us p99={}us max={}us",
        per_worker * concurrency,
        all.len(),
        all.len() as f64 / wall,
        pct(0.5),
        pct(0.99),
        all.last().copied().unwrap_or(0)
    );
    Ok(())
}

fn endpoint(cli: &Cli) -> Result<tonic::transport::Endpoint, Box<dyn Error>> {
    let ep = tonic::transport::Endpoint::from_shared(cli.addr.clone())?;
    with_tls(ep, cli)
}

#[cfg(not(feature = "tls"))]
fn with_tls(ep: tonic::transport::Endpoint, cli: &Cli) -> Result<tonic::transport::Endpoint, Box<dyn Error>> {
    if cli.ca.is_some() {
        return Err("este cliente foi compilado sem TLS (use --features tls)".into());
    }
    Ok(ep)
}

#[cfg(feature = "tls")]
fn with_tls(ep: tonic::transport::Endpoint, cli: &Cli) -> Result<tonic::transport::Endpoint, Box<dyn Error>> {
    use tonic::transport::{Certificate, ClientTlsConfig, Identity};
    let Some(ca) = &cli.ca else { return Ok(ep) };
    let mut tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(std::fs::read(ca)?));
    if let (Some(c), Some(k)) = (&cli.cert, &cli.key) {
        tls = tls.identity(Identity::from_pem(std::fs::read(c)?, std::fs::read(k)?));
    }
    if let Some(name) = &cli.server_name {
        tls = tls.domain_name(name.clone());
    }
    Ok(ep.tls_config(tls)?)
}
