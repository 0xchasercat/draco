//! Challenge / bot-wall detection for static responses and Tier 2 telemetry.
//!
//! Low-confidence page copy is corroborated before it can short-circuit the
//! extraction ladder. Vendor network telemetry and a severe hydration collapse
//! are high-confidence signals on their own.

/// A recognized bot-wall / challenge vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeKind {
    Cloudflare,
    DataDome,
    Akamai,
    PerimeterX,
    Kasada,
    Imperva,
    Recaptcha,
    Hcaptcha,
}

impl ChallengeKind {
    /// Short, stable label used in trace detail strings.
    pub fn as_str(self) -> &'static str {
        match self {
            ChallengeKind::Cloudflare => "cloudflare",
            ChallengeKind::DataDome => "datadome",
            ChallengeKind::Akamai => "akamai",
            ChallengeKind::PerimeterX => "perimeterx",
            ChallengeKind::Kasada => "kasada",
            ChallengeKind::Imperva => "imperva",
            ChallengeKind::Recaptcha => "recaptcha",
            ChallengeKind::Hcaptcha => "hcaptcha",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Signature {
    kind: ChallengeKind,
    tokens: &'static [&'static str],
}

const PHRASES: &[Signature] = &[
    Signature {
        kind: ChallengeKind::PerimeterX,
        tokens: &[
            "pardon the interruption",
            "press and hold to confirm you are a human",
            "access to this page has been denied",
        ],
    },
    Signature {
        kind: ChallengeKind::Cloudflare,
        tokens: &[
            "attention required!",
            "checking your browser before accessing",
            "verifying you are human",
            "enable javascript and cookies to continue",
            "why have i been blocked?",
            "just a moment",
        ],
    },
    Signature {
        kind: ChallengeKind::Imperva,
        tokens: &["request unsuccessful. incapsula incident"],
    },
    Signature {
        kind: ChallengeKind::Recaptcha,
        tokens: &["please verify you are a human"],
    },
];

const VENDOR_MARKERS: &[Signature] = &[
    Signature {
        kind: ChallengeKind::DataDome,
        tokens: &[
            "datadome",
            "js.datadome.co",
            "geo.captcha-delivery.com",
            "captcha-delivery.com",
            "dd_cookie_test",
            "x-datadome",
        ],
    },
    Signature {
        kind: ChallengeKind::PerimeterX,
        tokens: &[
            "_px",
            "px-captcha",
            "_pxhd",
            "px-cloud",
            "px-cdn",
            "_pxappid",
        ],
    },
    Signature {
        kind: ChallengeKind::Cloudflare,
        tokens: &[
            "__cf_chl",
            "cf-chl",
            "cf_chl_opt",
            "/cdn-cgi/challenge-platform/",
            "cf_clearance",
            "challenges.cloudflare.com",
            "cf-browser-verification",
        ],
    },
    Signature {
        kind: ChallengeKind::Akamai,
        tokens: &[
            "_abck",
            "ak_bmsc",
            "/akam/",
            "bm-verify",
            "errors.edgesuite.net",
            "akamai bot manager",
        ],
    },
    Signature {
        kind: ChallengeKind::Kasada,
        tokens: &["kpsdk", "x-kpsdk-", "ips.js"],
    },
    Signature {
        kind: ChallengeKind::Imperva,
        tokens: &[
            "visid_incap",
            "incap_ses",
            "_incapsula_resource",
            "imperva",
            "incapsula",
        ],
    },
    Signature {
        kind: ChallengeKind::Recaptcha,
        tokens: &[
            "google.com/recaptcha",
            "google.com/recaptcha/api.js",
            "g-recaptcha",
        ],
    },
    Signature {
        kind: ChallengeKind::Hcaptcha,
        tokens: &["hcaptcha.com", "h-captcha"],
    },
];

/// HTTP statuses commonly paired with challenge interstitials.
fn is_blocking_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 429 | 503)
}

