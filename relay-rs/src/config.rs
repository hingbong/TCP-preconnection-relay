//! Configuration — supports TOML file, environment variables, and CLI overrides.
//! Merge priority: CLI > env > TOML file > defaults.

use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;

/// Complete relay configuration.
/// All fields have `#[serde(default)]` so a TOML file only needs to specify what it overrides.
#[derive(Deserialize, Clone)]
pub struct Config {
    // ── Required (validated after merge) ─────────────────────
    #[serde(default)]
    pub local_ip: String,
    #[serde(default)]
    pub local_port: u16,
    #[serde(default)]
    pub remote_ip: String,
    #[serde(default)]
    pub remote_tcp_port: u16,
    #[serde(default)]
    pub remote_udp_port: u16,

    // ── Pool ─────────────────────────────────────────────────
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
    #[serde(default = "default_refill_batch")]
    pub refill_batch: usize,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
    #[serde(default = "default_half_close_timeout")]
    pub half_close_timeout: u64,
    #[serde(default = "default_preconnect_ttl_ms")]
    pub preconnect_ttl_ms: u64,
    #[serde(default = "default_splice_chunk")]
    pub splice_chunk: usize,

    // ── UDP ──────────────────────────────────────────────────
    #[serde(default = "default_udp_idle_timeout")]
    pub udp_idle_timeout: u64,
    #[serde(default = "default_udp_socket_buffer")]
    pub udp_socket_buffer: usize,

    // ── Listen ───────────────────────────────────────────────
    #[serde(default = "default_listen_backlog")]
    pub listen_backlog: i32,

    // ── Logging ──────────────────────────────────────────────
    #[serde(default = "default_log_rate_per_sec")]
    pub log_rate_per_sec: usize,
    #[serde(default = "default_log_enable")]
    pub log_enable: bool,

    // ── TCP Keepalive ────────────────────────────────────────
    #[serde(default = "default_tcp_keepidle")]
    pub tcp_keepidle: i32,
    #[serde(default = "default_tcp_keepintvl")]
    pub tcp_keepintvl: i32,
    #[serde(default = "default_tcp_keepcnt")]
    pub tcp_keepcnt: i32,
    #[serde(default = "default_tcp_user_timeout_ms")]
    pub tcp_user_timeout_ms: i32,
}

// ── Serde default functions ────────────────────────────────────
fn default_pool_size() -> usize {
    24
}
fn default_refill_batch() -> usize {
    2
}
fn default_connect_timeout() -> u64 {
    5
}
fn default_idle_timeout() -> u64 {
    240
}
fn default_half_close_timeout() -> u64 {
    10
}
fn default_preconnect_ttl_ms() -> u64 {
    300_000
}
fn default_splice_chunk() -> usize {
    65_536
}
fn default_udp_idle_timeout() -> u64 {
    60
}
fn default_udp_socket_buffer() -> usize {
    4_194_304
}
fn default_listen_backlog() -> i32 {
    16_384
}
fn default_log_rate_per_sec() -> usize {
    24
}
fn default_log_enable() -> bool {
    true
}
fn default_tcp_keepidle() -> i32 {
    360
}
fn default_tcp_keepintvl() -> i32 {
    15
}
fn default_tcp_keepcnt() -> i32 {
    1
}
fn default_tcp_user_timeout_ms() -> i32 {
    0
}

impl Default for Config {
    fn default() -> Self {
        Self {
            local_ip: String::new(),
            local_port: 0,
            remote_ip: String::new(),
            remote_tcp_port: 0,
            remote_udp_port: 0,
            pool_size: default_pool_size(),
            refill_batch: default_refill_batch(),
            connect_timeout: default_connect_timeout(),
            idle_timeout: default_idle_timeout(),
            half_close_timeout: default_half_close_timeout(),
            preconnect_ttl_ms: default_preconnect_ttl_ms(),
            splice_chunk: default_splice_chunk(),
            udp_idle_timeout: default_udp_idle_timeout(),
            udp_socket_buffer: default_udp_socket_buffer(),
            listen_backlog: default_listen_backlog(),
            log_rate_per_sec: default_log_rate_per_sec(),
            log_enable: default_log_enable(),
            tcp_keepidle: default_tcp_keepidle(),
            tcp_keepintvl: default_tcp_keepintvl(),
            tcp_keepcnt: default_tcp_keepcnt(),
            tcp_user_timeout_ms: default_tcp_user_timeout_ms(),
        }
    }
}

