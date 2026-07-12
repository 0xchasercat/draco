//! # Selector-schema structured extraction
//!
//! Deterministic, LLM-free structured extraction: the caller supplies a JSON
//! **schema of CSS selectors**, and the extractor evaluates it against the
//! (fetched or rendered) HTML, returning a JSON object shaped like the schema —
//! "extract all product prices from this page as a JSON array" without a model
//! call.
//!
//! This is Draco's answer to Firecrawl's `extract`: Firecrawl runs an LLM
//! server-side; Draco's consumers *are* LLMs (agents over MCP/REST), so the
//! engine hands the agent a precise, reproducible extraction primitive instead
//! of nesting another model call. An agent inspects a page once (scrape or an
//! interact session), derives selectors, and from then on the extraction is
//! deterministic, instant, and free.
//!
//! ## Schema shape
//!
//! A JSON object mapping output field names to **specs**:
//!
//! ```json
//! {
//!   "title":  "h1",
//!   "prices": { "selector": ".price", "all": true },
//!   "link":   { "selector": "a.buy", "attr": "href" },
//!   "items":  { "selector": ".card", "all": true, "fields": {
//!                 "name":  ".name",
//!                 "price": ".price",
//!                 "url":   { "selector": "a", "attr": "href" }
//!             } }
//! }
//! ```
//!
//! - **string spec** — shorthand for `{ "selector": <s> }`: first match, text.
//! - **object spec** — `selector` (required) plus:
//!   - `all` (bool, default `false`): every match as an array instead of the
//!     first match.
//!   - `attr` (string, default `"text"`): what to read from each element —
//!     `"text"` (whitespace-collapsed text content), `"html"` (inner HTML), or
//!     any attribute name. URL-carrying attributes (`href`, `src`, `action`,
//!     `poster`) are absolutized against the page URL.
//!   - `fields` (object): a nested schema evaluated **relative to each match**,
//!     producing an object per element (`attr` is ignored when `fields` is
//!     present). Nesting is capped at [`MAX_DEPTH`].
//!
//! ## Semantics
//!
//! - `all: false`, no match → `null`. `all: true`, no match → `[]`.
//! - A missing attribute skips that element in `all` collections (arrays stay
//!   dense); the single-match form yields `null`.
//! - Invalid selectors / malformed specs never panic or abort the run: the
//!   field extracts to `null` and a human-readable warning is pushed for the
//!   surface to attach to the response.
//! - Match count per `all` field is capped at [`MAX_MATCHES`]; schema fields
//!   per object at [`MAX_FIELDS`] — bounded output for hostile/huge pages.

use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Map, Value};

/// Maximum nested-`fields` depth. Deeper specs extract to `null` + a warning.
pub const MAX_DEPTH: usize = 5;
/// Maximum elements collected per `all: true` field.
pub const MAX_MATCHES: usize = 1_000;
/// Maximum fields evaluated per schema object (root or nested).
pub const MAX_FIELDS: usize = 100;

/// Attributes whose values are absolutized against the page URL.
const URL_ATTRS: [&str; 4] = ["href", "src", "action", "poster"];

/// Evaluate a selector `schema` against `html`, returning the extracted JSON
/// object and any warnings (invalid selectors, malformed specs, depth/field
/// caps). Never panics; a schema that is not a JSON object yields an empty
/// object plus one warning.
///
/// `page_url` is used to absolutize URL-carrying attributes; pass the final
/// (post-redirect) document URL.
pub fn extract_with_schema(html: &str, page_url: &str, schema: &Value) -> (Value, Vec<String>) {
    let mut warnings = Vec::new();
    let Some(fields) = schema.as_object() else {
        warnings.push(
            "extract schema must be a JSON object mapping field names to selector specs"
                .to_string(),
        );
        return (json!({}), warnings);
    };
    let doc = Html::parse_document(html);
    let base = url::Url::parse(page_url).ok();
    let out = eval_fields(
        EvalScope::Document(&doc),
        fields,
        base.as_ref(),
        0,
        &mut warnings,
    );
    (Value::Object(out), warnings)
}

/// A selection scope: the whole document (root schema) or one matched element
/// (nested `fields`). One lifetime — the parsed tree's — so both arms yield
/// `ElementRef<'t>` and the field evaluator can recurse without duplication.
#[derive(Clone, Copy)]
enum EvalScope<'t> {
    Document(&'t Html),
    Element(ElementRef<'t>),
}

impl<'t> EvalScope<'t> {
    /// Collect up to `cap` matches for `sel` within this scope. Materializing
    /// the (bounded) matches sidesteps the two iterator types' generics.
    fn matches(&self, sel: &Selector, cap: usize) -> Vec<ElementRef<'t>> {
        match self {
            EvalScope::Document(doc) => doc.select(sel).take(cap).collect(),
            EvalScope::Element(el) => el.select(sel).take(cap).collect(),
        }
    }
}

