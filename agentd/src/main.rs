//! Daemon entrypoint.
//!
//! Runs as the container `CMD` inside an AWS Lambda MicroVM. Being `CMD` is what
//! makes an omitted `cwd` inherit the image `WORKDIR`, and it is also the
//! unenforced half of the bootstrap trust boundary — see the crate docs.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use agentd::{AppState, Config, disk, exec, identity, routes, serve};
use tokio::net::TcpListener;

fn main() -> std::io::Result<()> {
    init_tracing();

    let config = Config::from_env();

    // A current-thread runtime with a small blocking pool: the workload is
    // I/O-bound, and on a VM whose baseline can be 512 MiB the default cap of
    // 512 blocking threads at 2 MiB of stack each is a way to die under a burst
    // of uploads rather than a way to go fast. Blocking threads still exist
    // under a current-thread runtime, which is what the tar and file paths use.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()?;

    runtime.block_on(run(config))
}

async fn run(config: Config) -> std::io::Result<()> {
    let port = config.port;

    // Identity repair runs before the listener is bound, so no request can observe
    // the image's shared machine-id even briefly. It is safe to do here and nowhere
    // later precisely because the daemon is the container `CMD`: nothing in the VM
    // has read these values yet. A workload started first would already hold its own
    // copy, and no amount of rewriting would recall it.
    //
    // Every failure inside is logged and swallowed. Refusing to serve because a bind
    // mount was denied would strand a VM with no way in — see `identity`.
    let identity = if config.repair_identity {
        identity::repair(&identity::Layout::default(), &identity::Host)
    } else {
        tracing::info!(
            "identity repair disabled by configuration; this VM keeps the image's \
             machine-id, hostname, and boot_id, which are shared with every other VM \
             derived from the same snapshot"
        );
        identity::Report::skipped()
    };

    let state = AppState::with_probe(config, disk::available_bytes, identity);

    // Collection of acked exec entries runs on its own interval rather than
    // inside a request handler, so a slow collection cannot delay a response and
    // a client that stops polling cannot pin output forever.
    let collector = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let collected = exec::collect_expired(&collector);
            if collected > 0 {
                tracing::debug!(collected, "collected expired exec entries");
            }
        }
    });

    let app = routes::app(state);

    // Binding all interfaces is correct here rather than a widening: the
    // platform's endpoint proxy terminates outside the VM and forwards inward,
    // and there is no unauthenticated internet path to this port — every request
    // through the endpoint carries a JWE scoped to this MicroVM and port set.
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, version = routes::VERSION, "agentd listening");

    serve::serve(listener, app).await
}

/// JSON logs to stdout, which is where the platform's CloudWatch capture reads
/// from. The log group is `/aws/lambda-microvms/<image-name>`; an IAM policy
/// granting the wrong prefix silently discards every line, which is how a run of
/// failed builds all reported `reason=unknown`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("AGENTD_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}
