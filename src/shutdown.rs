//! Process-wide graceful-shutdown latch.
//!
//! The latch is set once and stays set, so a shutdown requested before anything
//! waits on it is still honoured. Nothing here touches OS signals unless the host
//! calls [`install_signal_handlers`]; language bindings wire their host's own
//! signal handling to [`request_shutdown`] instead.
//!
//! ```no_run
//! # async fn example() -> std::io::Result<()> {
//! mq_bridge::shutdown::install_signal_handlers(|code| std::process::exit(code))?;
//! mq_bridge::shutdown::shutdown_requested().await;
//! mq_bridge::shutdown::stop_all_routes().await;
//! # Ok(())
//! # }
//! ```

use std::sync::{Mutex, OnceLock, PoisonError};
use tokio::sync::watch;
use tracing::{info, warn};

/// A one-way latch that flips to "shutdown requested" and never resets.
#[derive(Clone, Debug)]
pub struct Shutdown {
    requested: watch::Sender<bool>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            requested: watch::channel(false).0,
        }
    }

    /// Sets the latch. Returns `true` only for the call that set it.
    pub fn request(&self) -> bool {
        !self.requested.send_replace(true)
    }

    pub fn is_requested(&self) -> bool {
        *self.requested.borrow()
    }

    /// Resolves once the latch is set, including before this call.
    pub async fn requested(&self) {
        let mut rx = self.requested.subscribe();
        let _ = rx.wait_for(|requested| *requested).await;
    }
}

static GLOBAL: OnceLock<Shutdown> = OnceLock::new();
static SIGNALS_INSTALLED: Mutex<bool> = Mutex::new(false);

/// The process-wide latch used by the free functions in this module.
pub fn global() -> &'static Shutdown {
    GLOBAL.get_or_init(Shutdown::new)
}

/// Requests a graceful shutdown. Returns `true` only for the first request.
pub fn request_shutdown() -> bool {
    global().request()
}

pub fn is_shutdown_requested() -> bool {
    global().is_requested()
}

/// Resolves once a shutdown has been requested, including before this call.
pub async fn shutdown_requested() {
    global().requested().await
}

/// Stops every deployed route. Returns the names that were running.
pub async fn stop_all_routes() -> Vec<String> {
    let names = crate::route::list_routes();
    for name in &names {
        crate::route::stop_route(name).await;
    }
    names
}

/// Routes SIGINT and SIGTERM (Ctrl+C on Windows) into the global latch.
///
/// Opt-in: registering these permanently replaces the signals' default action for
/// the whole process, so only a Rust application that owns its process should call
/// it. The second signal calls `on_repeat` with the conventional exit code (130 for
/// SIGINT, 143 for SIGTERM); signals after that are ignored. Repeated calls are
/// no-ops. Must run inside a Tokio runtime.
///
/// Call it before loading any Go `c-shared` plugin: the Go runtime adds
/// `SA_ONSTACK` only to handlers that already exist when it loads.
pub fn install_signal_handlers(
    on_repeat: impl FnOnce(i32) + Send + 'static,
) -> std::io::Result<()> {
    let mut installed = SIGNALS_INSTALLED
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if *installed {
        return Ok(());
    }
    let mut signals = OsSignals::new()?;
    tokio::spawn(async move {
        signals.next().await;
        request_shutdown();
        info!("Shutting down; signal again to force exit.");
        let code = signals.next().await;
        warn!("Second shutdown signal received.");
        on_repeat(code);
    });
    *installed = true;
    Ok(())
}

#[cfg(unix)]
struct OsSignals {
    sigint: tokio::signal::unix::Signal,
    sigterm: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl OsSignals {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self {
            sigint: signal(SignalKind::interrupt())?,
            sigterm: signal(SignalKind::terminate())?,
        })
    }

    /// Waits for the next signal and returns its conventional exit code.
    async fn next(&mut self) -> i32 {
        tokio::select! {
            _ = self.sigint.recv() => { info!("Ctrl+C (SIGINT) received."); 130 }
            _ = self.sigterm.recv() => { info!("SIGTERM received."); 143 }
        }
    }
}

#[cfg(not(unix))]
struct OsSignals;

#[cfg(not(unix))]
impl OsSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn next(&mut self) -> i32 {
        let _ = tokio::signal::ctrl_c().await;
        info!("Ctrl+C received.");
        130
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn only_the_first_request_reports_true() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.is_requested());
        assert!(shutdown.request());
        assert!(!shutdown.request());
        assert!(shutdown.is_requested());
    }

    #[tokio::test]
    async fn request_before_wait_is_latched() {
        let shutdown = Shutdown::new();
        shutdown.request();
        tokio::time::timeout(Duration::from_secs(1), shutdown.requested())
            .await
            .expect("latched request must resolve immediately");
    }

    #[tokio::test]
    async fn waiters_wake_on_request() {
        let shutdown = Shutdown::new();
        let waiter = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { shutdown.requested().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        shutdown.request();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter must wake")
            .unwrap();
    }
}
