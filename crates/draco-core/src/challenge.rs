//! Challenge / bot-wall detection (spec §3 short-circuit).
//!
//! Before spending Tier 2 compute (booting V8), `draco-core` checks whether the
//! Tier 0 response is a **genuine bot-wall interstitial** — a page that *replaces*
//! the real content and blocks access. On a match the ladder finalizes
//! [`Status::NeedsBrowser`](draco_types::Status::NeedsBrowser): Draco v0.1 does
//! not defeat JS challenges, so there is nothing to extract.
//!
//! ## The cardinal rule: a `200 OK` is never a challenge
//!
//! Cloudflare, DataDome, Akamai, and PerimeterX front *millions* of ordinary
//! sites. On a normal page they still set cookies (`__cf_bm`, `_abck`, `_px*`,
//! `datadome`), emit `server: cloudflare` / `cf-ray` headers, and (with Bot Fight
//! Mode / "JS Detections" on) inject a `/cdn-cgi/challenge-platform/…​/jsd/main.js`
//! beacon into a perfectly normal `200` document. None of that is a challenge —
//! it is the page you asked for. A marketing page can even mention "cloudflare"
//! or "datadome" in its copy.
//!
//! So detection is deliberately narrow:
//! 1. The one status-independent signal is the `cf-mitigated` response header,
//!    which Cloudflare emits *only* when it actually issues a challenge.
//! 2. Every other signal requires a **blocking status** (`403`/`429`/`503`) —
//!    the codes CDNs serve interstitials with — *and* a specific interstitial
//!    token (a challenge script `src`, a captcha-delivery host, a verification
//!    class), never a bare vendor name or a benign cookie.
//!
//! A `200` (or any success/redirect) always returns `None` and flows through the
//! normal extraction ladder. This is the difference between Draco succeeding
//! where `curl` succeeds and Draco uselessly giving up. Detection is a pure
//! function of `(status, headers, body)`, fully unit-testable offline.

/// A recognized bot-wall / challenge vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeKind {
    /// Cloudflare (managed challenge / "Just a moment…" interstitial, `cf-mitigated`).
    Cloudflare,
    /// DataDome.
    DataDome,
    /// Akamai Bot Manager.
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

/// Join headers into one lower-cased `name:value\n` blob for substring scanning.
/// Header order is irrelevant for detection (unlike for fingerprinting).
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

/// HTTP statuses a CDN serves a bot-wall interstitial with. A challenge replaces
/// the page, so it is never a `2xx`/`3xx`.
fn is_blocking_status(status: u16) -> bool {
    matches!(status, 403 | 429 | 503)
}

