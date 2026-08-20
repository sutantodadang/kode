//! Token-friendly web access: `fetch_url` (page → plain text, capped and
//! pageable) and `web_search` (DuckDuckGo HTML results → compact list).
//! Both are read-only with respect to the workspace but do reach the
//! network; non-http(s) schemes and local/private hosts are refused so the
//! agent can't be steered into probing the user's LAN or cloud metadata.

use std::net::IpAddr;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Result, ToolError};
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

const TIMEOUT: Duration = Duration::from_secs(20);
/// Hard cap on bytes read from any response body.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_CHARS: usize = 6000;
const MAX_MAX_CHARS: usize = 40_000;
const DEFAULT_RESULTS: usize = 5;
const MAX_RESULTS: usize = 10;
const USER_AGENT: &str =
    "Mozilla/5.0 (compatible; kode/0.2; +https://github.com/sutantodadang/kode)";

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(USER_AGENT)
        // Redirects are followed manually in `get_text` so every hop is
        // re-validated (scheme, host string, resolved IPs).
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ToolError::Failed(format!("http client: {e}")))
}

/// Extracts the lowercase host from an http(s) URL, refusing other schemes
/// and hosts that name the local machine or private/link-local ranges by
/// string. `check_url_resolved` adds the DNS-level check.
pub(crate) fn check_url(url: &str) -> Result<String> {
    let lower = url.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .ok_or_else(|| ToolError::Failed("only http(s) URLs are allowed".to_string()))?;
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host_port.rsplit_once('@').map_or(host_port, |(_, h)| h);
    let host = host.trim_start_matches('[');
    let host = host.split([']', ':']).next().unwrap_or("");
    if host.is_empty() {
        return Err(ToolError::Failed("URL has no host".to_string()));
    }
    let blocked = host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host == "0.0.0.0"
        || host == "::1"
        || host == "::"
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("169.254.")
        || host.starts_with("fc")
        || host.starts_with("fd")
        || host.starts_with("fe80:")
        || (host.starts_with("172.")
            && host
                .split('.')
                .nth(1)
                .and_then(|o| o.parse::<u8>().ok())
                .is_some_and(|o| (16..=31).contains(&o)));
    if blocked {
        return Err(ToolError::Failed(format!(
            "refusing to fetch local/private host '{host}'"
        )));
    }
    Ok(host.to_string())
}

/// True for IPs a fetch must never reach: loopback, unspecified, RFC1918,
/// link-local/metadata (169.254/16), CGNAT (100.64/10), IPv6 ULA/link-local,
/// and IPv4-mapped IPv6 forms of the same.
pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                || o[0] == 0
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
                || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

/// String check plus DNS resolution: every address the host resolves to
/// must be public. Literal IPs are checked directly.
// ponytail: resolve-then-connect leaves a DNS-rebinding TOCTOU window; pin
// the resolved IP via a custom resolver if that ever matters.
pub(crate) async fn check_url_resolved(url: &str) -> Result<()> {
    let host = check_url(url)?;
    let addrs: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::net::lookup_host((host.as_str(), 80))
            .await
            .map_err(|e| ToolError::Failed(format!("cannot resolve host '{host}': {e}")))?
            .map(|sa| sa.ip())
            .collect(),
    };
    if addrs.is_empty() {
        return Err(ToolError::Failed(format!(
            "host '{host}' resolved to nothing"
        )));
    }
    if let Some(ip) = addrs.into_iter().find(|ip| is_private_ip(*ip)) {
        return Err(ToolError::Failed(format!(
            "refusing to fetch '{host}': resolves to private address {ip}"
        )));
    }
    Ok(())
}

const MAX_REDIRECTS: usize = 5;

/// Resolves `location` against `base` for the common forms: absolute,
/// scheme-relative (`//host/p`), root-relative (`/p`), and relative (`p`).
pub(crate) fn resolve_location(base: &str, location: &str) -> String {
    let loc = location.trim();
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let (scheme, rest) = base.split_at(scheme_end);
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    if let Some(stripped) = loc.strip_prefix("//") {
        return format!("{scheme}{stripped}");
    }
    if loc.starts_with('/') {
        return format!("{scheme}{host}{loc}");
    }
    let path = &rest[host_end..];
    let path = path.split(['?', '#']).next().unwrap_or("");
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    format!("{scheme}{host}{dir}{loc}")
}

