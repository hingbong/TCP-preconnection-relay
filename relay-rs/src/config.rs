//! Configuration parsed from environment variables.
//! Mirrors the C version's env-var interface exactly.

use std::env;

pub struct Config {
    pub local_ip: String,
    pub local_port: u16,
    pub remote_ip: String,
    pub remote_tcp_port: u16,
    pub remote_udp_port: u16,

    pub pool_size: usize,
    pub refill_batch: usize,
    pub connect_timeout: u64,
    pub idle_timeout: u64,
    pub half_close_timeout: u64,
    pub preconnect_ttl_ms: u64,
    pub splice_chunk: usize,
    pub udp_idle_timeout: u64,
    pub udp_socket_buffer: usize,
    pub listen_backlog: i32,
    pub log_rate_per_sec: usize,
    pub log_enable: bool,

    pub tcp_keepidle: i32,
    pub tcp_keepintvl: i32,
    pub tcp_keepcnt: i32,
    pub tcp_user_timeout_ms: i32,
}

fn parse_env_int(name: &str, def: i64, min: i64, max: i64) -> i64 {
    match env::var(name) {
        Ok(val) => val.parse::<i64>().unwrap_or_else(|_| {
            eprintln!("WARN: invalid {name}={val}, using {def}");
            def
        }),
        Err(_) => def,
    }
    .clamp(min, max)
}

fn parse_env_bool(name: &str, def: bool) -> bool {
    match env::var(name) {
        Ok(val) => match val.as_str() {
            "0" | "false" | "no" | "off" => false,
            "1" | "true" | "yes" | "on" => true,
            _ => {
                eprintln!("WARN: invalid {name}={val}, using {def}");
                def
            }
        },
        Err(_) => def,
    }
}

impl Config {
    pub fn from_env() -> Self {
        let pool_size = parse_env_int("POOL_SIZE", 24, 0, 256) as usize;
        let raw_refill = parse_env_int("REFILL_BATCH", 8, 1, 256) as usize;
        let refill_batch = if raw_refill > pool_size && pool_size > 0 {
            pool_size
        } else {
            raw_refill
        };

        Self {
            local_ip: env::var("LOCAL_IP").expect("LOCAL_IP not set"),
            local_port: env::var("LOCAL_PORT")
                .expect("LOCAL_PORT not set")
                .parse()
                .expect("LOCAL_PORT must be integer"),
            remote_ip: env::var("REMOTE_IP").expect("REMOTE_IP not set"),
            remote_tcp_port: env::var("REMOTE_TCP_PORT")
                .expect("REMOTE_TCP_PORT not set")
                .parse()
                .expect("REMOTE_TCP_PORT must be integer"),
            remote_udp_port: env::var("REMOTE_UDP_PORT")
                .expect("REMOTE_UDP_PORT not set")
                .parse()
                .expect("REMOTE_UDP_PORT must be integer"),

            pool_size,
            refill_batch,
            connect_timeout: parse_env_int("CONNECT_TIMEOUT", 5, 1, 120) as u64,
            idle_timeout: parse_env_int("IDLE_TIMEOUT", 240, 30, 86400) as u64,
            half_close_timeout: parse_env_int("HALF_CLOSE_TIMEOUT", 10, 1, 300) as u64,
            preconnect_ttl_ms: parse_env_int("PRECONNECT_TTL_MS", 50000, 10000, 3600000) as u64,
            splice_chunk: parse_env_int("SPLICE_CHUNK", 262144, 16384, 1048576) as usize,
            udp_idle_timeout: parse_env_int("UDP_IDLE_TIMEOUT", 60, 5, 3600) as u64,
            udp_socket_buffer: parse_env_int("UDP_SOCKET_BUFFER", 4194304, 65536, 67108864)
                as usize,
            listen_backlog: parse_env_int("LISTEN_BACKLOG", 16384, 128, 65535) as i32,
            log_rate_per_sec: parse_env_int("LOG_RATE_PER_SEC", 24, 0, 10000) as usize,
            log_enable: parse_env_bool("LOG_ENABLE", true),

            tcp_keepidle: parse_env_int("TCP_KEEPIDLE", 360, 30, 86400) as i32,
            tcp_keepintvl: parse_env_int("TCP_KEEPINTVL", 15, 1, 3600) as i32,
            tcp_keepcnt: parse_env_int("TCP_KEEPCNT", 1, 1, 30) as i32,
            tcp_user_timeout_ms: parse_env_int("TCP_USER_TIMEOUT_MS", 0, 0, 3600000) as i32,
        }
    }
}