impl Config {
    /// Load from a TOML file. Missing fields stay at defaults.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, String> {
        let content = fs::read_to_string(path.as_ref())
            .map_err(|e| format!("cannot read config file {:?}: {e}", path.as_ref()))?;
        toml::from_str(&content).map_err(|e| format!("invalid TOML in {:?}: {e}", path.as_ref()))
    }

    /// Override fields from environment variables (UPPER_SNAKE_CASE).
    /// Only overrides if the env var is set; does NOT error on missing env.
    pub fn apply_env_overrides(&mut self) {
        apply_env_string("LOCAL_IP", &mut self.local_ip);
        apply_env_u16("LOCAL_PORT", &mut self.local_port);
        apply_env_string("REMOTE_IP", &mut self.remote_ip);
        apply_env_u16("REMOTE_TCP_PORT", &mut self.remote_tcp_port);
        apply_env_u16("REMOTE_UDP_PORT", &mut self.remote_udp_port);

        apply_env_usize("POOL_SIZE", &mut self.pool_size);
        apply_env_usize("REFILL_BATCH", &mut self.refill_batch);
        apply_env_u64("CONNECT_TIMEOUT", &mut self.connect_timeout);
        apply_env_u64("IDLE_TIMEOUT", &mut self.idle_timeout);
        apply_env_u64("HALF_CLOSE_TIMEOUT", &mut self.half_close_timeout);
        apply_env_u64("PRECONNECT_TTL_MS", &mut self.preconnect_ttl_ms);
        apply_env_usize("SPLICE_CHUNK", &mut self.splice_chunk);
        apply_env_u64("UDP_IDLE_TIMEOUT", &mut self.udp_idle_timeout);
        apply_env_usize("UDP_SOCKET_BUFFER", &mut self.udp_socket_buffer);
        apply_env_i32("LISTEN_BACKLOG", &mut self.listen_backlog);
        apply_env_usize("LOG_RATE_PER_SEC", &mut self.log_rate_per_sec);
        apply_env_bool("LOG_ENABLE", &mut self.log_enable);

        apply_env_i32("TCP_KEEPIDLE", &mut self.tcp_keepidle);
        apply_env_i32("TCP_KEEPINTVL", &mut self.tcp_keepintvl);
        apply_env_i32("TCP_KEEPCNT", &mut self.tcp_keepcnt);
        apply_env_i32("TCP_USER_TIMEOUT_MS", &mut self.tcp_user_timeout_ms);
    }

    /// Validate required fields after merge.
    pub fn validate(mut self) -> Result<Self, String> {
        if self.local_ip.is_empty() {
            return Err("local_ip is required (set LOCAL_IP env or local_ip in TOML)".into());
        }
        if self.local_port == 0 {
            return Err("local_port is required (set LOCAL_PORT env or local_port in TOML)".into());
        }
        if self.remote_ip.is_empty() {
            return Err("remote_ip is required (set REMOTE_IP env or remote_ip in TOML)".into());
        }
        if self.remote_tcp_port == 0 {
            return Err(
                "remote_tcp_port is required (set REMOTE_TCP_PORT env or remote_tcp_port in TOML)"
                    .into(),
            );
        }
        if self.remote_udp_port == 0 {
            return Err(
                "remote_udp_port is required (set REMOTE_UDP_PORT env or remote_udp_port in TOML)"
                    .into(),
            );
        }
        if self.pool_size > 256 {
            return Err("pool_size must be <= 256".into());
        }
        if self.refill_batch == 0 {
            self.refill_batch = 1;
        }
        if self.pool_size > 0 && self.refill_batch > self.pool_size {
            self.refill_batch = self.pool_size;
        }
        if !(16 * 1024..=1024 * 1024).contains(&self.splice_chunk) {
            return Err("splice_chunk must be between 16 KiB and 1 MiB".into());
        }
        if self.preconnect_ttl_ms < 10_000 {
            return Err("preconnect_ttl_ms must be >= 10000".into());
        }
        Ok(self)
    }
}

// ── Env override helpers ────────────────────────────────────────

fn apply_env_string(name: &str, target: &mut String) {
    if let Ok(val) = env::var(name) {
        *target = val;
    }
}

fn apply_env_u16(name: &str, target: &mut u16) {
    if let Ok(val) = env::var(name) {
        if let Ok(v) = val.parse() {
            *target = v;
        } else {
            eprintln!("WARN: invalid {name}={val}, keeping {target}");
        }
    }
}

fn apply_env_u64(name: &str, target: &mut u64) {
    if let Ok(val) = env::var(name) {
        if let Ok(v) = val.parse() {
            *target = v;
        } else {
            eprintln!("WARN: invalid {name}={val}, keeping {target}");
        }
    }
}

fn apply_env_usize(name: &str, target: &mut usize) {
    if let Ok(val) = env::var(name) {
        if let Ok(v) = val.parse() {
            *target = v;
        } else {
            eprintln!("WARN: invalid {name}={val}, keeping {target}");
        }
    }
}

fn apply_env_i32(name: &str, target: &mut i32) {
    if let Ok(val) = env::var(name) {
        if let Ok(v) = val.parse() {
            *target = v;
        } else {
            eprintln!("WARN: invalid {name}={val}, keeping {target}");
        }
    }
}

fn apply_env_bool(name: &str, target: &mut bool) {
    if let Ok(val) = env::var(name) {
        match val.as_str() {
            "0" | "false" | "no" | "off" => *target = false,
            "1" | "true" | "yes" | "on" => *target = true,
            _ => eprintln!("WARN: invalid {name}={val}, keeping {target}"),
        }
    }
}
