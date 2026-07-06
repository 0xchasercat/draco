//! # draco (CLI)
//!
//! Command-line interface + output contract. Draco is first a **URL → Markdown
//! scraper** (Firecrawl-style): `draco scrape <url>` fetches a page and prints
//! clean Markdown of its main content to stdout. `--format json` switches to the
//! tiered JSON-API extraction (embedded state → build-id replay → runtime
//! interception), and `--format both` returns Markdown, metadata, and JSON in
//! one envelope. `--format` is repeatable, so any combination of
//! markdown/html/raw-html/links/json/endpoints may be requested together
//! (mirroring the daemon's `formats: [...]` array).
//!
//! Output rules: for the default `markdown`-only format the raw Markdown string
//! is printed (pipeable: `draco scrape url > page.md`); `--json` instead prints
//! the full [`ExtractionResult`] envelope. Any other combination of formats
//! always prints the envelope. The `--extract <JSONPATH>` filter runs over
//! `result.data`, and the status→exit-code mapping (spec §12) is preserved in
//! all modes.
//!
//! Beyond `scrape`, the CLI also exposes `discover` (API endpoint discovery +
//! replay, mirroring `POST /v1/discover`) and, under the `serve` feature,
//! `map` (fast site URL discovery, mirroring `POST /v1/map`).

use clap::{Parser, Subcommand, ValueEnum};
use draco_core::{extract, Config, FormatSet};

/// MCP server — stdio transport (`draco mcp`) + HTTP binding (`POST /mcp`).
#[cfg(feature = "serve")]
mod mcp;
/// `draco serve` — persistent daemon with a Firecrawl-compatible REST API.
#[cfg(feature = "serve")]
mod serve;
use draco_types::{DracoError, ExtractionResult, SourceTier, Status, StepOutcome, TraceStep};
use serde_json::Value;
use serde_json_path::JsonPath;

#[derive(Parser)]
#[command(
    name = "draco",
    version,
    about = "URL → Markdown scraper (with an optional tiered JSON-API extraction mode)",
    long_about = "Draco — a browserless URL → Markdown scraper.\n\n\
        By default `draco scrape <url>` fetches a page and prints clean Markdown of \
        its main content (headings, links, lists, code, tables), dropping nav/header/\
        footer boilerplate — the fast, static-only path. Use `--format json` for the \
        tiered JSON-API extraction (embedded state → Next.js build-id replay → runtime \
        interception) or `--format both` to get Markdown, metadata, and JSON together."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// CLI surface for [`FormatSet`] — the multi-select `--format` flag, repeatable
/// so any combination of outputs can be requested in one invocation (e.g.
/// `--format markdown --format links`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
enum FormatArg {
    /// Clean Markdown + metadata of the page's main content (default).
    Markdown,
    /// Cleaned, absolutized HTML of the page's main content.
    Html,
    /// The unmodified fetched HTML.
    #[value(name = "raw-html", alias = "rawhtml")]
    RawHtml,
    /// Every absolutized `<a href>` on the page.
    Links,
    /// Tiered JSON-API extraction, populating `data`.
    Json,
    /// Discover the JSON/XHR API endpoints the page's JavaScript calls, ranked,
    /// and replay the best one — populates `endpoints` (and `data`).
    Endpoints,
    /// Convenience alias for `markdown` + `json` together.
    Both,
}

/// Fold repeated `--format` values into a [`FormatSet`].
///
/// An empty slice (the flag was never given) means "use the default",
/// [`FormatSet::markdown_only`]. Otherwise each `FormatArg` sets its matching
/// flag; `both` is sugar for `markdown` + `json` together (composes with any
/// other formats given alongside it, e.g. `--format both --format links`).
fn formats_from_args(args: &[FormatArg]) -> FormatSet {
    if args.is_empty() {
        return FormatSet::markdown_only();
    }
    let mut set = FormatSet::none();
    for arg in args {
        match arg {
            FormatArg::Markdown => set.markdown = true,
            FormatArg::Html => set.html = true,
            FormatArg::RawHtml => set.raw_html = true,
            FormatArg::Links => set.links = true,
            FormatArg::Json => set.json = true,
            FormatArg::Endpoints => set.endpoints = true,
            FormatArg::Both => {
                set.markdown = true;
                set.json = true;
            }
        }
    }
    set
}