async fn get_text(url: &str) -> Result<(String, String)> {
    let client = client()?;
    let mut url = url.trim().to_string();
    let mut resp;
    let mut hops = 0;
    loop {
        check_url_resolved(&url).await?;
        resp = client
            .get(&url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,text/plain,application/json;q=0.9,*/*;q=0.5",
            )
            .send()
            .await
            .map_err(|e| ToolError::Failed(format!("fetch failed: {e}")))?;
        if !resp.status().is_redirection() {
            break;
        }
        let Some(location) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        else {
            break;
        };
        hops += 1;
        if hops > MAX_REDIRECTS {
            return Err(ToolError::Failed(format!(
                "too many redirects (>{MAX_REDIRECTS})"
            )));
        }
        url = resolve_location(&url, location);
    }
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !status.is_success() {
        return Err(ToolError::Failed(format!("fetch failed: HTTP {status}")));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ToolError::Failed(format!("fetch body failed: {e}")))?;
    let slice: &[u8] = &bytes;
    let slice = &slice[..slice.len().min(MAX_BODY_BYTES)];
    Ok((String::from_utf8_lossy(slice).into_owned(), content_type))
}

// --- HTML → text -------------------------------------------------------------

/// Strips `<script>/<style>/<noscript>/<head>` blocks and all tags, decodes the
/// common entities, turns block-level tags into newlines and collapses
/// whitespace. Not a real parser — good enough to read docs/articles.
pub(crate) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    let mut skip_depth_tag: Option<&'static str> = None;

    while let Some(lt) = rest.find('<') {
        if skip_depth_tag.is_none() {
            out.push_str(&rest[..lt]);
        }
        rest = &rest[lt..];
        // Comments.
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |i| &after[i + 3..]);
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        let closing = tag.starts_with('/');

        if let Some(skip) = skip_depth_tag {
            if closing && name == skip {
                skip_depth_tag = None;
            }
            continue;
        }
        match name.as_str() {
            "script" | "style" | "noscript" | "head" | "svg" | "template" if !closing => {
                skip_depth_tag = Some(match name.as_str() {
                    "script" => "script",
                    "style" => "style",
                    "noscript" => "noscript",
                    "head" => "head",
                    "svg" => "svg",
                    _ => "template",
                });
            }
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre"
            | "blockquote" | "section" | "article" | "ul" | "ol" | "table" | "header"
            | "footer" | "nav" | "dd" | "dt" => out.push('\n'),
            "td" | "th" => out.push(' '),
            _ => {}
        }
    }
    if skip_depth_tag.is_none() {
        out.push_str(rest);
    }
    collapse_whitespace(&decode_entities(&out))
}

pub(crate) fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapses runs of spaces/tabs to one space and 3+ newlines to two; trims
/// each line.
pub(crate) fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0;
    for line in s.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_run += 1;
            if blank_run == 1 {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Returns chars `[offset, offset+max)` of `text` plus a one-line footer when
/// more remains, so the model knows to page.
pub(crate) fn window(text: &str, offset: usize, max_chars: usize) -> String {
    let total = text.chars().count();
    if offset >= total {
        return format!("[no content at offset {offset}; total {total} chars]");
    }
    let slice: String = text.chars().skip(offset).take(max_chars).collect();
    let end = offset + slice.chars().count();
    if end < total {
        format!(
            "{slice}\n\n[truncated: showing chars {offset}..{end} of {total}; call again with offset={end} for more]"
        )
    } else {
        slice
    }
}

// --- fetch_url ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    url: String,
    max_chars: Option<usize>,
    offset: Option<usize>,
    raw: Option<bool>,
}

pub struct FetchUrl;

#[async_trait::async_trait]
impl Tool for FetchUrl {
    fn name(&self) -> &str {
        "fetch_url"
    }

    fn description(&self) -> &str {
        "Fetch a public http(s) URL and return its readable text (HTML stripped to plain text; JSON/text passed through). \
         Output is capped (default 6000 chars) — use offset to page through long pages. Use web_search first to find URLs."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "absolute http(s) URL" },
                "max_chars": { "type": "integer", "description": "max characters to return (default 6000, max 40000)" },
                "offset": { "type": "integer", "description": "character offset to start from (for paging)" },
                "raw": { "type": "boolean", "description": "return the raw body without HTML-to-text conversion" }
            },
            "required": ["url"]
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::ReadOnly
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: FetchArgs = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: e.to_string(),
        })?;
        let max_chars = args
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_MAX_CHARS);
        let (body, content_type) = get_text(&args.url).await?;
        let text = if args.raw.unwrap_or(false) || !content_type.contains("html") {
            collapse_whitespace(&body)
        } else {
            html_to_text(&body)
        };
        tracing::debug!(url = %args.url, chars = text.chars().count(), "fetch_url executed");
        Ok(ToolOutput {
            content: window(&text, args.offset.unwrap_or(0), max_chars),
        })
    }
}

// --- web_search --------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    max_results: Option<usize>,
}

pub struct WebSearch;

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web (DuckDuckGo). Returns up to 10 results as `title — url` plus a one-line snippet. \
         Follow up with fetch_url on a result to read it."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "max_results": { "type": "integer", "description": "1-10, default 5" }
            },
            "required": ["query"]
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::ReadOnly
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: SearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: e.to_string(),
            })?;
        let query = args.query.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "query is empty".to_string(),
            });
        }
        let n = args
            .max_results
            .unwrap_or(DEFAULT_RESULTS)
            .clamp(1, MAX_RESULTS);
        // ponytail: DDG's HTML endpoint, no API key. Markup may change;
        // parse_ddg_results is the only thing to update if it does.
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            percent_encode(query)
        );
        let (body, _) = get_text(&url).await?;
        let results = parse_ddg_results(&body, n);
        if results.is_empty() {
            return Ok(ToolOutput {
                content: format!("no results for '{query}'"),
            });
        }
        let content = results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                if r.snippet.is_empty() {
                    format!("{}. {} — {}", i + 1, r.title, r.url)
                } else {
                    format!("{}. {} — {}\n   {}", i + 1, r.title, r.url, r.snippet)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        tracing::debug!(query, results = results.len(), "web_search executed");
        Ok(ToolOutput { content })
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Extracts results from DuckDuckGo's HTML endpoint: each result has an
/// `<a class="result__a" href="...">title</a>` and, usually, an
/// `<a class="result__snippet">…</a>` (or `<td class="result-snippet">`).
pub(crate) fn parse_ddg_results(html: &str, max: usize) -> Vec<SearchResult> {
    let mut out = Vec::new();
    let mut rest = html;
    while out.len() < max {
        let Some(i) = rest.find("result__a") else {
            break;
        };
        // Walk back to the enclosing `<a`.
        let tag_start = rest[..i].rfind("<a").unwrap_or(i);
        let after = &rest[tag_start..];
        let Some(gt) = after.find('>') else { break };
        let open_tag = &after[..gt];
        let href = attr(open_tag, "href").unwrap_or_default();
        let body = &after[gt + 1..];
        let Some(close) = body.find("</a>") else {
            break;
        };
        let title = collapse_whitespace(&html_to_text(&body[..close]));
        rest = &body[close + 4..];

        // Snippet: the next result__snippet / result-snippet before the next result__a.
        let next_result = rest.find("result__a").unwrap_or(rest.len());
        let region = &rest[..next_result];
        let snippet = ["result__snippet", "result-snippet"]
            .iter()
            .find_map(|marker| region.find(marker).map(|p| &region[p..]))
            .and_then(|s| {
                let gt = s.find('>')?;
                let body = &s[gt + 1..];
                let close = body.find("</a>").or_else(|| body.find("</td>"))?;
                Some(collapse_whitespace(&html_to_text(&body[..close])))
            })
            .unwrap_or_default();

        let url = clean_ddg_url(&decode_entities(&href));
        if title.is_empty() || url.is_empty() {
            continue;
        }
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=\"");
    let i = tag.find(&pat)? + pat.len();
    let end = tag[i..].find('"')?;
    Some(tag[i..i + end].to_string())
}

/// DDG wraps result links as `//duckduckgo.com/l/?uddg=<encoded>&rut=…`;
/// unwrap to the real destination.
pub(crate) fn clean_ddg_url(href: &str) -> String {
    if let Some(i) = href.find("uddg=") {
        let enc = &href[i + 5..];
        let enc = enc.split('&').next().unwrap_or(enc);
        return percent_decode(enc);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_url_allows_public_and_blocks_local() {
        assert!(check_url("https://example.com/a?b=c").is_ok());
        assert!(check_url("http://docs.rs").is_ok());
        for bad in [
            "ftp://example.com",
            "file:///etc/passwd",
            "http://localhost:8080/",
            "http://127.0.0.1/",
            "http://10.1.2.3/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/",
            "http://user@localhost/",
            "http://my.internal/",
        ] {
            assert!(check_url(bad).is_err(), "{bad} should be blocked");
        }
        assert!(check_url("http://172.15.0.1/").is_ok());
        assert!(check_url("http://172.32.0.1/").is_ok());
    }

    #[test]
    fn is_private_ip_covers_local_ranges() {
        for ip in [
            "127.0.0.1",
            "0.0.0.0",
            "10.0.0.1",
            "172.16.5.5",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "172.32.0.1"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[tokio::test]
    async fn check_url_resolved_blocks_literal_private_ips_and_localhost() {
        assert!(check_url_resolved("http://127.0.0.1/").await.is_err());
        assert!(
            check_url_resolved("http://[::ffff:127.0.0.1]/")
                .await
                .is_err()
        );
        assert!(check_url_resolved("http://localhost/").await.is_err());
        assert!(check_url_resolved("http://169.254.169.254/").await.is_err());
    }

    #[test]
    fn resolve_location_handles_relative_forms() {
        let base = "https://example.com/a/b?q=1";
        assert_eq!(
            resolve_location(base, "https://other.org/x"),
            "https://other.org/x"
        );
        assert_eq!(
            resolve_location(base, "//cdn.example.com/y"),
            "https://cdn.example.com/y"
        );
        assert_eq!(resolve_location(base, "/root"), "https://example.com/root");
        assert_eq!(resolve_location(base, "sib"), "https://example.com/a/sib");
        assert_eq!(
            resolve_location("https://example.com", "p"),
            "https://example.com/p"
        );
    }

    #[test]
    fn html_to_text_strips_scripts_and_tags() {
        let html = r#"<html><head><title>T</title><style>p{}</style></head>
<body><script>var x = "<p>";</script><h1>Hello</h1><p>World &amp; <b>friends</b>&nbsp;&#33;</p>
<!-- comment --><ul><li>one</li><li>two</li></ul></body></html>"#;
        let text = html_to_text(html);
        // Block boundaries keep one blank line; inline tags collapse away.
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, vec!["Hello", "World & friends !", "one", "two"]);
        assert!(!text.contains("var x"));
        assert!(!text.contains("p{}"));
        assert!(!text.contains("comment"));
    }

    #[test]
    fn decode_entities_handles_named_and_numeric() {
        assert_eq!(
            decode_entities("a &lt; b &#x41; &#66; &unknown; &"),
            "a < b A B &unknown; &"
        );
    }

    #[test]
    fn window_pages_and_reports_remaining() {
        let text = "abcdefghij";
        assert_eq!(window(text, 0, 20), "abcdefghij");
        let w = window(text, 0, 4);
        assert!(w.starts_with("abcd\n\n[truncated: showing chars 0..4 of 10"));
        assert!(w.contains("offset=4"));
        assert_eq!(window(text, 8, 4), "ij");
        assert!(window(text, 10, 4).starts_with("[no content at offset 10"));
    }

    #[test]
    fn parse_ddg_results_extracts_title_url_snippet() {
        let html = r#"
<div class="result"><h2 class="result__title">
<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&amp;rut=abc">Example <b>Docs</b></a></h2>
<a class="result__snippet" href="x">The <b>docs</b> for example.</a></div>
<div class="result"><a class="result__a" href="https://second.org/">Second</a></div>
"#;
        let r = parse_ddg_results(html, 10);
        assert_eq!(
            r,
            vec![
                SearchResult {
                    title: "Example Docs".into(),
                    url: "https://example.com/docs".into(),
                    snippet: "The docs for example.".into(),
                },
                SearchResult {
                    title: "Second".into(),
                    url: "https://second.org/".into(),
                    snippet: String::new(),
                },
            ]
        );
        assert_eq!(parse_ddg_results(html, 1).len(), 1);
        assert!(parse_ddg_results("<html></html>", 5).is_empty());
    }

    #[test]
    fn percent_roundtrip() {
        let q = "rust async trait & lifetimes?";
        assert_eq!(percent_encode(q), "rust+async+trait+%26+lifetimes%3F");
        assert_eq!(percent_decode(&percent_encode(q)), q);
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn tool_metadata_is_read_only() {
        assert_eq!(FetchUrl.required_permission(), RequiredPermission::ReadOnly);
        assert_eq!(
            WebSearch.required_permission(),
            RequiredPermission::ReadOnly
        );
        assert_eq!(FetchUrl.name(), "fetch_url");
        assert_eq!(WebSearch.name(), "web_search");
    }
}
