use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "cardinal",
    author,
    version,
    about = "Cardinal daemon & CLI",
    long_about = "Cardinal General System — rode sem argumentos para iniciar o daemon,\nou use subcomandos para interagir com um daemon em execução."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    Status,

    Stop,

    Reload,

    Stats,

    Exec {
        command: String,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}