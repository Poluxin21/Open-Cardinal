//! Logging setup: daily rolling file (bare daemon, as before) or stdout (containers),
//! text or JSON.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt};

use crate::config::{LogFormat, LogOutput, Settings};

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Keeps the background log writer alive; drop it only at process exit.
pub struct LogGuard {
    _guards: Vec<WorkerGuard>,
}

pub fn init(settings: &Settings) -> LogGuard {
    let cfg = &settings.config.log;
    // RUST_LOG wins over the config file for ad-hoc debugging
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let mut guards = Vec::new();
    let mut layers: Vec<BoxedLayer> = Vec::new();
    let output = settings.log_output();

    if matches!(output, LogOutput::File | LogOutput::Both) {
        let appender = tracing_appender::rolling::daily(settings.paths.logs_dir(), "cardinal_log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        guards.push(guard);
        layers.push(layer(cfg.format, writer));
    }
    if matches!(output, LogOutput::Stdout | LogOutput::Both) {
        let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());
        guards.push(guard);
        layers.push(layer(cfg.format, writer));
    }

    // `try_init`: tests and embedders may already have installed a subscriber
    let _ = tracing_subscriber::registry().with(layers).with(filter).try_init();
    LogGuard { _guards: guards }
}

fn layer<W>(format: LogFormat, writer: W) -> BoxedLayer
where
    W: for<'w> fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    match format {
        LogFormat::Json => fmt::layer().json().with_writer(writer).with_target(false).boxed(),
        LogFormat::Text => fmt::layer().with_ansi(false).with_target(false).with_writer(writer).boxed(),
    }
}