/// Evaluate one schema object within `scope`. Each field resolves independently;
/// a bad field warns and yields `null` without affecting its siblings.
fn eval_fields(
    scope: EvalScope,
    fields: &Map<String, Value>,
    base: Option<&url::Url>,
    depth: usize,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    let mut out = Map::with_capacity(fields.len().min(MAX_FIELDS));
    for (i, (name, spec)) in fields.iter().enumerate() {
        if i >= MAX_FIELDS {
            warnings.push(format!(
                "schema object has more than {MAX_FIELDS} fields; the rest were skipped"
            ));
            break;
        }
        out.insert(
            name.clone(),
            eval_spec(scope, name, spec, base, depth, warnings),
        );
    }
    out
}

/// Evaluate a single field spec (string shorthand or object form) in `scope`.
fn eval_spec(
    scope: EvalScope,
    name: &str,
    spec: &Value,
    base: Option<&url::Url>,
    depth: usize,
    warnings: &mut Vec<String>,
) -> Value {
    // Normalize the two spec forms.
    let (selector_str, all, attr, sub_fields) = match spec {
        Value::String(s) => (s.as_str(), false, "text", None),
        Value::Object(o) => {
            let Some(sel) = o.get("selector").and_then(Value::as_str) else {
                warnings.push(format!(
                    "field {name:?}: object spec is missing \"selector\""
                ));
                return Value::Null;
            };
            (
                sel,
                o.get("all").and_then(Value::as_bool).unwrap_or(false),
                o.get("attr").and_then(Value::as_str).unwrap_or("text"),
                o.get("fields").and_then(Value::as_object),
            )
        }
        _ => {
            warnings.push(format!(
                "field {name:?}: spec must be a selector string or an object with \"selector\""
            ));
            return Value::Null;
        }
    };

    let Ok(selector) = Selector::parse(selector_str) else {
        warnings.push(format!(
            "field {name:?}: invalid CSS selector {selector_str:?}"
        ));
        return Value::Null;
    };

    if sub_fields.is_some() && depth >= MAX_DEPTH {
        warnings.push(format!(
            "field {name:?}: nested \"fields\" exceeds the depth cap of {MAX_DEPTH}"
        ));
        return Value::Null;
    }

    if all {
        let mut items = Vec::new();
        for el in scope.matches(&selector, MAX_MATCHES) {
            match sub_fields {
                Some(fields) => items.push(Value::Object(eval_fields(
                    EvalScope::Element(el),
                    fields,
                    base,
                    depth + 1,
                    warnings,
                ))),
                None => {
                    // Dense arrays: elements missing the attribute are skipped.
                    if let Some(v) = read_element(el, attr, base) {
                        items.push(v);
                    }
                }
            }
        }
        Value::Array(items)
    } else {
        match scope.matches(&selector, 1).into_iter().next() {
            None => Value::Null,
            Some(el) => match sub_fields {
                Some(fields) => Value::Object(eval_fields(
                    EvalScope::Element(el),
                    fields,
                    base,
                    depth + 1,
                    warnings,
                )),
                None => read_element(el, attr, base).unwrap_or(Value::Null),
            },
        }
    }
}

/// Read one value from an element: collapsed text, inner HTML, or an attribute
/// (URL attributes absolutized against `base`). `None` when the attribute is
/// absent.
fn read_element(el: ElementRef, attr: &str, base: Option<&url::Url>) -> Option<Value> {
    match attr {
        "text" => Some(Value::String(collapse_ws(&el.text().collect::<String>()))),
        "html" => Some(Value::String(el.inner_html())),
        other => {
            let raw = el.value().attr(other)?;
            let val = if URL_ATTRS.contains(&other) {
                absolutize(raw, base)
            } else {
                raw.to_string()
            };
            Some(Value::String(val))
        }
    }
}

/// Collapse whitespace runs to single spaces and trim — element text nodes are
/// full of layout newlines/indentation that mean nothing to a data consumer.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(c);
        }
    }
    out
}

