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
    /// Bytes of recent output each exec keeps for stream replay. Independent of
    /// `max_output_bytes`: that one caps the *head* of the log for the polled
    /// result, this one caps the *tail* for a reattaching stream. A reattach past
    /// this window gets an explicit gap event rather than silently-missing bytes.
    pub stream_buffer_bytes: usize,
    /// Slots in an exec's live fan-out channel. A subscriber that falls this far
    /// behind is told it lagged and re-reads the ring from its last good offset,
    /// so the bound costs a re-read rather than lost output.
    pub stream_channel_capacity: usize,
    /// Interval between SSE keep-alive comments. Needed because an exec can be
    /// silent for minutes — an agent harness thinking, a build linking — and an
    /// idle connection through a proxy is indistinguishable from a dead one.
    pub sse_keepalive: Duration,
    /// Largest single decoded stdin write accepted. Bounded for the same reason
    /// every other buffer here is: the write is held in memory before it reaches
    /// the pipe.
    pub max_stdin_write_bytes: usize,
    /// How long a stdin write may block on the pipe before the request gives up.
    /// A child that has stopped reading fills the 64 KiB pipe buffer and then the
    /// write blocks forever; without this bound one wedged child pins a request
    /// (and a connection) for the life of the VM.
    pub stdin_write_timeout: Duration,
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
            stream_buffer_bytes: 1024 * 1024,
            stream_channel_capacity: 256,
            sse_keepalive: Duration::from_secs(15),
            max_stdin_write_bytes: 1024 * 1024,
            stdin_write_timeout: Duration::from_secs(5),
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
        if let Some(bytes) = env_parse("AGENTD_STREAM_BUFFER_BYTES") {
            cfg.stream_buffer_bytes = bytes;
        }
        if let Some(slots) = env_parse("AGENTD_STREAM_CHANNEL_CAPACITY") {
            cfg.stream_channel_capacity = slots;
        }
        if let Some(secs) = env_parse::<u64>("AGENTD_SSE_KEEPALIVE_SECS") {
            cfg.sse_keepalive = Duration::from_secs(secs);
        }
        if let Some(bytes) = env_parse("AGENTD_MAX_STDIN_WRITE_BYTES") {
            cfg.max_stdin_write_bytes = bytes;
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
