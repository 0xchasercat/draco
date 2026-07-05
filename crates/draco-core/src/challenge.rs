//! Challenge / bot-wall detection (spec §3 short-circuit).
//!
//! Before spending Tier 2 compute (booting V8), `draco-core` inspects the Tier 0
//! HTML **and** response headers for known bot-wall signatures. On a match the
//! ladder finalizes [`Status::NeedsBrowser`](draco_types::Status::NeedsBrowser)
//! immediately — hydrating a challenge page in the isolate is wasted work, and
//! Draco v0.1 deliberately does not defeat JS challenges.
//!
//! Detection is a pure function of `(status, headers, body)`, so it is fully
//! unit-testable against fixture strings with no network.

/// A recognized bot-wall / challenge vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeKind {
    /// Cloudflare (managed challenge / "Just a moment…" interstitial, `cf-mitigated`).
    Cloudflare,
    /// DataDome.
    DataDome,
    /// Akamai Bot Manager (`_abck` / `bm_sz` cookies, sensor markup).
    Akamai,
    /// PerimeterX / HUMAN.
    PerimeterX,
}

impl ChallengeKind {
    /// Short, stable label used in trace `detail` strings.
    pub fn as_str(self) -> &'static str {
        match self {
            ChallengeKind::Cloudflare => "cloudflare",
            ChallengeKind::DataDome => "datadome",
            ChallengeKind::Akamai => "akamai",
            ChallengeKind::PerimeterX => "perimeterx",
        }
    }
}

/// Header names/values worth inspecting are matched case-insensitively; we lower
/// a joined header blob once and substring-scan it. Header order is irrelevant
/// for detection (unlike for fingerprinting), so a flat blob is fine.
fn join_headers_lower(headers: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in headers {
        out.push_str(&k.to_ascii_lowercase());
        out.push(':');
        out.push_str(&v.to_ascii_lowercase());
        out.push('\n');
    }
    out
}

