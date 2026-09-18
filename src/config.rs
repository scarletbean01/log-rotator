//! CLI configuration (clap derive).

use clap::Parser;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "log-sidecar",
    version,
    about = "Tail/grep/stream large logs over HTTP"
)]
pub struct Config {
    /// Root directory containing log files
    #[arg(long, default_value = "/var/log/tomcat")]
    pub root: PathBuf,

    /// Bind address (IPv4 or IPv6)
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,

    /// Listen port
    #[arg(long, default_value_t = 9090)]
    pub port: u16,

    /// Optional bearer token (unreserved URL characters only: A-Z a-z 0-9 _ . - ~)
    #[arg(long)]
    pub token: Option<String>,

    /// Max concurrent searches
    #[arg(long, default_value_t = 2)]
    pub max_searches: usize,

    /// Max concurrent live streams
    #[arg(long, default_value_t = 8)]
    pub max_streams: usize,

    /// Allow binding to a non-loopback interface
    #[arg(long)]
    pub allow_public: bool,

    /// Read chunk size in bytes (4096..=8388608)
    #[arg(long, default_value_t = 65536)]
    pub chunk_size: usize,
}

/// Token charset: URL-unreserved only, so the token works identically in the
/// `Authorization` header and in `?token=` (EventSource cannot set headers
/// and the query arm is not percent-decoded).
const TOKEN_CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.~-";

const MIN_CHUNK_SIZE: usize = 4096;
const MAX_CHUNK_SIZE: usize = 8 * 1024 * 1024;

impl Config {
    /// Validate flag combinations; returns a human-readable startup error.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_searches == 0 {
            return Err("--max-searches must be > 0".into());
        }
        if self.max_streams == 0 {
            return Err("--max-streams must be > 0".into());
        }
        if self.chunk_size < MIN_CHUNK_SIZE || self.chunk_size > MAX_CHUNK_SIZE {
            return Err(format!(
                "--chunk-size must be between {MIN_CHUNK_SIZE} and {MAX_CHUNK_SIZE} bytes"
            ));
        }
        if let Some(token) = &self.token {
            if token.is_empty() {
                return Err("--token must not be empty".into());
            }
            if !token.chars().all(|c| TOKEN_CHARS.contains(c)) {
                return Err(
                    "--token may only contain unreserved URL characters (A-Z a-z 0-9 _ . - ~)"
                        .into(),
                );
            }
        }
        // Always parse (even with --allow-public) so a garbage address fails
        // here with a clean message instead of panicking in socket_addr().
        let ip: IpAddr = self
            .bind
            .parse()
            .map_err(|e| format!("invalid --bind address {:?}: {e}", self.bind))?;
        if !self.allow_public && !ip.is_loopback() {
            return Err("binding to a non-loopback interface requires --allow-public".into());
        }
        Ok(())
    }

    /// Parsed socket address. Never panics: `validate()` checks the bind
    /// address, but this stays fallible so callers cannot panic by skipping it.
    pub fn socket_addr(&self) -> Result<SocketAddr, String> {
        let ip: IpAddr = self
            .bind
            .parse()
            .map_err(|e| format!("invalid --bind address {:?}: {e}", self.bind))?;
        Ok(SocketAddr::new(ip, self.port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cfg(args: &[&str]) -> Config {
        Config::try_parse_from(std::iter::once("log-sidecar").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn defaults_are_valid() {
        assert!(cfg(&[]).validate().is_ok());
    }

    #[test]
    fn ipv6_loopback_address_parses() {
        // Regression: `format!("{ip}:{port}")` used to produce "::1:9090",
        // which is not a valid socket address string.
        let c = cfg(&["--bind", "::1"]);
        c.validate().unwrap();
        assert_eq!(
            c.socket_addr().unwrap(),
            "[::1]:9090".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn garbage_bind_rejected_even_with_allow_public() {
        assert!(
            cfg(&["--bind", "not-an-ip", "--allow-public"])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn public_bind_requires_flag() {
        assert!(cfg(&["--bind", "0.0.0.0"]).validate().is_err());
        assert!(
            cfg(&["--bind", "0.0.0.0", "--allow-public"])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn chunk_size_bounds_enforced() {
        assert!(cfg(&["--chunk-size", "4095"]).validate().is_err());
        assert!(cfg(&["--chunk-size", "4096"]).validate().is_ok());
        assert!(cfg(&["--chunk-size", "8388608"]).validate().is_ok());
        assert!(cfg(&["--chunk-size", "8388609"]).validate().is_err());
    }

    #[test]
    fn empty_token_rejected() {
        assert!(cfg(&["--token", ""]).validate().is_err());
    }

    #[test]
    fn token_charset_enforced() {
        // A space would never match via the non-decoded `?token=` arm.
        assert!(cfg(&["--token", "has space"]).validate().is_err());
        assert!(cfg(&["--token", "has=eq"]).validate().is_err());
        assert!(cfg(&["--token", "s3cret_X-1.~"]).validate().is_ok());
    }

    #[test]
    fn zero_concurrency_rejected() {
        assert!(cfg(&["--max-searches", "0"]).validate().is_err());
        assert!(cfg(&["--max-streams", "0"]).validate().is_err());
    }
}