#[derive(Subcommand)]
enum Command {
    /// Scrape a URL to Markdown (default), or to any combination of
    /// markdown/html/raw-html/links/json/endpoints with (repeatable)
    /// `--format`. Mirrors `POST /v1/scrape`.
    Scrape {
        /// Target URL.
        url: String,
        /// Output format(s); repeatable (e.g. `--format markdown --format
        /// links`). Defaults to `markdown` alone when omitted. `both` is sugar
        /// for `markdown` + `json`.
        #[arg(long, value_enum)]
        format: Vec<FormatArg>,
        /// Print the full ExtractionResult JSON envelope instead of the raw
        /// Markdown string, even when the only requested format is `markdown`.
        /// (No effect when any other format is also requested — the envelope
        /// is always printed then.)
        #[arg(long)]
        json: bool,
        /// JSONPath filter applied to `.data` before printing.
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
        /// Tier 2 capture-window duration (ms). Defaults to 2000 when neither
        /// this nor `--wait-for` is given; an explicit `--capture-window-ms`
        /// always wins over `--wait-for` if both are passed.
        #[arg(long)]
        capture_window_ms: Option<u64>,
        /// Firecrawl-style alias for `--capture-window-ms` (ms to wait for the
        /// page to settle before extracting). Ignored when
        /// `--capture-window-ms` is also given.
        #[arg(long)]
        wait_for: Option<u64>,
        /// Skip OS-level sandbox hardening; Tier 2 still runs V8 with no host
        /// bindings.
        #[arg(long)]
        no_jail: bool,
        /// Use the strict default-deny seccomp allowlist (maximum hardening; may
        /// need per-host tuning).
        #[arg(long)]
        strict_sandbox: bool,
        /// Allow Tier 2 to replay a state-changing request (an unsafe HTTP
        /// method that is not a GraphQL/JSON-RPC read) picked by ranking. Off by
        /// default: such requests are withheld from replay for mutation-safety.
        #[arg(long)]
        allow_unsafe_replay: bool,
        /// Bypass robots.txt.
        #[arg(long)]
        ignore_robots: bool,
        /// Strip boilerplate (nav/header/footer/ads) to the main content
        /// (Firecrawl's `onlyMainContent`). On by default; pass this to get
        /// the full page instead.
        #[arg(long)]
        no_main_content: bool,
        /// Pretty-print the JSON envelope (no effect on raw Markdown output).
        #[arg(long)]
        pretty: bool,
    },
    /// Discover the JSON/XHR API endpoints a page's JavaScript calls (ranked)
    /// and replay the best one into `data`. Mirrors `POST /v1/discover`; always
    /// runs the Tier 2 isolate regardless of `--tier-max`.
    Discover {
        /// Target URL.
        url: String,
        /// http/https/socks5 proxy URL.
        #[arg(long)]
        proxy: Option<String>,
        /// Total request timeout (ms).
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
        /// Cap the escalation ladder (0, 1, or 2). Discovery needs Tier 2, so
        /// this is only meaningful as a value ≥ 2.
        #[arg(long, default_value_t = 2)]
        tier_max: u8,
        /// Tier 2 capture-window duration (ms).
        #[arg(long, default_value_t = 2_000)]
        capture_window_ms: u64,
        /// Skip OS-level sandbox hardening; Tier 2 still runs V8 with no host
        /// bindings.
        #[arg(long)]
        no_jail: bool,
        /// Use the strict default-deny seccomp allowlist (maximum hardening; may
        /// need per-host tuning).
        #[arg(long)]
        strict_sandbox: bool,
        /// Allow Tier 2 to replay a state-changing request (an unsafe HTTP
        /// method that is not a GraphQL/JSON-RPC read) picked by ranking. Off by
        /// default: such requests are withheld from replay for mutation-safety.
        #[arg(long)]
        allow_unsafe_replay: bool,
        /// Bypass robots.txt.
        #[arg(long)]
        ignore_robots: bool,
        /// Pretty-print the JSON envelope.
        #[arg(long)]
        pretty: bool,
    },
    /// Fast, shallow discovery of a site's URLs (sitemap + on-page links).
    /// Mirrors `POST /v1/map`.
    #[cfg(feature = "serve")]
    Map {
        /// Target URL (the site to map).
        url: String,
        /// Case-insensitive substring filter on the returned URL list.
        #[arg(long)]
        search: Option<String>,
        /// Max links to return.
        #[arg(long, default_value_t = 5_000)]
        limit: usize,
        /// Restrict results to the exact host (by default, subdomains of the
        /// target host are included too).
        #[arg(long)]
        exclude_subdomains: bool,
        /// Skip all sitemap sources (robots.txt-discovered and default) —
        /// on-page hrefs only.
        #[arg(long)]
        ignore_sitemap: bool,
        /// Return only sitemap-derived links; never fetch the page itself.
        /// Mutually exclusive with `--ignore-sitemap`.
        #[arg(long)]
        sitemap_only: bool,
        /// http/https/socks5 proxy URL.
        #[arg(long)]
        proxy: Option<String>,
        /// Total request timeout (ms), applied to every fetch the map performs.
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
        /// Pretty-print the JSON envelope.
        #[arg(long)]
        pretty: bool,
    },
    /// Run a persistent HTTP daemon exposing a Firecrawl-compatible REST API
    /// (`POST /v1/scrape`, `GET /health`). The process stays warm, so clients
    /// skip the per-scrape binary spawn.
    #[cfg(feature = "serve")]
    Serve {
        /// Bind address.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Bind port (Firecrawl's self-hosted API default is 3002).
        #[arg(long, default_value_t = 3002)]
        port: u16,
        /// Maximum concurrent extractions; excess requests queue.
        #[arg(long, default_value_t = 8)]
        max_concurrency: usize,
        /// Warm Tier 2 isolate workers to keep pooled (0 = auto, ≈ CPU count).
        /// Reused across requests so each scrape skips the jail-spawn + snapshot
        /// cost; also caps concurrent isolates.
        #[arg(long, default_value_t = 0)]
        isolate_pool_size: usize,
        /// Recycle a pooled worker after this many captures (leak hygiene).
        #[arg(long, default_value_t = 100)]
        isolate_max_jobs: u32,
        /// Default total request timeout (ms); per-request `timeout` overrides.
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
        /// Default escalation-ladder cap (0, 1, or 2); per-request `tierMax`
        /// overrides.
        #[arg(long, default_value_t = 2)]
        tier_max: u8,
        /// Default Tier 2 capture-window duration (ms); per-request
        /// `captureWindowMs` overrides.
        #[arg(long, default_value_t = 2_000)]
        capture_window_ms: u64,
        /// Default: skip OS-level sandbox hardening (Tier 2 still runs V8 with
        /// no host bindings); per-request `noJail` overrides.
        #[arg(long)]
        no_jail: bool,
        /// Use the strict default-deny seccomp allowlist for jailed children.
        #[arg(long)]
        strict_sandbox: bool,
        /// Default: bypass robots.txt; per-request `ignoreRobots` overrides.
        #[arg(long)]
        ignore_robots: bool,
        /// Default http/https/socks5 proxy URL; per-request `proxy` overrides.
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Run an MCP (Model Context Protocol) server over stdio, exposing Draco's
    /// scraping as MCP tools for agent clients (Claude, editors, …). The same
    /// server is available on the daemon at `POST /mcp`.
    #[cfg(feature = "serve")]
    Mcp {
        /// Default total request timeout (ms) for tool calls.
        #[arg(long, default_value_t = 30_000)]
        timeout: u64,
        /// Default escalation-ladder cap (0, 1, or 2).
        #[arg(long, default_value_t = 2)]
        tier_max: u8,
        /// Default Tier 2 capture-window duration (ms).
        #[arg(long, default_value_t = 2_000)]
        capture_window_ms: u64,
        /// Skip OS-level sandbox hardening (Tier 2 still runs V8 with no host
        /// bindings).
        #[arg(long)]
        no_jail: bool,
        /// Use the strict default-deny seccomp allowlist for jailed children.
        #[arg(long)]
        strict_sandbox: bool,
        /// Bypass robots.txt.
        #[arg(long)]
        ignore_robots: bool,
        /// http/https/socks5 proxy URL.
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Internal: jailed child entry (self-re-exec target). Hidden.
    #[command(name = "__jail", hide = true)]
    Jail,
}

/// Map a terminal [`Status`] to the process exit code, per spec §12.
///
/// `success` → 0, `error` → 1, `unsupported` → 2, `needs_browser` → 3.
fn status_to_exit_code(status: Status) -> i32 {
    match status {
        Status::Success => 0,
        Status::Error => 1,
        Status::Unsupported => 2,
        Status::NeedsBrowser => 3,
    }
}

/// Outcome of applying a JSONPath query to a `data` value.
#[derive(Debug, PartialEq)]
enum FilterOutcome {
    /// The query matched nothing; `.data` becomes `null` (spec §12).
    NoMatch,
    /// Exactly one node matched; it replaces `.data` verbatim.
    Single(Value),
    /// Multiple nodes matched; they are collected into a JSON array.
    Many(Value),
}

/// Apply a parsed JSONPath query to a `data` value, following spec §12 semantics:
///
/// - zero matches → [`FilterOutcome::NoMatch`],
/// - one match    → [`FilterOutcome::Single`] (the node itself),
/// - many matches → [`FilterOutcome::Many`] (a JSON array of nodes).
fn apply_jsonpath(data: &Value, path: &JsonPath) -> FilterOutcome {
    let nodes = path.query(data).all();
    match nodes.as_slice() {
        [] => FilterOutcome::NoMatch,
        [single] => FilterOutcome::Single((*single).clone()),
        many => FilterOutcome::Many(Value::Array(many.iter().map(|n| (*n).clone()).collect())),
    }
}

/// Apply the `--extract` filter to a completed [`ExtractionResult`], in place.
///
/// Only touches results that actually carry `data`. On a malformed query the
/// result is downgraded to [`Status::Error`] with a [`DracoError::Config`]
/// (exit 1). A zero-match query keeps `status: success`, sets `data: null`, and
/// records a `cli.extract_filter` note in the trace so the miss is observable.
fn filter_result(mut result: ExtractionResult, expr: &str) -> ExtractionResult {
    let path = match JsonPath::parse(expr) {
        Ok(p) => p,
        Err(e) => {
            result.status = Status::Error;
            result.data = None;
            result.source_tier = None;
            result.error = Some(DracoError::Config {
                detail: format!("invalid --extract JSONPath `{expr}`: {e}"),
            });
            return result;
        }
    };

    // Nothing to filter if the run produced no data (e.g. non-success status).
    let Some(data) = result.data.as_ref() else {
        return result;
    };

    match apply_jsonpath(data, &path) {
        FilterOutcome::NoMatch => {
            result.data = None;
            let tier = result.source_tier.unwrap_or(SourceTier::Static);
            result.trace.push(TraceStep {
                tier,
                action: "cli.extract_filter".to_string(),
                outcome: StepOutcome::Missed,
                elapsed_ms: 0,
                detail: Some(format!(
                    "--extract `{expr}` matched no nodes; data set to null"
                )),
            });
        }
        FilterOutcome::Single(v) => result.data = Some(v),
        FilterOutcome::Many(v) => result.data = Some(v),
    }
    result
}

/// Serialize the result to stdout-ready JSON (pretty or compact) and return it.
fn render(result: &ExtractionResult, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(result).expect("serialize result")
    } else {
        serde_json::to_string(result).expect("serialize result")
    }
}

/// Decide what to print to stdout, and return it **with a trailing newline**.
///
/// * A markdown-only [`FormatSet`] (the default) prints the raw `markdown`
///   string — clean and pipeable (`draco scrape url > page.md`) — unless
///   `--json` is set, in which case the full envelope is printed. If a
///   markdown-only run produced no markdown (e.g. a challenge →
///   `NeedsBrowser`, or a fetch error), we fall back to the envelope so the
///   failure is still legible on stdout.
/// * Any other combination of formats always prints the full
///   [`ExtractionResult`] envelope.
///
/// `pretty` only affects envelope output.
fn render_output(
    result: &ExtractionResult,
    formats: FormatSet,
    json: bool,
    pretty: bool,
) -> String {
    let print_envelope = json || formats != FormatSet::markdown_only();
    if !print_envelope {
        if let Some(md) = result.markdown.as_deref() {
            return format!("{md}\n");
        }
        // No markdown to print (non-success markdown run): show the envelope so
        // the status/error is visible rather than emitting an empty line.
    }
    format!("{}\n", render(result, pretty))
}

fn main() {
    // ---- Jailed-child re-exec hook (canonical §6/§7) -----------------------
    //
    // The supervisor re-execs this very binary as `draco __jail` to become the
    // jailed Tier 2 child, inheriting the IPC socket on fd 3. That child arms the
    // sandbox and then hosts the V8 capture — it must run **before** any tokio
    // runtime is created, because `draco-runtime::run_capture` (invoked deep
    // inside the child) builds its OWN current-thread tokio runtime and would
    // panic if nested inside another. So we detect the hook at the very top of
    // `main`, before `#[tokio::main]`'s runtime would have started, and hand off
    // to the child entry, which never returns.
    //
    // Only compiled with the `tier2` feature: the lean build has no jail/runtime
    // linked, so there is nothing for `__jail` to do. (Clap still knows the
    // hidden `__jail` subcommand in both builds; without tier2 it falls through
    // to the normal dispatch, which reports it is unavailable.)
    #[cfg(feature = "tier2")]
    {
        if std::env::args().nth(1).as_deref() == Some("__jail") {
            // Never returns: arms the sandbox, hosts the capture, exits.
            draco_core::run_jail_child();
        }
    }

    // ---- Normal path: build the async runtime and run the ladder ----------
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    runtime.block_on(async_main());
}

/// The async entry proper — everything that needs the tokio runtime. Split out of
/// `main` so the `__jail` re-exec check can run before any runtime exists.
async fn async_main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Scrape {
            url,
            format,
            json,
            extract: extract_expr,
            proxy,
            delay,
            timeout,
            tier_max,
            capture_window_ms,
            no_jail,
            strict_sandbox,
            allow_unsafe_replay,
            ignore_robots,
            pretty,
        } => {
            let formats = formats_from_args(&format);
            let config = Config {
                formats,
                proxy,
                delay_ms: delay,
                timeout_ms: timeout,
                respect_robots: !ignore_robots,
                tier_max,
                capture_window_ms,
                no_jail,
                strict_sandbox,
                allow_unsafe_replay,
            };
            let mut result = extract(&url, &config).await;
            if let Some(expr) = extract_expr.as_deref() {
                result = filter_result(result, expr);
            }
            print!("{}", render_output(&result, formats, json, pretty));
            std::process::exit(status_to_exit_code(result.status));
        }
        Command::Discover {
            url,
            proxy,
            timeout,
            tier_max,
            capture_window_ms,
            no_jail,
            strict_sandbox,
            allow_unsafe_replay,
            ignore_robots,
            pretty,
        } => {
            // Discovery + replay: the ranked endpoint catalog plus the winner
            // replayed into `data` (mirrors `discover::discover_handler`'s
            // `Config` construction). `endpoints` forces the Tier 2 capture;
            // `json` carries the replayed winner.
            let config = Config {
                formats: FormatSet {
                    json: true,
                    endpoints: true,
                    ..FormatSet::none()
                },
                proxy,
                delay_ms: 0,
                timeout_ms: timeout,
                respect_robots: !ignore_robots,
                tier_max,
                capture_window_ms,
                no_jail,
                strict_sandbox,
                allow_unsafe_replay,
            };
            let result = extract(&url, &config).await;
            println!("{}", render(&result, pretty));
            std::process::exit(status_to_exit_code(result.status));
        }
        #[cfg(feature = "serve")]
        Command::Map {
            url,
            search,
            limit,
            exclude_subdomains,
            ignore_sitemap,
            sitemap_only,
            proxy,
            timeout,
            pretty,
        } => {
            if ignore_sitemap && sitemap_only {
                eprintln!("draco map: --ignore-sitemap and --sitemap-only are mutually exclusive");
                std::process::exit(1);
            }
            let target = match serve::map::parse_http_url(&url) {
                Ok(u) => u,
                Err(msg) => {
                    eprintln!("draco map: {msg}");
                    std::process::exit(1);
                }
            };
            // Project the same CLI flags into a `Config` purely to reuse
            // `draco_core::session_opts`'s Config → SessionOpts mapping (the
            // same helper `map_handler` uses); the rest of `Config` is unused
            // by `map_site`.
            let config = Config {
                proxy,
                timeout_ms: timeout,
                respect_robots: true,
                ..Config::default()
            };
            let session = draco_core::session_opts(&config);
            let opts = serve::map::MapOptions {
                target,
                session,
                search,
                limit,
                include_subdomains: !exclude_subdomains,
                ignore_sitemap,
                sitemap_only,
            };
            match serve::map::map_site(&opts).await {
                Ok(outcome) => {
                    let body = serde_json::json!({ "success": true, "links": outcome.links });
                    let out = if pretty {
                        serde_json::to_string_pretty(&body).expect("serialize map result")
                    } else {
                        serde_json::to_string(&body).expect("serialize map result")
                    };
                    println!("{out}");
                }
                Err(serve::map::MapError::BadRequest(msg))
                | Err(serve::map::MapError::Upstream(msg)) => {
                    eprintln!("draco map: {msg}");
                    std::process::exit(1);
                }
            }
        }
        #[cfg(feature = "serve")]
        Command::Serve {
            host,
            port,
            max_concurrency,
            isolate_pool_size,
            isolate_max_jobs,
            timeout,
            tier_max,
            capture_window_ms,
            no_jail,
            strict_sandbox,
            ignore_robots,
            proxy,
        } => {
            let defaults = Config {
                // Per-request `formats` decides markdown/json/…; this default is
                // overwritten on every request but keeps the struct total.
                formats: FormatSet::markdown_only(),
                proxy,
                delay_ms: 0,
                timeout_ms: timeout,
                respect_robots: !ignore_robots,
                tier_max,
                capture_window_ms,
                no_jail,
                strict_sandbox,
                allow_unsafe_replay: false,
            };
            // Pool size 0 → auto: the available parallelism (CPU count), a sane
            // cap on concurrent isolates. Fall back to 4 if it can't be probed.
            let isolate_pool_size = if isolate_pool_size == 0 {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            } else {
                isolate_pool_size
            };
            let opts = serve::ServeOptions {
                host,
                port,
                max_concurrency,
                isolate_pool_size,
                isolate_max_jobs,
                defaults,
            };
            if let Err(e) = serve::serve(opts).await {
                eprintln!("draco serve: {e}");
                std::process::exit(1);
            }
        }
        #[cfg(feature = "serve")]
        Command::Mcp {
            timeout,
            tier_max,
            capture_window_ms,
            no_jail,
            strict_sandbox,
            ignore_robots,
            proxy,
        } => {
            let defaults = Config {
                formats: FormatSet::markdown_only(),
                proxy,
                delay_ms: 0,
                timeout_ms: timeout,
                respect_robots: !ignore_robots,
                tier_max,
                capture_window_ms,
                no_jail,
                strict_sandbox,
                allow_unsafe_replay: false,
            };
            if let Err(e) = mcp::run_stdio(defaults).await {
                eprintln!("draco mcp: {e}");
                std::process::exit(1);
            }
        }
        Command::Jail => {
            // Reached only when `tier2` is OFF (the tier2 build handles `__jail`
            // in `main` before the runtime starts and never gets here), or if
            // `__jail` is somehow dispatched through clap in a lean build.
            eprintln!(
                "draco __jail: unavailable — this binary was built without the `tier2` feature, \
                 so there is no jailed Tier 2 runtime to enter."
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use draco_types::Timing;
    use serde_json::json;

    // ---- status → exit-code mapping (spec §12) ----

    #[test]
    fn exit_codes_match_spec() {
        assert_eq!(status_to_exit_code(Status::Success), 0);
        assert_eq!(status_to_exit_code(Status::Error), 1);
        assert_eq!(status_to_exit_code(Status::Unsupported), 2);
        assert_eq!(status_to_exit_code(Status::NeedsBrowser), 3);
    }

    // ---- JSONPath filtering as a pure function ----

    fn sample() -> Value {
        json!({
            "products": [
                { "id": 1, "name": "Widget", "price": 42 },
                { "id": 2, "name": "Gadget", "price": 99 }
            ],
            "meta": { "count": 2 }
        })
    }

    fn parse(expr: &str) -> JsonPath {
        JsonPath::parse(expr).expect("valid test JSONPath")
    }

    #[test]
    fn filter_single_match_returns_node_verbatim() {
        let data = sample();
        let out = apply_jsonpath(&data, &parse("$.meta.count"));
        assert_eq!(out, FilterOutcome::Single(json!(2)));
    }

    #[test]
    fn filter_single_object_match() {
        let data = sample();
        let out = apply_jsonpath(&data, &parse("$.products[0]"));
        assert_eq!(
            out,
            FilterOutcome::Single(json!({ "id": 1, "name": "Widget", "price": 42 }))
        );
    }

    #[test]
    fn filter_no_match_is_reported() {
        let data = sample();
        let out = apply_jsonpath(&data, &parse("$.nonexistent"));
        assert_eq!(out, FilterOutcome::NoMatch);
    }

    #[test]
    fn filter_multi_match_collects_into_array() {
        let data = sample();
        let out = apply_jsonpath(&data, &parse("$.products[*].name"));
        assert_eq!(out, FilterOutcome::Many(json!(["Widget", "Gadget"])));
    }

    #[test]
    fn filter_wildcard_over_array_of_objects() {
        let data = sample();
        let out = apply_jsonpath(&data, &parse("$.products[*]"));
        assert_eq!(
            out,
            FilterOutcome::Many(json!([
                { "id": 1, "name": "Widget", "price": 42 },
                { "id": 2, "name": "Gadget", "price": 99 }
            ]))
        );
    }

    // ---- filter_result: end-to-end mutation of an ExtractionResult ----

    fn success_result(data: Value) -> ExtractionResult {
        ExtractionResult {
            url: "https://example.com".into(),
            status: Status::Success,
            source_tier: Some(SourceTier::Static),
            data: Some(data),
            markdown: None,
            metadata: None,
            html: None,
            raw_html: None,
            links: None,
            endpoints: None,
            timing: Timing::default(),
            trace: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn filter_result_single_replaces_data_and_keeps_success() {
        let r = filter_result(success_result(sample()), "$.meta.count");
        assert_eq!(r.status, Status::Success);
        assert_eq!(r.data, Some(json!(2)));
        assert!(r.trace.is_empty());
    }

    #[test]
    fn filter_result_multi_produces_array() {
        let r = filter_result(success_result(sample()), "$.products[*].price");
        assert_eq!(r.status, Status::Success);
        assert_eq!(r.data, Some(json!([42, 99])));
    }

    #[test]
    fn filter_result_no_match_nulls_data_keeps_success_and_notes() {
        let r = filter_result(success_result(sample()), "$.missing");
        assert_eq!(r.status, Status::Success, "no-match must stay success");
        assert_eq!(r.data, None, "no-match must null the data");
        assert_eq!(r.trace.len(), 1, "the miss must be noted in the trace");
        assert_eq!(r.trace[0].action, "cli.extract_filter");
        assert_eq!(r.trace[0].outcome, StepOutcome::Missed);
    }

    #[test]
    fn filter_result_invalid_path_becomes_config_error() {
        let r = filter_result(success_result(sample()), "not a valid path");
        assert_eq!(r.status, Status::Error);
        assert_eq!(status_to_exit_code(r.status), 1);
        assert!(r.data.is_none());
        assert!(matches!(r.error, Some(DracoError::Config { .. })));
    }

    #[test]
    fn filter_result_without_data_is_untouched() {
        // A non-success run carries no data; the filter is a no-op on it.
        let mut base = success_result(sample());
        base.status = Status::Unsupported;
        base.data = None;
        base.source_tier = None;
        let r = filter_result(base, "$.anything");
        assert_eq!(r.status, Status::Unsupported);
        assert!(r.data.is_none());
        assert!(r.trace.is_empty());
    }

    // ---- rendering ----

    #[test]
    fn render_compact_has_no_newlines() {
        let s = render(&success_result(json!({ "a": 1 })), false);
        assert!(!s.contains('\n'));
    }

    #[test]
    fn render_pretty_is_multiline() {
        let s = render(&success_result(json!({ "a": 1 })), true);
        assert!(s.contains('\n'));
    }

    // ---- render_output: markdown vs envelope ----

    /// A Markdown-scrape result (markdown + metadata, no data).
    fn markdown_result() -> ExtractionResult {
        ExtractionResult {
            url: "https://example.com".into(),
            status: Status::Success,
            source_tier: Some(SourceTier::Static),
            data: None,
            markdown: Some("# Title\n\nBody text.".into()),
            metadata: Some(json!({ "title": "Title", "statusCode": 200 })),
            html: None,
            raw_html: None,
            links: None,
            endpoints: None,
            timing: Timing::default(),
            trace: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn markdown_format_prints_raw_markdown() {
        let out = render_output(&markdown_result(), FormatSet::markdown_only(), false, false);
        // Raw markdown string, newline-terminated, NOT JSON.
        assert_eq!(out, "# Title\n\nBody text.\n");
        assert!(!out.contains("\"status\""));
    }

    #[test]
    fn markdown_format_with_json_flag_prints_envelope() {
        let out = render_output(&markdown_result(), FormatSet::markdown_only(), true, false);
        let json: Value = serde_json::from_str(out.trim()).expect("envelope is JSON");
        assert_eq!(json["status"], "success");
        assert_eq!(json["markdown"], "# Title\n\nBody text.");
        assert_eq!(json["metadata"]["title"], "Title");
    }

    #[test]
    fn json_format_prints_envelope_not_markdown() {
        // Even if markdown were present, a json-only FormatSet prints the
        // envelope.
        let out = render_output(&markdown_result(), FormatSet::json_only(), false, false);
        let json: Value = serde_json::from_str(out.trim()).expect("envelope is JSON");
        assert_eq!(json["status"], "success");
    }

    #[test]
    fn both_format_prints_envelope() {
        let mut r = markdown_result();
        r.data = Some(json!({ "ok": true }));
        let both = FormatSet {
            markdown: true,
            json: true,
            ..FormatSet::none()
        };
        let out = render_output(&r, both, false, true);
        assert!(out.contains('\n'), "pretty envelope is multi-line");
        let json: Value = serde_json::from_str(out.trim()).expect("envelope is JSON");
        assert_eq!(json["data"]["ok"], true);
        assert_eq!(json["markdown"], "# Title\n\nBody text.");
    }

    #[test]
    fn markdown_format_falls_back_to_envelope_when_no_markdown() {
        // A challenged/errored markdown run has no markdown; show the envelope
        // so the failure is legible rather than printing an empty line.
        let mut r = markdown_result();
        r.status = Status::NeedsBrowser;
        r.markdown = None;
        r.metadata = None;
        r.source_tier = None;
        let out = render_output(&r, FormatSet::markdown_only(), false, false);
        let json: Value = serde_json::from_str(out.trim()).expect("envelope is JSON");
        assert_eq!(json["status"], "needs_browser");
    }

    // ---- FormatArg slice → FormatSet folding ----

    #[test]
    fn empty_format_args_default_to_markdown_only() {
        assert_eq!(formats_from_args(&[]), FormatSet::markdown_only());
    }

    #[test]
    fn single_format_args_map_straight_through() {
        assert_eq!(
            formats_from_args(&[FormatArg::Markdown]),
            FormatSet::markdown_only()
        );
        assert_eq!(
            formats_from_args(&[FormatArg::Json]),
            FormatSet::json_only()
        );
        assert_eq!(
            formats_from_args(&[FormatArg::Html]),
            FormatSet {
                html: true,
                ..FormatSet::none()
            }
        );
        assert_eq!(
            formats_from_args(&[FormatArg::RawHtml]),
            FormatSet {
                raw_html: true,
                ..FormatSet::none()
            }
        );
        assert_eq!(
            formats_from_args(&[FormatArg::Links]),
            FormatSet {
                links: true,
                ..FormatSet::none()
            }
        );
        assert_eq!(
            formats_from_args(&[FormatArg::Endpoints]),
            FormatSet {
                endpoints: true,
                ..FormatSet::none()
            }
        );
    }

    #[test]
    fn both_format_arg_sets_markdown_and_json() {
        assert_eq!(
            formats_from_args(&[FormatArg::Both]),
            FormatSet {
                markdown: true,
                json: true,
                ..FormatSet::none()
            }
        );
    }

    #[test]
    fn repeatable_format_args_union_into_one_set() {
        // `--format markdown --format links` composes both flags.
        let set = formats_from_args(&[FormatArg::Markdown, FormatArg::Links]);
        assert_eq!(
            set,
            FormatSet {
                markdown: true,
                links: true,
                ..FormatSet::none()
            }
        );
    }

    #[test]
    fn both_composes_with_other_formats() {
        // `--format both --format links` => markdown + json + links.
        let set = formats_from_args(&[FormatArg::Both, FormatArg::Links]);
        assert_eq!(
            set,
            FormatSet {
                markdown: true,
                json: true,
                links: true,
                ..FormatSet::none()
            }
        );
    }

    // ---- clap parsing: Scrape / Discover / Map ----

    #[test]
    fn scrape_parses_with_default_markdown_only_format() {
        let cli = Cli::try_parse_from(["draco", "scrape", "https://example.com"])
            .expect("scrape should parse");
        match cli.command {
            Command::Scrape { url, format, .. } => {
                assert_eq!(url, "https://example.com");
                assert!(format.is_empty(), "no --format given ⇒ empty Vec");
                assert_eq!(formats_from_args(&format), FormatSet::markdown_only());
            }
            _ => panic!("expected Command::Scrape"),
        }
    }

    #[test]
    fn scrape_parses_repeatable_format_flag() {
        let cli = Cli::try_parse_from([
            "draco",
            "scrape",
            "https://example.com",
            "--format",
            "markdown",
            "--format",
            "links",
        ])
        .expect("repeatable --format should parse");
        match cli.command {
            Command::Scrape { format, .. } => {
                assert_eq!(format, vec![FormatArg::Markdown, FormatArg::Links]);
                assert_eq!(
                    formats_from_args(&format),
                    FormatSet {
                        markdown: true,
                        links: true,
                        ..FormatSet::none()
                    }
                );
            }
            _ => panic!("expected Command::Scrape"),
        }
    }

    #[test]
    fn scrape_parses_raw_html_value_name_and_alias() {
        for value in ["raw-html", "rawhtml"] {
            let cli =
                Cli::try_parse_from(["draco", "scrape", "https://example.com", "--format", value])
                    .unwrap_or_else(|e| panic!("--format {value} should parse: {e}"));
            match cli.command {
                Command::Scrape { format, .. } => {
                    assert_eq!(format, vec![FormatArg::RawHtml], "value {value}");
                }
                _ => panic!("expected Command::Scrape"),
            }
        }
    }

    #[test]
    fn discover_command_parses() {
        let cli = Cli::try_parse_from(["draco", "discover", "https://example.com", "--pretty"])
            .expect("discover should parse");
        match cli.command {
            Command::Discover { url, pretty, .. } => {
                assert_eq!(url, "https://example.com");
                assert!(pretty);
            }
            _ => panic!("expected Command::Discover"),
        }
    }

    #[cfg(feature = "serve")]
    #[test]
    fn map_command_parses_with_defaults() {
        let cli =
            Cli::try_parse_from(["draco", "map", "https://example.com"]).expect("map should parse");
        match cli.command {
            Command::Map {
                url,
                limit,
                exclude_subdomains,
                ignore_sitemap,
                sitemap_only,
                ..
            } => {
                assert_eq!(url, "https://example.com");
                assert_eq!(limit, 5_000);
                assert!(!exclude_subdomains, "subdomains included by default");
                assert!(!ignore_sitemap);
                assert!(!sitemap_only);
            }
            _ => panic!("expected Command::Map"),
        }
    }

    #[cfg(feature = "serve")]
    #[test]
    fn map_command_parses_all_flags() {
        let cli = Cli::try_parse_from([
            "draco",
            "map",
            "https://example.com",
            "--search",
            "blog",
            "--limit",
            "10",
            "--exclude-subdomains",
            "--sitemap-only",
            "--pretty",
        ])
        .expect("map should parse with flags");
        match cli.command {
            Command::Map {
                search,
                limit,
                exclude_subdomains,
                sitemap_only,
                pretty,
                ..
            } => {
                assert_eq!(search.as_deref(), Some("blog"));
                assert_eq!(limit, 10);
                assert!(exclude_subdomains);
                assert!(sitemap_only);
                assert!(pretty);
            }
            _ => panic!("expected Command::Map"),
        }
    }

    #[test]
    fn extract_subcommand_no_longer_exists() {
        // Clean break: the old `extract` subcommand name was renamed to
        // `scrape` with no deprecated alias.
        assert!(Cli::try_parse_from(["draco", "extract", "https://example.com"]).is_err());
    }
}