/// Inspect a Tier 0 response for a challenge signature.
///
/// `status` is the HTTP status code, `headers` the response headers (any order),
/// and `body` the raw HTML text. Returns the first matching [`ChallengeKind`],
/// or `None` if the page looks like an ordinary document.
///
/// The checks are intentionally conservative: they key off vendor-specific
/// header/cookie names and well-known interstitial markup, not generic 403s, so
/// a plain "forbidden" page still escalates through the normal ladder rather
/// than being mislabeled `needs_browser`.
pub fn detect_challenge(
    status: u16,
    headers: &[(String, String)],
    body: &str,
) -> Option<ChallengeKind> {
    let hdr = join_headers_lower(headers);
    let body_lc = body.to_ascii_lowercase();

    // ---- Cloudflare -----------------------------------------------------
    // Header tells: `cf-mitigated: challenge`, `server: cloudflare` paired with a
    // challenge status, and the `cf_chl_*` / `__cf_bm` cookie family.
    if hdr.contains("cf-mitigated")
        || hdr.contains("cf-chl-bypass")
        || hdr.contains("__cf_chl")
        || hdr.contains("cf_chl_")
    {
        return Some(ChallengeKind::Cloudflare);
    }
    let cf_server = hdr.contains("server:cloudflare");
    if (cf_server && (status == 403 || status == 429 || status == 503))
        || body_lc.contains("cf-browser-verification")
        || body_lc.contains("cf_chl_opt")
        || body_lc.contains("challenge-platform")
        || (body_lc.contains("just a moment") && body_lc.contains("cloudflare"))
    {
        return Some(ChallengeKind::Cloudflare);
    }

    // ---- DataDome -------------------------------------------------------
    // `datadome` cookie / `x-datadome` header, or the DataDome challenge JS tag.
    if hdr.contains("x-datadome")
        || hdr.contains("datadome=")
        || hdr.contains("set-cookie:datadome")
        || body_lc.contains("datadome")
    {
        return Some(ChallengeKind::DataDome);
    }

    // ---- Akamai Bot Manager --------------------------------------------
    // The `_abck` / `bm_sz` sensor cookies and the akam sensor script are the tells.
    if hdr.contains("_abck=")
        || hdr.contains("bm_sz=")
        || hdr.contains("akamai-bmp")
        || body_lc.contains("_abck")
        || body_lc.contains("bm-verify")
    {
        return Some(ChallengeKind::Akamai);
    }

    // ---- PerimeterX / HUMAN --------------------------------------------
    // `_px*` cookies, the `x-px` header family, or the px captcha bootstrap.
    if hdr.contains("x-px")
        || hdr.contains("_pxhd=")
        || hdr.contains("_px2=")
        || hdr.contains("_px3=")
        || body_lc.contains("px-captcha")
        || body_lc.contains("_pxappid")
        || body_lc.contains("perimeterx")
    {
        return Some(ChallengeKind::PerimeterX);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn cloudflare_via_header() {
        let got = detect_challenge(503, &hdr(&[("cf-mitigated", "challenge")]), "");
        assert_eq!(got, Some(ChallengeKind::Cloudflare));
    }

    #[test]
    fn cloudflare_via_interstitial_body() {
        let body = r#"<!DOCTYPE html><html><head><title>Just a moment...</title>
            <script src="/cdn-cgi/challenge-platform/h/b/orchestrate/jsch/v1"></script></head>
            <body class="cf-browser-verification">Checking your browser…</body></html>"#;
        let got = detect_challenge(403, &hdr(&[("server", "cloudflare")]), body);
        assert_eq!(got, Some(ChallengeKind::Cloudflare));
    }

    #[test]
    fn cloudflare_server_header_needs_challenge_status() {
        // `server: cloudflare` with a 200 and ordinary body is NOT a challenge —
        // Cloudflare fronts plenty of normal sites.
        let got = detect_challenge(200, &hdr(&[("server", "cloudflare")]), "<html>hi</html>");
        assert_eq!(got, None);
    }

    #[test]
    fn datadome_via_cookie_and_body() {
        let hdrs = hdr(&[("set-cookie", "datadome=abc; Path=/")]);
        assert_eq!(
            detect_challenge(403, &hdrs, ""),
            Some(ChallengeKind::DataDome)
        );
        let body =
            r#"<html><body><script src="https://js.datadome.co/tags.js"></script></body></html>"#;
        assert_eq!(
            detect_challenge(200, &[], body),
            Some(ChallengeKind::DataDome)
        );
    }

    #[test]
    fn akamai_via_sensor_cookie() {
        let hdrs = hdr(&[("set-cookie", "_abck=0~-1~-1; path=/")]);
        assert_eq!(
            detect_challenge(429, &hdrs, ""),
            Some(ChallengeKind::Akamai)
        );
    }

    #[test]
    fn perimeterx_via_body_and_header() {
        let body = r#"<html><head></head><body><div id="px-captcha"></div>
            <script>window._pxAppId = 'PXxxxx';</script></body></html>"#;
        assert_eq!(
            detect_challenge(403, &[], body),
            Some(ChallengeKind::PerimeterX)
        );
        let hdrs = hdr(&[("set-cookie", "_px3=token; path=/")]);
        assert_eq!(
            detect_challenge(200, &hdrs, ""),
            Some(ChallengeKind::PerimeterX)
        );
    }

    #[test]
    fn plain_forbidden_is_not_a_challenge() {
        // A generic 403 with no vendor tell must escalate normally, not be
        // mislabeled needs_browser.
        let body = "<html><body><h1>403 Forbidden</h1><p>Access denied.</p></body></html>";
        let hdrs = hdr(&[("server", "nginx"), ("content-type", "text/html")]);
        assert_eq!(detect_challenge(403, &hdrs, body), None);
    }

    #[test]
    fn ordinary_page_is_not_a_challenge() {
        let body = r#"<html><head><title>Widget — Shop</title></head>
            <body><script id="__NEXT_DATA__">{"props":{}}</script></body></html>"#;
        let hdrs = hdr(&[("content-type", "text/html"), ("server", "vercel")]);
        assert_eq!(detect_challenge(200, &hdrs, body), None);
    }

    #[test]
    fn header_matching_is_case_insensitive() {
        let got = detect_challenge(503, &hdr(&[("CF-Mitigated", "CHALLENGE")]), "");
        assert_eq!(got, Some(ChallengeKind::Cloudflare));
    }

    #[test]
    fn as_str_labels_are_stable() {
        assert_eq!(ChallengeKind::Cloudflare.as_str(), "cloudflare");
        assert_eq!(ChallengeKind::DataDome.as_str(), "datadome");
        assert_eq!(ChallengeKind::Akamai.as_str(), "akamai");
        assert_eq!(ChallengeKind::PerimeterX.as_str(), "perimeterx");
    }
}
