use std::time::Duration;

use clap::Parser;
use draco_heavy::config::{Cli, Command, Config};
use draco_heavy::discovery::resolve;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("draco-heavy: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let config = Config::from_args(cli.serve);
    let ttl = Duration::from_secs(config.cache_ttl_secs);

    match cli.command {
        Some(Command::Discover { refresh }) => {
            let resolved = resolve(&config.cache_path, ttl, refresh);
            if let Some(error) = &resolved.cache_error {
                eprintln!(
                    "draco-heavy: warning: could not persist discovery cache {}: {error}",
                    config.cache_path.display()
                );
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&resolved.config)
                    .map_err(|error| format!("serialize host config: {error}"))?
            );
            Ok(())
        }
        None => {
            #[cfg(feature = "pipe")]
            {
                let resolved = resolve(&config.cache_path, ttl, false);
                if let Some(error) = &resolved.cache_error {
                    eprintln!(
                        "draco-heavy: warning: discovery cache unavailable at {}: {error}",
                        config.cache_path.display()
                    );
                }
                draco_heavy::serve(config, resolved).await
            }
            #[cfg(not(feature = "pipe"))]
            {
                Err("daemon mode requires the `pipe` feature; use the library API for local fallback"
                    .into())
            }
        }
    }
}
