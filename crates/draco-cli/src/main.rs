//! # draco (CLI — WS-D)
//!
//! Command-line interface + output contract. Implements canonical spec §12:
//! the `--extract <JSONPATH>` filter over `result.data` and the
//! status→exit-code mapping. All flags are wired into [`draco_core::Config`];
//! the well-formed [`ExtractionResult`] is always emitted as JSON on stdout.

use clap::{Parser, Subcommand};
use draco_core::{extract, Config};
use draco_types::{DracoError, ExtractionResult, SourceTier, Status, StepOutcome, TraceStep};
use serde_json::Value;
use serde_json_path::JsonPath;

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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Extract {
            url,
            extract: extract_expr,
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
            let mut result = extract(&url, &config).await;
            if let Some(expr) = extract_expr.as_deref() {
                result = filter_result(result, expr);
            }
            println!("{}", render(&result, pretty));
            std::process::exit(status_to_exit_code(result.status));
        }
        Command::Jail => {
            // TODO(Slice 2): draco_jail::run_jail_child();
            eprintln!("draco __jail: not implemented (Slice 2 spike)");
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
}