fn first_match<'a>(haystack: &str, table: &'a [Signature]) -> Option<&'a Signature> {
    table.iter().find(|signature| {
        signature
            .tokens
            .iter()
            .any(|token| haystack.contains(token))
    })
}

fn join_headers_lower(headers: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in headers {
        out.push_str(&name.to_ascii_lowercase());
        out.push(':');
        out.push_str(&value.to_ascii_lowercase());
        out.push('\n');
    }
    out
}

fn looks_like_guid_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn has_guid_script_path(text: &str) -> bool {
    text.split(['/', '?', '#', '\'', '"', '<', '>', ' '])
        .any(looks_like_guid_segment)
}

fn vendor_marker(text: &str) -> Option<ChallengeKind> {
    first_match(text, VENDOR_MARKERS)
        .map(|signature| signature.kind)
        .or_else(|| has_guid_script_path(text).then_some(ChallengeKind::Kasada))
}

/// Inspect a Tier 0 response for a challenge interstitial.
///
/// A phrase alone is insufficient. It must be corroborated by a blocking status,
/// a vendor marker, or a tiny response. A vendor marker without challenge copy is
/// accepted only on a blocking status. `cf-mitigated: challenge` remains a
/// definitive Cloudflare signal at any status.
pub fn detect_challenge(
    status: u16,
    headers: &[(String, String)],
    body: &str,
) -> Option<ChallengeKind> {
    let headers = join_headers_lower(headers);
    if headers.contains("cf-mitigated:challenge") {
        return Some(ChallengeKind::Cloudflare);
    }

    let body = body.to_ascii_lowercase();
    let phrase = first_match(&body, PHRASES);
    let marker = vendor_marker(&body).or_else(|| vendor_marker(&headers));
    let blocking = is_blocking_status(status);
    let tiny = body.len() < 2_048;

    if let Some(signature) = phrase {
        if blocking
            || tiny
            || (marker.is_some() && runtime_challenge_dominates(Some(body.as_str())))
        {
            return Some(marker.unwrap_or(signature.kind));
        }
    }
    if blocking {
        return marker;
    }
    None
}

/// Classify a Tier 2 subresource URL. These are high-confidence signals because
/// they are requests the running page actually made, not words in page copy.
pub(crate) fn detect_network_challenge(url: &str) -> Option<ChallengeKind> {
    let url = url.to_ascii_lowercase();
    if url.contains("px-cloud.net")
        || url.contains("perimeterx.net")
        || url.contains("captcha.px-cdn")
    {
        return Some(ChallengeKind::PerimeterX);
    }
    if url.contains("cdn.datadome.co")
        || url.contains("js.datadome.co")
        || url.contains("captcha-delivery.com")
    {
        return Some(ChallengeKind::DataDome);
    }
    if url.contains("challenges.cloudflare.com") || url.contains("/cdn-cgi/challenge-platform/") {
        return Some(ChallengeKind::Cloudflare);
    }
    if url.contains("hcaptcha.com") {
        return Some(ChallengeKind::Hcaptcha);
    }
    if url.contains("google.com/recaptcha") || url.contains("recaptcha.net/recaptcha") {
        return Some(ChallengeKind::Recaptcha);
    }
    if url.contains("imperva.com")
        || url.contains("incapsula.com")
        || url.contains("_incapsula_resource")
    {
        return Some(ChallengeKind::Imperva);
    }
    if url.contains("kpsdk") || url.contains("x-kpsdk-") || url.contains("ips.js") {
        return Some(ChallengeKind::Kasada);
    }
    if has_guid_script_path(&url) {
        return Some(ChallengeKind::Kasada);
    }
    None
}

