//! # draco (CLI)
//!
//! Command-line interface + output contract. Draco is first a **URL → Markdown
//! scraper** (Firecrawl-style): `draco extract <url>` fetches a page and prints
//! clean Markdown of its main content to stdout. `--format json` switches to the
//! tiered JSON-API extraction (embedded state → build-id replay → runtime
//! interception), and `--format both` returns Markdown, metadata, and JSON in
//! one envelope.
//!
//! Output rules: for the default `markdown` format the raw Markdown string is
//! printed (pipeable: `draco extract url > page.md`); `--json` instead prints the
//! full [`ExtractionResult`] envelope. For `json`/`both` the envelope is always
//! printed. The `--extract <JSONPATH>` filter runs over `result.data`, and the
//! status→exit-code mapping (spec §12) is preserved in all modes.

use clap::{Parser, Subcommand, ValueEnum};
use draco_core::{extract, Config, OutputFormat};
use draco_types::{DracoError, ExtractionResult, SourceTier, Status, StepOutcome, TraceStep};
use serde_json::Value;
use serde_json_path::JsonPath;

#[derive(Parser)]
#[command(
    name = "draco",
    version,
    about = "URL → Markdown scraper (with an optional tiered JSON-API extraction mode)",
    long_about = "Draco — a browserless URL → Markdown scraper.\n\n\
        By default `draco extract <url>` fetches a page and prints clean Markdown of \
        its main content (headings, links, lists, code, tables), dropping nav/header/\
        footer boilerplate — the fast, static-only path. Use `--format json` for the \
        tiered JSON-API extraction (embedded state → Next.js build-id replay → runtime \
        interception) or `--format both` to get Markdown, metadata, and JSON together."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// CLI surface for [`OutputFormat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
enum FormatArg {
    /// Clean Markdown + metadata of the page's main content (default).
    Markdown,
    /// Tiered JSON-API extraction, populating `data`.
    Json,
    /// Both Markdown + metadata AND the JSON-API extraction.
    Both,
}

impl From<FormatArg> for OutputFormat {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::Markdown => OutputFormat::Markdown,
            FormatArg::Json => OutputFormat::Json,
            FormatArg::Both => OutputFormat::Both,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Scrape a URL to Markdown (default), or extract JSON with `--format json`.
    Extract {
        /// Target URL.
        url: String,
        /// Output format: `markdown` (default), `json`, or `both`.
        #[arg(long, value_enum, default_value_t = FormatArg::Markdown)]
        format: FormatArg,
        /// For `--format markdown`: print the full ExtractionResult JSON envelope
        /// instead of the raw Markdown string. (No effect for json/both, which
        /// always print the envelope.)
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
        /// Pretty-print the JSON envelope (no effect on raw Markdown output).
        #[arg(long)]
        pretty: bool,
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
/// * `markdown` format (the default) prints the raw `markdown` string — clean
///   and pipeable (`draco extract url > page.md`) — unless `--json` is set, in
///   which case the full envelope is printed. If a `markdown` run produced no
///   markdown (e.g. a challenge → `NeedsBrowser`, or a fetch error), we fall
///   back to the envelope so the failure is still legible on stdout.
/// * `json` / `both` always print the full [`ExtractionResult`] envelope.
///
/// `pretty` only affects envelope output.
fn render_output(result: &ExtractionResult, format: FormatArg, json: bool, pretty: bool) -> String {
    let print_envelope = json || !matches!(format, FormatArg::Markdown);
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
        Command::Extract {
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
            let config = Config {
                format: format.into(),
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
            print!("{}", render_output(&result, format, json, pretty));
            std::process::exit(status_to_exit_code(result.status));
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
            timing: Timing::default(),
            trace: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn markdown_format_prints_raw_markdown() {
        let out = render_output(&markdown_result(), FormatArg::Markdown, false, false);
        // Raw markdown string, newline-terminated, NOT JSON.
        assert_eq!(out, "# Title\n\nBody text.\n");
        assert!(!out.contains("\"status\""));
    }

    #[test]
    fn markdown_format_with_json_flag_prints_envelope() {
        let out = render_output(&markdown_result(), FormatArg::Markdown, true, false);
        let json: Value = serde_json::from_str(out.trim()).expect("envelope is JSON");
        assert_eq!(json["status"], "success");
        assert_eq!(json["markdown"], "# Title\n\nBody text.");
        assert_eq!(json["metadata"]["title"], "Title");
    }

    #[test]
    fn json_format_prints_envelope_not_markdown() {
        // Even if markdown were present, json format prints the envelope.
        let out = render_output(&markdown_result(), FormatArg::Json, false, false);
        let json: Value = serde_json::from_str(out.trim()).expect("envelope is JSON");
        assert_eq!(json["status"], "success");
    }

    #[test]
    fn both_format_prints_envelope() {
        let mut r = markdown_result();
        r.data = Some(json!({ "ok": true }));
        let out = render_output(&r, FormatArg::Both, false, true);
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
        let out = render_output(&r, FormatArg::Markdown, false, false);
        let json: Value = serde_json::from_str(out.trim()).expect("envelope is JSON");
        assert_eq!(json["status"], "needs_browser");
    }

    #[test]
    fn format_arg_maps_to_output_format() {
        assert_eq!(
            OutputFormat::from(FormatArg::Markdown),
            OutputFormat::Markdown
        );
        assert_eq!(OutputFormat::from(FormatArg::Json), OutputFormat::Json);
        assert_eq!(OutputFormat::from(FormatArg::Both), OutputFormat::Both);
    }
}
