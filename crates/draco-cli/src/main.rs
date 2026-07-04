//! # draco (CLI — STUB WS-D)
//!
//! Command-line interface + output contract. Implement against canonical spec §12:
//! the `--extract <JSONPATH>` filter and status→exit-code mapping still need work.
//! The skeleton parses args and prints a well-formed `ExtractionResult`.

use clap::{Parser, Subcommand};
use draco_core::{extract, Config};

#[derive(Parser)]
#[command(
    name = "draco",
    version,
    about = "Browserless, tiered data-extraction engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract structured data from a URL.
    Extract {
        /// Target URL.
        url: String,
        /// JSONPath filter applied to `.data` before printing (WS-D).
        #[arg(long)]
        extract: Option<String>,
        /// http/https/socks5 proxy URL.
        #[arg(long)]
        proxy: Option<String>,
        /// Minimum per-host inter-request delay (ms).
        #[arg(long, default_value_t = 0)]
        delay: u64,
        /// Total request timeout (ms).
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
        /// Cap the escalation ladder (0, 1, or 2).
        #[arg(long, default_value_t = 2)]
        tier_max: u8,
        /// Tier 2 capture-window duration (ms).
        #[arg(long, default_value_t = 2_000)]
        capture_window_ms: u64,
        /// Dev-only: run Tier 2 un-jailed.
        #[arg(long)]
        no_jail: bool,
        /// Bypass robots.txt.
        #[arg(long)]
        ignore_robots: bool,
        /// Pretty-print the JSON output.
        #[arg(long)]
        pretty: bool,
    },
    /// Internal: jailed child entry (self-re-exec target). Hidden.
    #[command(name = "__jail", hide = true)]
    Jail,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Extract {
            url,
            extract: _extract,
            proxy,
            delay,
            timeout,
            tier_max,
            capture_window_ms,
            no_jail,
            ignore_robots,
            pretty,
        } => {
            let config = Config {
                proxy,
                delay_ms: delay,
                timeout_ms: timeout,
                respect_robots: !ignore_robots,
                tier_max,
                capture_window_ms,
                no_jail,
            };
            let result = extract(&url, &config).await;
            // TODO(WS-D): apply the --extract JSONPath filter to result.data.
            let json = if pretty {
                serde_json::to_string_pretty(&result).expect("serialize result")
            } else {
                serde_json::to_string(&result).expect("serialize result")
            };
            println!("{json}");
            // TODO(WS-D): map result.status → exit code (0/2/3/1) per spec §12.
        }
        Command::Jail => {
            // TODO(Slice 2): draco_jail::run_jail_child();
            eprintln!("draco __jail: not implemented (Slice 2 spike)");
            std::process::exit(1);
        }
    }
}