/// Whether runtime-only challenge telemetry represents the page rather than an
/// embedded widget or background bot-management request. A missing DOM remains
/// conservative; when serialization succeeded, substantial visible content is
/// proof that the vendor request did not replace the page with a wall.
pub(crate) fn runtime_challenge_dominates(rendered_html: Option<&str>) -> bool {
    const MAX_WALL_CONTENT_CHARS: usize = 1_000;

    let Some(html) = rendered_html else {
        return true;
    };
    let rendered =
        draco_static::content::scrape(html, "about:blank", 200, "text/html; charset=utf-8", false);
    draco_static::content::is_thin_content(&rendered.markdown, MAX_WALL_CONTENT_CHARS)
}

/// Return a trace detail when hydration collapses below 20% of the shell's
/// visible, non-whitespace character count.
pub(crate) fn hydration_collapse_detail(
    shell_chars: usize,
    hydrated_chars: usize,
) -> Option<String> {
    (shell_chars > 0 && hydrated_chars.saturating_mul(5) < shell_chars).then(|| {
        format!("hydration-collapse: shell_chars={shell_chars}, hydrated_chars={hydrated_chars}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn target_perimeterx_fixture_is_detected_on_200() {
        let body = r#"<html><body><h1>Pardon the interruption</h1>
            <p>Press and hold to confirm you are a human</p>
            <div id="px-captcha"></div><script src="https://captcha.px-cdn.net/PXabc/captcha.js"></script>
            </body></html>"#;
        assert_eq!(
            detect_challenge(200, &[], body),
            Some(ChallengeKind::PerimeterX)
        );
    }

    #[test]
    fn target_captcha_widget_does_not_override_content_rich_page() {
        let body = format!(
            "<html><body><main><h1>Homepage</h1>{}</main>\
             <aside><p>Please verify you are a human</p><div id=\"px-captcha\"></div></aside>\
             <script src=\"https://captcha.px-cdn.net/PXabc/captcha.js\"></script>\
             </body></html>",
            "<p>Real products, promotions, categories, and store information.</p>".repeat(40)
        );

        assert_eq!(detect_challenge(200, &[], &body), None);
    }

    #[test]
    fn cloudflare_fixture_is_detected() {
        let body = r#"<title>Attention Required! | Cloudflare</title>
            <script src="/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1"></script>"#;
        assert_eq!(
            detect_challenge(403, &[], body),
            Some(ChallengeKind::Cloudflare)
        );
    }

    #[test]
    fn cloudflare_mitigated_header_is_status_independent() {
        assert_eq!(
            detect_challenge(
                200,
                &headers(&[("CF-Mitigated", "CHALLENGE")]),
                "<html>empty shell</html>",
            ),
            Some(ChallengeKind::Cloudflare)
        );
    }

    #[test]
    fn cloudflare_instrumentation_on_200_is_not_flagged() {
        let body = r#"<main>real content</main>
            <script src="/cdn-cgi/challenge-platform/scripts/jsd/main.js"></script>"#;
        assert_eq!(
            detect_challenge(
                200,
                &headers(&[("server", "cloudflare"), ("set-cookie", "__cf_bm=ok")]),
                body,
            ),
            None
        );
    }

    #[test]
    fn datadome_fixture_is_detected() {
        let body = r#"<script src="https://geo.captcha-delivery.com/captcha/"></script>"#;
        assert_eq!(
            detect_challenge(403, &[], body),
            Some(ChallengeKind::DataDome)
        );
    }

    #[test]
    fn akamai_fixture_is_detected() {
        let body = r#"<h1>Access Denied</h1><script>bm-verify</script>
            errors.edgesuite.net reference #18.abc"#;
        assert_eq!(
            detect_challenge(403, &[], body),
            Some(ChallengeKind::Akamai)
        );
    }

    #[test]
    fn kasada_fixture_is_detected() {
        let body = r#"<script src="/149e9513-01fa-4fb0-aad4-566afd725d1b/ips.js"></script>"#;
        assert_eq!(
            detect_challenge(429, &[], body),
            Some(ChallengeKind::Kasada)
        );
    }

    #[test]
    fn incapsula_fixture_is_detected() {
        let body = r#"<p>Request unsuccessful. Incapsula incident ID: 123</p>
            <script src="/_Incapsula_Resource?SWJIYLWA=1"></script>"#;
        assert_eq!(
            detect_challenge(403, &[], body),
            Some(ChallengeKind::Imperva)
        );
    }

    #[test]
    fn phrase_in_legitimate_article_is_not_flagged() {
        let body = format!(
            "<article><h1>CAPTCHA UX research</h1><p>{}</p></article>",
            "This article quotes the message ‘Please verify you are a human’. ".repeat(80)
        );
        assert_eq!(detect_challenge(200, &[], &body), None);
    }

    #[test]
    fn normal_vendor_instrumentation_on_200_is_not_flagged() {
        let body = r#"<main>real store content</main>
            <script src="https://js.datadome.co/tags.js"></script>"#;
        assert_eq!(
            detect_challenge(200, &headers(&[("set-cookie", "datadome=ok")]), body),
            None
        );
    }

    #[test]
    fn perimeterx_cookie_on_200_is_not_flagged() {
        assert_eq!(
            detect_challenge(
                200,
                &headers(&[("set-cookie", "_px3=token; path=/")]),
                "<main>real content</main>",
            ),
            None
        );
    }

    #[test]
    fn blocking_status_without_challenge_evidence_is_not_flagged() {
        assert_eq!(detect_challenge(403, &[], "<h1>Forbidden</h1>"), None);
    }

    #[test]
    fn network_vendor_hits_are_high_confidence() {
        assert_eq!(
            detect_network_challenge("https://collector.px-cloud.net/api/v2/collector"),
            Some(ChallengeKind::PerimeterX)
        );
        assert_eq!(
            detect_network_challenge(
                "https://example.com/149e9513-01fa-4fb0-aad4-566afd725d1b/script.js"
            ),
            Some(ChallengeKind::Kasada)
        );
        assert_eq!(
            detect_network_challenge("https://example.com/api/products"),
            None
        );
    }

    #[test]
    fn runtime_network_signal_requires_a_content_poor_render() {
        let rich = format!(
            "<html><body><main><h1>Store homepage</h1>{}</main>\
             <iframe src=\"https://captcha.example/challenge\"></iframe></body></html>",
            "<p>Real products, promotions, categories, and store information.</p>".repeat(40)
        );
        let wall = "<html><body><h1>Pardon the interruption</h1>\
                    <p>Press and hold to confirm you are a human.</p></body></html>";

        assert!(!runtime_challenge_dominates(Some(&rich)));
        assert!(runtime_challenge_dominates(Some(wall)));
        assert!(runtime_challenge_dominates(None));
    }

    #[test]
    fn hydration_collapse_is_exactly_below_twenty_percent() {
        assert_eq!(
            hydration_collapse_detail(5_867, 445).as_deref(),
            Some("hydration-collapse: shell_chars=5867, hydrated_chars=445")
        );
        assert!(hydration_collapse_detail(1_000, 200).is_none());
        assert!(hydration_collapse_detail(1_000, 800).is_none());
        assert!(hydration_collapse_detail(0, 0).is_none());
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(ChallengeKind::Cloudflare.as_str(), "cloudflare");
        assert_eq!(ChallengeKind::DataDome.as_str(), "datadome");
        assert_eq!(ChallengeKind::Akamai.as_str(), "akamai");
        assert_eq!(ChallengeKind::PerimeterX.as_str(), "perimeterx");
        assert_eq!(ChallengeKind::Kasada.as_str(), "kasada");
        assert_eq!(ChallengeKind::Imperva.as_str(), "imperva");
        assert_eq!(ChallengeKind::Recaptcha.as_str(), "recaptcha");
        assert_eq!(ChallengeKind::Hcaptcha.as_str(), "hcaptcha");
    }
}
