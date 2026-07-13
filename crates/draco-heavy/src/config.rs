use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::discovery::default_cache_path;

const DEFAULT_CACHE_TTL_SECS: u64 = 24 * 60 * 60;

/// `draco-heavy` is expected to sit behind a cloudflared tunnel protected by a
/// Cloudflare Access service token. It does not expose or configure that tunnel.
#[derive(Debug, Parser)]
#[command(name = "draco-heavy", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub serve: ServeArgs,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Resolve and cache the best host rendering configuration.
    Discover {
        /// Ignore a valid cache entry and probe the host again.
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    /// Address to bind. Keep loopback unless a trusted tunnel terminates locally.
    #[arg(long, env = "DRACO_HEAVY_BIND", default_value = "127.0.0.1")]
    pub bind: IpAddr,

    #[arg(long, env = "DRACO_HEAVY_PORT", default_value_t = 3003)]
    pub port: u16,

    #[arg(long, env = "DRACO_HEAVY_SLOTS", default_value_t = 8)]
    pub slots: usize,

    #[arg(long, env = "DRACO_HEAVY_CACHE_PATH")]
    pub cache_path: Option<PathBuf>,

    #[arg(
        long,
        env = "DRACO_HEAVY_CACHE_TTL_SECS",
        default_value_t = DEFAULT_CACHE_TTL_SECS
    )]
    pub cache_ttl_secs: u64,

    #[cfg(feature = "pipe")]
    #[arg(long, env = "DRACO_HEAVY_RELAY_BASE_PORT", default_value_t = 41_000)]
    pub relay_base_port: u16,

    #[cfg(feature = "pipe")]
    #[arg(long, env = "DRACO_HEAVY_TUN2SOCKS_BIN", default_value = "tun2socks")]
    pub tun2socks_bin: PathBuf,

    #[cfg(feature = "pipe")]
    #[arg(long, env = "DRACO_HEAVY_HOST_PUBLIC_IP")]
    pub host_public_ip: Option<IpAddr>,

    #[cfg(feature = "pipe")]
    #[arg(
        long,
        env = "DRACO_HEAVY_TCP_ECHO_URL",
        default_value = "https://api.ipify.org"
    )]
    pub tcp_echo_url: String,

    #[cfg(feature = "pipe")]
    #[arg(
        long,
        env = "DRACO_HEAVY_QUIC_ECHO_URL",
        default_value = "https://cloudflare-quic.com/cdn-cgi/trace"
    )]
    pub quic_echo_url: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub slots: usize,
    pub cache_path: PathBuf,
    pub cache_ttl_secs: u64,
    #[cfg(feature = "pipe")]
    pub relay_base_port: u16,
    #[cfg(feature = "pipe")]
    pub tun2socks_bin: PathBuf,
    #[cfg(feature = "pipe")]
    pub host_public_ip: Option<IpAddr>,
    #[cfg(feature = "pipe")]
    pub tcp_echo_url: String,
    #[cfg(feature = "pipe")]
    pub quic_echo_url: String,
}

impl Config {
    pub fn from_args(args: ServeArgs) -> Self {
        Self {
            bind: SocketAddr::new(args.bind, args.port),
            slots: args.slots.max(1),
            cache_path: args.cache_path.unwrap_or_else(default_cache_path),
            cache_ttl_secs: args.cache_ttl_secs,
            #[cfg(feature = "pipe")]
            relay_base_port: args.relay_base_port,
            #[cfg(feature = "pipe")]
            tun2socks_bin: args.tun2socks_bin,
            #[cfg(feature = "pipe")]
            host_public_ip: args.host_public_ip,
            #[cfg(feature = "pipe")]
            tcp_echo_url: args.tcp_echo_url,
            #[cfg(feature = "pipe")]
            quic_echo_url: args.quic_echo_url,
        }
    }

    #[cfg(feature = "pipe")]
    pub fn pipe_config(&self) -> Result<crate::pipe::PipeConfig, String> {
        let host_public_ip = self.host_public_ip.ok_or_else(|| {
            "--host-public-ip (or DRACO_HEAVY_HOST_PUBLIC_IP) is required in pipe mode".to_string()
        })?;
        Ok(crate::pipe::PipeConfig {
            relay_base_port: self.relay_base_port,
            tun2socks: crate::pipe::namespace::Tun2SocksSpec {
                binary: self.tun2socks_bin.clone(),
                extra_args: Vec::new(),
            },
            host_public_ip,
            tcp_echo_url: self.tcp_echo_url.clone(),
            quic_echo_url: self.quic_echo_url.clone(),
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3003),
            slots: 8,
            cache_path: default_cache_path(),
            cache_ttl_secs: DEFAULT_CACHE_TTL_SECS,
            #[cfg(feature = "pipe")]
            relay_base_port: 41_000,
            #[cfg(feature = "pipe")]
            tun2socks_bin: PathBuf::from("tun2socks"),
            #[cfg(feature = "pipe")]
            host_public_ip: None,
            #[cfg(feature = "pipe")]
            tcp_echo_url: "https://api.ipify.org".into(),
            #[cfg(feature = "pipe")]
            quic_echo_url: "https://cloudflare-quic.com/cdn-cgi/trace".into(),
        }
    }
}
