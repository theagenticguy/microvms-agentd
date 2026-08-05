//! Runtime configuration.
//!
//! Every bound here exists because an unbounded version of it was a defect in the
//! Python predecessor. The MicroVM's baseline memory can be as low as 512 MiB and
//! an OOM-killed daemon is unrecoverable — the platform forwards no traffic to a
//! dead process and there is no supervisor inside the VM to restart it.

use std::time::Duration;

/// Daemon configuration. Defaults are sized for a 512 MiB baseline VM.
#[derive(Clone, Debug)]
pub struct Config {
    /// Port the control API and lifecycle hooks listen on.
    pub port: u16,
    /// Largest request body accepted on the wire, enforced by
    /// `tower_http::limit::RequestBodyLimitLayer` because `DefaultBodyLimit` does
    /// not apply to bodies consumed as a stream.
    pub max_body_bytes: usize,
    /// Bytes of a rejected request's body the daemon will drain before closing.
    /// Draining lets a client's pooled connection survive an error response;
    /// draining without a cap is itself a denial-of-service vector, so anything
    /// larger closes instead.
    pub max_drain_bytes: usize,
    /// Per-stream cap on captured exec output. Exceeding it truncates and sets a
    /// marker on the result rather than growing until the daemon is killed.
    pub max_output_bytes: usize,
    /// How long to keep reading an exec's pipes after the direct child exits.
    /// Grandchildren inherit the pipe, so EOF can lag the child's exit; without a
    /// deadline a backgrounded process would hold the exec open indefinitely.
    pub output_linger: Duration,
    /// How long an acked exec entry is retained before collection.
    pub exec_ttl: Duration,
    /// Grace period between SIGTERM and SIGKILL when killing an exec's process
    /// group.
    pub kill_grace: Duration,
    /// Maximum members in an uploaded archive.
    pub max_tar_members: u64,
    /// Maximum total uncompressed bytes across an archive's members.
    pub max_tar_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 9000,
            max_body_bytes: 512 * 1024 * 1024,
            max_drain_bytes: 64 * 1024,
            max_output_bytes: 8 * 1024 * 1024,
            output_linger: Duration::from_secs(5),
            exec_ttl: Duration::from_secs(15 * 60),
            kill_grace: Duration::from_secs(10),
            max_tar_members: 100_000,
            max_tar_bytes: 8 * 1024 * 1024 * 1024,
        }
    }
}

impl Config {
    /// Reads overrides from the environment, keeping the defaults for anything
    /// unset or unparseable. An unparseable value is a warning rather than a
    /// startup failure: refusing to boot would strand the VM with no way in,
    /// since the control API is the only channel.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Some(port) = env_parse("AGENTD_PORT") {
            cfg.port = port;
        }
        if let Some(bytes) = env_parse("AGENTD_MAX_BODY_BYTES") {
            cfg.max_body_bytes = bytes;
        }
        if let Some(bytes) = env_parse("AGENTD_MAX_OUTPUT_BYTES") {
            cfg.max_output_bytes = bytes;
        }
        if let Some(secs) = env_parse::<u64>("AGENTD_OUTPUT_LINGER_SECS") {
            cfg.output_linger = Duration::from_secs(secs);
        }
        if let Some(secs) = env_parse::<u64>("AGENTD_EXEC_TTL_SECS") {
            cfg.exec_ttl = Duration::from_secs(secs);
        }
        cfg
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    let raw = std::env::var(key).ok()?;
    match raw.parse() {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::warn!(key, raw, "ignoring unparseable configuration value");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sized_for_a_small_vm() {
        let cfg = Config::default();
        assert_eq!(cfg.port, 9000);
        // The output cap has to be well under the smallest baseline memory, or a
        // single noisy command can kill a daemon nothing can restart.
        assert!(cfg.max_output_bytes < 64 * 1024 * 1024);
        assert!(cfg.max_drain_bytes < cfg.max_body_bytes);
    }
}
