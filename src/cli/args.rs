use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::control::protocol::CliRequest;

#[derive(Parser, Debug)]
#[command(
    name = "open-cardinal",
    author,
    version,
    about = "Cardinal daemon & CLI",
    long_about = "Cardinal General System — rode sem argumentos para iniciar o daemon,\nou use subcomandos para interagir com um daemon em execução."
)]
pub struct Cli {
    /// Home directory of the daemon (config/, rules/, data). Defaults to $CARDINAL_HOME or the current directory.
    #[arg(long, global = true, env = "CARDINAL_HOME")]
    pub home: Option<PathBuf>,

    /// Admin token for CLI commands. Defaults to <home>/config/admin.token.
    #[arg(long, global = true, env = "CARDINAL_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Probe the local daemon's /readyz (exit code 0 = ready). For container HEALTHCHECKs.
    Health,

    /// Daemon status.
    Status,

    /// Stop the daemon gracefully.
    Stop,

    /// Reload rules and tenants now.
    Reload,

    /// Process statistics.
    Stats,

    /// Operator overrides: force | revoke_force | list.
    ///
    /// Example: open-cardinal heathcliff force --agent Rocket_01 --force 1
    Heathcliff {
        command: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Cluster: status | members | add-peer <ip> | remove-peer <ip> | transfer-leader [ip] | snapshot.
    Raft {
        command: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Tenants: list | add <id> | issue-key <id> | revoke-key <id> <label> | enable|disable <id>.
    Tenant {
        command: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Loaded rules: list | issues.
    Rules {
        command: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Certificates for TLS/mTLS: init-ca | issue (runs locally, no daemon needed).
    ///
    /// Example: open-cardinal tls init-ca --dir certs
    ///          open-cardinal tls issue --dir certs --name node1 --ip 10.0.0.11
    Tls {
        command: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Audit trail: verify | tail.
    Audit {
        command: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

impl Commands {
    pub fn to_request(&self) -> CliRequest {
        match self.clone() {
            // handled locally in `main`, never sent to the daemon
            Commands::Health => CliRequest::Status,
            Commands::Status => CliRequest::Status,
            Commands::Stop => CliRequest::Stop,
            Commands::Reload => CliRequest::Reload,
            Commands::Stats => CliRequest::Stats,
            Commands::Heathcliff { command, args } => CliRequest::Heathcliff { command, args },
            Commands::Raft { command, args } => CliRequest::Raft { command, args },
            Commands::Tenant { command, args } => CliRequest::Tenant { command, args },
            Commands::Rules { command, args } => CliRequest::Rules { command, args },
            Commands::Audit { command, args } => CliRequest::Audit { command, args },
            // handled locally in `main`
            Commands::Tls { .. } => CliRequest::Status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("open-cardinal").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn no_args_means_run_the_daemon() {
        assert!(parse(&[]).command.is_none());
    }

    #[test]
    fn heathcliff_works_with_and_without_the_double_dash() {
        // the wiki documents `-- --agent ...`; the original rejected the form without it
        for argv in [
            vec!["heathcliff", "force", "--", "--agent", "R1", "--force", "1"],
            vec!["heathcliff", "force", "--agent", "R1", "--force", "1"],
        ] {
            match parse(&argv).command.unwrap().to_request() {
                CliRequest::Heathcliff { command, args } => {
                    assert_eq!(command, "force");
                    assert!(args.contains(&"--agent".to_string()) && args.contains(&"R1".to_string()));
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn global_flags_after_the_subcommand() {
        let cli = parse(&["status", "--home", "/tmp/c", "--token", "abc"]);
        assert_eq!(cli.home.as_deref(), Some(std::path::Path::new("/tmp/c")));
        assert_eq!(cli.token.as_deref(), Some("abc"));
    }
}