/// Join a possibly-relative URL attribute against the page URL. Returns the
/// raw value untouched when there is no base or the join fails (e.g.
/// `javascript:` / `mailto:` pseudo-URLs are already "absolute").
fn absolutize(raw: &str, base: Option<&url::Url>) -> String {
    match base {
        Some(b) => match b.join(raw) {
            Ok(u) => u.to_string(),
            Err(_) => raw.to_string(),
        },
        None => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head><title>Shop</title></head><body>
        <h1>  Deals
            of the day </h1>
        <div class="card"><span class="name">Widget</span><span class="price">$9.99</span>
            <a href="/buy/widget">Buy</a></div>
        <div class="card"><span class="name">Gadget</span><span class="price">$19.99</span>
            <a href="/buy/gadget">Buy</a></div>
        <div class="card"><span class="name">Doodad</span><span class="price">$4.49</span></div>
        <img src="images/hero.png">
    </body></html>"#;

    const URL: &str = "https://shop.example/deals?page=1";

    #[test]
    fn string_shorthand_first_match_collapsed_text() {
        let (v, w) = extract_with_schema(PAGE, URL, &json!({ "title": "h1" }));
        assert_eq!(v["title"], "Deals of the day");
        assert!(w.is_empty());
    }

    #[test]
    fn all_collects_every_match() {
        let (v, w) = extract_with_schema(
            PAGE,
            URL,
            &json!({ "prices": { "selector": ".price", "all": true } }),
        );
        assert_eq!(v["prices"], json!(["$9.99", "$19.99", "$4.49"]));
        assert!(w.is_empty());
    }

    #[test]
    fn url_attr_is_absolutized() {
        let (v, _) = extract_with_schema(
            PAGE,
            URL,
            &json!({
                "first_link": { "selector": ".card a", "attr": "href" },
                "hero": { "selector": "img", "attr": "src" }
            }),
        );
        assert_eq!(v["first_link"], "https://shop.example/buy/widget");
        assert_eq!(v["hero"], "https://shop.example/images/hero.png");
    }

    #[test]
    fn nested_fields_build_objects_per_match() {
        let (v, w) = extract_with_schema(
            PAGE,
            URL,
            &json!({
                "items": { "selector": ".card", "all": true, "fields": {
                    "name": ".name",
                    "price": ".price",
                    "url": { "selector": "a", "attr": "href" }
                } }
            }),
        );
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["name"], "Widget");
        assert_eq!(items[1]["price"], "$19.99");
        assert_eq!(items[0]["url"], "https://shop.example/buy/widget");
        // Third card has no <a>: single-match sub-field yields null, not a skip.
        assert_eq!(items[2]["url"], Value::Null);
        assert!(w.is_empty());
    }

    #[test]
    fn no_match_null_or_empty_array() {
        let (v, _) = extract_with_schema(
            PAGE,
            URL,
            &json!({
                "missing": ".does-not-exist",
                "none": { "selector": ".does-not-exist", "all": true }
            }),
        );
        assert_eq!(v["missing"], Value::Null);
        assert_eq!(v["none"], json!([]));
    }

    #[test]
    fn dense_arrays_skip_elements_missing_the_attr() {
        // Three cards, only two have an <a href>.
        let (v, _) = extract_with_schema(
            PAGE,
            URL,
            &json!({ "links": { "selector": ".card a", "attr": "href", "all": true } }),
        );
        assert_eq!(v["links"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn invalid_selector_warns_and_yields_null() {
        let (v, w) = extract_with_schema(PAGE, URL, &json!({ "bad": ":::nope" }));
        assert_eq!(v["bad"], Value::Null);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("invalid CSS selector"));
    }

    #[test]
    fn malformed_specs_warn_without_aborting_siblings() {
        let (v, w) = extract_with_schema(
            PAGE,
            URL,
            &json!({
                "no_selector": { "all": true },
                "wrong_type": 42,
                "title": "h1"
            }),
        );
        assert_eq!(v["no_selector"], Value::Null);
        assert_eq!(v["wrong_type"], Value::Null);
        assert_eq!(v["title"], "Deals of the day");
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn non_object_schema_is_rejected_gracefully() {
        let (v, w) = extract_with_schema(PAGE, URL, &json!(["h1"]));
        assert_eq!(v, json!({}));
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn html_attr_returns_inner_html() {
        let (v, _) = extract_with_schema(
            PAGE,
            URL,
            &json!({ "card": { "selector": ".card", "attr": "html" } }),
        );
        let html = v["card"].as_str().unwrap();
        assert!(html.contains(r#"<span class="name">Widget</span>"#));
    }

    #[test]
    fn depth_cap_warns() {
        // Build a schema nested one past MAX_DEPTH.
        let mut spec = json!({ "selector": ".card", "fields": { "name": ".name" } });
        for _ in 0..MAX_DEPTH {
            spec = json!({ "selector": ".card", "fields": { "inner": spec } });
        }
        let (_, w) = extract_with_schema(PAGE, URL, &json!({ "deep": spec }));
        assert!(w.iter().any(|m| m.contains("depth cap")));
    }
}
