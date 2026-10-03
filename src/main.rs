use clap::Parser;

use open_cardinal::cli::args::Cli;
use open_cardinal::config::Paths;
use open_cardinal::control::client;
use open_cardinal::daemon;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Subcommand → talk to a running daemon. No subcommand → be the daemon.
    let code = match &cli.command {
        Some(cmd) => run_command(&cli, cmd).await,
        None => match daemon::run(Paths::resolve(cli.home.as_deref())).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("❌ {e}");
                1
            }
        },
    };
    std::process::exit(code);
}

async fn run_command(cli: &Cli, cmd: &open_cardinal::cli::args::Commands) -> i32 {
    if let open_cardinal::cli::args::Commands::Tls { command, args } = cmd {
        return open_cardinal::tls::cli(command, args);
    }
    if matches!(cmd, open_cardinal::cli::args::Commands::Health) {
        return open_cardinal::daemon::health_probe(cli.home.as_deref()).await;
    }
    // generating a secret needs no daemon (and must work before one exists)
    if let open_cardinal::cli::args::Commands::Raft { command, .. } = cmd
        && command == "keygen"
    {
        println!("{}", open_cardinal::raft::config::generate_secret());
        return 0;
    }
    let target = match client::resolve_target(cli.home.as_deref(), cli.token.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("❌ {e}");
            return 1;
        }
    };
    match client::send(&target, cmd.to_request()).await {
        Ok(response) => client::print_response(response),
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}
