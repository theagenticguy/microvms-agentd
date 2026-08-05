//! Serving the router, over either a real socket or a simulated one.
//!
//! Since axum 0.8, `axum::serve` accepts anything implementing
//! `axum::serve::Listener`. `tokio::net::TcpListener` already does, and a turmoil
//! listener needs only a small newtype in the test tier. So the daemon's serve
//! path is generic and the production binary never links the simulator.

use std::fmt::Debug;

use axum::Router;
use axum::serve::Listener;

/// Serves `app` on `listener` until the process is asked to stop.
///
/// The `L::Addr: Debug` bound is required by axum 0.8: without it the returned
/// `Serve` does not implement `Future`, and the compiler reports that rather than
/// the missing bound.
pub async fn serve<L>(listener: L, app: Router) -> std::io::Result<()>
where
    L: Listener + Send + 'static,
    L::Addr: Debug,
{
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

/// Resolves when the platform asks the VM to stop.
///
/// The MicroVM lifecycle sends `/terminate` through the hook route, but the
/// process can also be signalled directly, so both paths are handled. In-flight
/// requests drain rather than being cut, which matters because a harness waiting
/// on `/exec/{id}` should get its status rather than a transport error.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(err) => {
                tracing::warn!(%err, "cannot listen for SIGTERM; graceful shutdown disabled");
                std::future::pending::<()>().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received; draining"),
            result = tokio::signal::ctrl_c() => match result {
                Ok(()) => tracing::info!("interrupt received; draining"),
                Err(err) => tracing::warn!(%err, "interrupt listener failed"),
            },
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
