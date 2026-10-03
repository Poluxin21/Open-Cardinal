//! OS shutdown signals. Containers send SIGTERM (`docker stop`, Kubernetes, Swarm), so
//! handling only Ctrl-C — as the original did — meant a hard kill after the grace period.

/// Resolves with the name of the first signal received.
#[cfg(unix)]
pub async fn wait_for_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) =
        (signal(SignalKind::terminate()), signal(SignalKind::interrupt()), signal(SignalKind::hangup()))
    else {
        // cannot install handlers: fall back to Ctrl-C only
        let _ = tokio::signal::ctrl_c().await;
        return "SIGINT";
    };
    tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
        _ = hup.recv() => "SIGHUP",
    }
}

#[cfg(windows)]
pub async fn wait_for_signal() -> &'static str {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
    let (Ok(mut c), Ok(mut b), Ok(mut cl), Ok(mut sd)) = (ctrl_c(), ctrl_break(), ctrl_close(), ctrl_shutdown()) else {
        let _ = tokio::signal::ctrl_c().await;
        return "CTRL_C";
    };
    tokio::select! {
        _ = c.recv() => "CTRL_C",
        _ = b.recv() => "CTRL_BREAK",
        _ = cl.recv() => "CTRL_CLOSE",
        _ = sd.recv() => "CTRL_SHUTDOWN",
    }
}