/// Inspect a Tier 0 response for a **genuine** challenge interstitial.
///
/// Returns the vendor on a match, or `None` for an ordinary document. See the
/// module docs: the only status-independent signal is the `cf-mitigated` header;
/// everything else requires a blocking status *and* a specific interstitial
/// token, so a normal page behind a CDN (including a `200` that carries CDN
/// cookies or a JS-detection beacon) is never misclassified.
pub fn detect_challenge(
    status: u16,
    headers: &[(String, String)],
    body: &str,
) -> Option<ChallengeKind> {
    let hdr = join_headers_lower(headers);

    // (1) Definitive, status-independent: Cloudflare sends `cf-mitigated`
    // (value `challenge`) ONLY when it issues a challenge.
    if hdr.contains("cf-mitigated") {
        return Some(ChallengeKind::Cloudflare);
    }

    // (2) Every other signal requires a blocking status. A 200 OK is the real
    // page — no matter which CDN fronts it or what beacons/cookies it carries.
    if !is_blocking_status(status) {
        return None;
    }

    let body_lc = body.to_ascii_lowercase();

    // ---- Cloudflare interstitial ---------------------------------------------
    // Specific tokens only: the challenge-platform script path, the classic
    // verification class/opt token, or the interstitial titles.
    if body_lc.contains("/cdn-cgi/challenge-platform/")
        || body_lc.contains("cf-browser-verification")
        || body_lc.contains("cf_chl_opt")
        || body_lc.contains("window._cf_chl_opt")
        || body_lc.contains("just a moment")
        || body_lc.contains("attention required! | cloudflare")
    {
        return Some(ChallengeKind::Cloudflare);
    }

    // ---- DataDome interstitial -----------------------------------------------
    // The captcha-delivery host / challenge script, or the `x-datadome` response
    // header on a blocking status.
    if body_lc.contains("geo.captcha-delivery.com")
        || body_lc.contains("js.datadome.co")
        || body_lc.contains("dd_cookie_test")
        || hdr.contains("x-datadome")
    {
        return Some(ChallengeKind::DataDome);
    }

    // ---- Akamai Bot Manager --------------------------------------------------
    // Akamai block/sensor markup on a blocking status.
    if body_lc.contains("bm-verify")
        || body_lc.contains("errors.edgesuite.net")
        || body_lc.contains("akamai bot manager")
    {
        return Some(ChallengeKind::Akamai);
    }

    // ---- PerimeterX / HUMAN --------------------------------------------------
    if body_lc.contains("px-captcha")
        || body_lc.contains("captcha.px-cloud")
        || body_lc.contains("/px/captcha")
        || body_lc.contains("_pxappid")
    {
        return Some(ChallengeKind::PerimeterX);
    }

    // A blocking status with no recognized interstitial markup is a plain app
    // 403/429/503, not a bot-wall — let the ladder handle it honestly.
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

    // ---- The regression that motivated the rewrite --------------------------

    #[test]
    fn cloudflare_fronted_200_is_not_a_challenge() {
        // chaser.sh: a normal 200 page behind Cloudflare's DNS with no anti-bot
        // enforcement — CDN headers, a `__cf_bm` cookie, a JS-detections beacon,
        // and marketing copy that literally mentions bot-walls. `curl` reads it
        // fine, so Draco must NOT report needs_browser.
        let body = r#"<!doctype html><html><head>
            <title>chaser.sh — the browser that doesn't get blocked</title>
            <meta name="description" content="beats cloudflare, datadome, perimeterx"></head>
            <body><h1>hi</h1>
            <script src="/cdn-cgi/challenge-platform/scripts/jsd/main.js"></script>
            <script defer src="https://static.cloudflareinsights.com/beacon.min.js"></script>
            </body></html>"#;
        let hdrs = hdr(&[
            ("server", "cloudflare"),
            ("cf-ray", "8abc123def-IAD"),
            ("content-type", "text/html"),
            ("set-cookie", "__cf_bm=token; path=/; HttpOnly"),
        ]);
        assert_eq!(detect_challenge(200, &hdrs, body), None);
    }

    #[test]
    fn vendor_names_in_body_copy_are_not_challenges_on_200() {
        let body =
            "<html><body>We bypass DataDome, PerimeterX, and Akamai bot manager.</body></html>";
        assert_eq!(detect_challenge(200, &[], body), None);
    }

    // ---- Genuine challenges (blocking status + interstitial) -----------------

    #[test]
    fn cloudflare_via_mitigated_header_is_status_independent() {
        assert_eq!(
            detect_challenge(403, &hdr(&[("cf-mitigated", "challenge")]), ""),
            Some(ChallengeKind::Cloudflare)
        );
    }

    #[test]
    fn cloudflare_just_a_moment_interstitial() {
        let body = r#"<!DOCTYPE html><html><head><title>Just a moment...</title>
            <script src="/cdn-cgi/challenge-platform/h/b/orchestrate/jsch/v1"></script></head>
            <body class="cf-browser-verification">Checking your browser…</body></html>"#;
        assert_eq!(
            detect_challenge(403, &hdr(&[("server", "cloudflare")]), body),
            Some(ChallengeKind::Cloudflare)
        );
    }

    #[test]
    fn cloudflare_503_under_attack() {
        let body = r#"<html><head><title>Just a moment...</title></head><body></body></html>"#;
        assert_eq!(
            detect_challenge(503, &hdr(&[("server", "cloudflare")]), body),
            Some(ChallengeKind::Cloudflare)
        );
    }

    #[test]
    fn datadome_block_page() {
        let body = r#"<html><body><script src="https://geo.captcha-delivery.com/captcha/"></script></body></html>"#;
        assert_eq!(
            detect_challenge(403, &[], body),
            Some(ChallengeKind::DataDome)
        );
    }

    #[test]
    fn datadome_200_with_tag_is_not_a_challenge() {
        // A normal page that merely loads DataDome's script still served the real
        // content (200) — not a block.
        let body = r#"<html><body><script src="https://js.datadome.co/tags.js"></script>real content</body></html>"#;
        assert_eq!(detect_challenge(200, &[], body), None);
    }

    #[test]
    fn akamai_block_page() {
        let body = r#"<html><body>Access Denied <script>bm-verify</script>
            errors.edgesuite.net reference #18.abc</body></html>"#;
        assert_eq!(
            detect_challenge(403, &[], body),
            Some(ChallengeKind::Akamai)
        );
    }

    #[test]
    fn perimeterx_block_page() {
        let body = r#"<html><head></head><body><div id="px-captcha"></div>
            <script>window._pxAppId = 'PXxxxx';</script></body></html>"#;
        assert_eq!(
            detect_challenge(403, &[], body),
            Some(ChallengeKind::PerimeterX)
        );
    }

    #[test]
    fn perimeterx_200_with_cookie_is_not_a_challenge() {
        // `_px3` cookie on a 200 = normal PX-protected page that let us through.
        assert_eq!(
            detect_challenge(
                200,
                &hdr(&[("set-cookie", "_px3=token; path=/")]),
                "<html>ok</html>"
            ),
            None
        );
    }

    // ---- Non-challenges ------------------------------------------------------

    #[test]
    fn cloudflare_server_header_with_200_is_not_a_challenge() {
        assert_eq!(
            detect_challenge(200, &hdr(&[("server", "cloudflare")]), "<html>hi</html>"),
            None
        );
    }

    #[test]
    fn plain_forbidden_is_not_a_challenge() {
        // A generic app 403 with no bot-wall markup must NOT be needs_browser.
        let body = "<html><body><h1>403 Forbidden</h1><p>Access denied.</p></body></html>";
        let hdrs = hdr(&[("server", "nginx"), ("content-type", "text/html")]);
        assert_eq!(detect_challenge(403, &hdrs, body), None);
    }

    #[test]
    fn challenge_platform_beacon_on_403_is_a_challenge_but_not_on_200() {
        // The exact same beacon markup: a challenge on a 403, benign on a 200.
        let body = r#"<script src="/cdn-cgi/challenge-platform/scripts/jsd/main.js"></script>"#;
        assert_eq!(
            detect_challenge(403, &hdr(&[("server", "cloudflare")]), body),
            Some(ChallengeKind::Cloudflare)
        );
        assert_eq!(
            detect_challenge(200, &hdr(&[("server", "cloudflare")]), body),
            None
        );
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
        assert_eq!(
            detect_challenge(403, &hdr(&[("CF-Mitigated", "CHALLENGE")]), ""),
            Some(ChallengeKind::Cloudflare)
        );
    }

    #[test]
    fn as_str_labels_are_stable() {
        assert_eq!(ChallengeKind::Cloudflare.as_str(), "cloudflare");
        assert_eq!(ChallengeKind::DataDome.as_str(), "datadome");
        assert_eq!(ChallengeKind::Akamai.as_str(), "akamai");
        assert_eq!(ChallengeKind::PerimeterX.as_str(), "perimeterx");
    }
}
