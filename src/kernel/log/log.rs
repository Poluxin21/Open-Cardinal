use tracing_appender::rolling;
use tracing_subscriber::fmt;

pub async fn init_logger() -> tracing_appender::non_blocking::WorkerGuard {
    let log_dir = "logs";
    let file_prefix = "cardinal_log";

    let file_appender = rolling::daily(log_dir, file_prefix);

    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(false)
        .init();

    guard
}
