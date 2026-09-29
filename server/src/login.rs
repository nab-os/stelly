//! Signing in to Qobuz, and keeping the app credentials current.
//!
//! Qobuz issues no API credentials to individuals, so the web player's own
//! app id and signing secret are what every third-party client uses. They
//! rotate with player releases, which is why they are scraped rather than
//! pinned. The secrets are obfuscated: each is a `seed` attached to a timezone
//! plus an `info`/`extras` pair on the matching timezone entry, concatenated
//! and base64-decoded.
//!
//! The user token needs a browser. Since April 2026 `user/login` sits behind a
//! reCAPTCHA-protected OAuth flow, so: listen on a local port, send the
//! browser to Qobuz's sign-in page with that port as the redirect, and trade
//! the code that comes back for a token. Copying `localuser.token` out of the
//! web player's local storage by hand still works where a browser cannot reach
//! this machine.

use crate::qobuz::API_BASE;
use anyhow::{bail, Context, Result};
use regex::Regex;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LOGIN_PAGE: &str = "https://play.qobuz.com/login";
const PLAYER_BASE: &str = "https://play.qobuz.com";
const SIGNIN_URL: &str = "https://www.qobuz.com/signin/oauth";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// What the web player bundle gives away.
pub struct Bundle {
    pub app_id: String,
    pub secrets: Vec<String>,
    /// Trades an OAuth code for a token. Absent from some player releases.
    pub private_key: Option<String>,
}

/// Fetch the web player's bundle (~9MB) and pull the credentials out of it.
pub async fn fetch_bundle() -> Result<Bundle> {
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(120))
        .build()?;
    let page = http.get(LOGIN_PAGE).send().await?.error_for_status()?.text().await?;
    let path = Regex::new(r#"<script src="(/resources/[^"]+/bundle\.js)""#)?
        .captures(&page)
        .map(|c| c[1].to_string())
        .context("could not find the bundle.js URL on the login page")?;
    let source = http
        .get(format!("{PLAYER_BASE}{path}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    parse_bundle(&source)
}

pub fn parse_bundle(source: &str) -> Result<Bundle> {
    let app = Regex::new(r#"production:\{api:\{appId:"(\d+)",appSecret:"(\w*)""#)?
        .captures(source)
        .context("could not find appId in the bundle")?;
    let app_id = app[1].to_string();

    // The literal appSecret is sometimes usable, so keep it as a candidate.
    let mut secrets = Vec::new();
    if !app[2].is_empty() {
        secrets.push(app[2].to_string());
    }

    let timezones: std::collections::HashMap<String, (String, String)> =
        Regex::new(r#"name:"([\w/_+-]+)",info:"([\w=]+)",extras:"([\w=]+)""#)?
            .captures_iter(source)
            .map(|c| {
                let name = c[1].rsplit('/').next().unwrap_or("").to_lowercase();
                (name, (c[2].to_string(), c[3].to_string()))
            })
            .collect();

    let hex32 = Regex::new(r"^[0-9a-f]{32}$")?;
    let seeds = Regex::new(r#"\.initialSeed\("([\w=]+)",window\.utimezone\.([a-z]+)\)"#)?;
    for seed in seeds.captures_iter(source) {
        let Some((info, extras)) = timezones.get(&seed[2]) else {
            continue;
        };
        let blob = format!("{}{info}{extras}", &seed[1]);
        if let Some(secret) = base64_decode(&blob).and_then(|b| String::from_utf8(b).ok()) {
            if hex32.is_match(&secret) && !secrets.contains(&secret) {
                secrets.push(secret);
            }
        }
    }
    if secrets.is_empty() {
        bail!("found no usable secrets in the bundle");
    }

    let private_key = Regex::new(r#"privateKey:"([\w=]+)""#)?
        .captures(source)
        .map(|c| c[1].to_string());

    Ok(Bundle {
        app_id,
        secrets,
        private_key,
    })
}

/// Standard alphabet, padding optional; `None` on anything else.
///
/// Decoding stops at the first `=`, as Python's decoder does. That is load-
/// bearing: the player's blobs are the secret, its padding, then 44 more
/// characters of decoy, and decoding straight through yields 64 bytes of junk.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let bytes: Vec<u8> = text.bytes().take_while(|&c| c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= value(c)? << (18 - 6 * i);
        }
        let produced = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        out.extend_from_slice(&acc.to_be_bytes()[1..1 + produced]);
    }
    Some(out)
}

// -------------------------------------------------------------------- oauth

const SUCCESS_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>Stelly</title>\
<body style=\"font:15px system-ui;background:#12131a;color:#e5e7ef;padding:3rem\">\
<h2>Signed in.</h2><p>You can close this tab and return to the terminal.</p>";
const FAILURE_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>Stelly</title>\
<body style=\"font:15px system-ui;background:#12131a;color:#f7768e;padding:3rem\">\
<h2>No authorisation code in the redirect.</h2><p>Check the terminal for details.</p>";

/// Run the browser flow and return a user auth token.
pub async fn browser_login(bundle: &Bundle, timeout: Duration, open_browser: bool) -> Result<String> {
    let private_key = bundle.private_key.as_deref().context(
        "could not find the OAuth private key in the web player bundle; \
         copy localuser.token from play.qobuz.com's local storage instead",
    )?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let url = format!(
        "{SIGNIN_URL}?ext_app_id={}&redirect_url=http://localhost:{port}",
        bundle.app_id
    );

    eprintln!("\nOpening your browser to sign in to Qobuz.");
    eprintln!("If it does not open, visit this URL:\n\n  {url}\n");
    eprintln!("Waiting up to {}s for the redirect…", timeout.as_secs());
    if open_browser {
        open_url(&url);
    }

    let code = tokio::time::timeout(timeout, catch_code(&listener))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out waiting for the OAuth redirect. You can instead copy \
                 localuser.token from https://play.qobuz.com/ and set QOBUZ_USER_AUTH_TOKEN."
            )
        })??;

    exchange_code(&code, private_key, &bundle.app_id).await
}

/// Answer requests until one carries a code. A browser may ask for a favicon
/// first, so a request without one is not the end.
async fn catch_code(listener: &tokio::net::TcpListener) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = vec![0u8; 8192];
        let read = stream.read(&mut buffer).await?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");

        // Qobuz sends `code_autorisation`; accept plain `code` as well.
        let code = target.split_once('?').and_then(|(_, query)| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                matches!(key, "code_autorisation" | "code").then(|| value.to_string())
            })
        });

        let (status, body) = match &code {
            Some(_) => ("200 OK", SUCCESS_PAGE),
            None => ("400 Bad Request", FAILURE_PAGE),
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(reply.as_bytes()).await;
        if let Some(code) = code.filter(|c| !c.is_empty()) {
            return Ok(code);
        }
    }
}

fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

async fn exchange_code(code: &str, private_key: &str, app_id: &str) -> Result<String> {
    let response = reqwest::Client::new()
        .get(format!("{API_BASE}/oauth/callback"))
        .query(&[("code", code), ("private_key", private_key)])
        .header("X-App-Id", app_id)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        bail!("oauth/callback -> HTTP {status}: {}", &body[..body.len().min(200)]);
    }
    let data: serde_json::Value = serde_json::from_str(&body)?;
    data.get("token")
        .or_else(|| data.get("user_auth_token"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .with_context(|| format!("oauth/callback returned no token: {data}"))
}

// --------------------------------------------------------------------- .env

/// Rewrite `.env` with `updates`, keeping every other line, after backing the
/// old one up to `.env.bak`. Both are left readable by the owner alone.
pub fn update_env(env_dir: &Path, updates: &[(&str, String)]) -> Result<()> {
    let path = env_dir.join(".env");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    if !existing.is_empty() {
        let backup = env_dir.join(".env.bak");
        std::fs::write(&backup, &existing)?;
        restrict(&backup)?;
        eprintln!("backed up the existing .env to .env.bak");
    }

    let mut seen = std::collections::HashSet::new();
    let mut lines: Vec<String> = existing
        .lines()
        .map(|line| {
            let key = line.split('=').next().unwrap_or("").trim();
            match updates.iter().find(|(k, _)| *k == key) {
                Some((k, v)) => {
                    seen.insert(*k);
                    format!("{k}={v}")
                }
                None => line.to_string(),
            }
        })
        .collect();
    for (k, v) in updates {
        if !seen.contains(k) {
            lines.push(format!("{k}={v}"));
        }
    }

    std::fs::write(&path, lines.join("\n") + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    restrict(&path)
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_with_and_without_padding() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert!(base64_decode("a!==").is_none());
    }

    #[test]
    fn pulls_credentials_out_of_a_bundle() {
        // One secret as the web player carried it in September 2026: the
        // seed, then info and extras, with the padding and a decoy tail
        // landing mid-blob.
        let source = r#"x production:{api:{appId:"798273057",appSecret:""} y
            .initialSeed("ZjY5YTc3MzQ2ODZjYjk0Mjc2MjkzNz",window.utimezone.london) z
            name:"Europe/London",info:"hhNGI3YWMzODE=MTlkMTI1OWM1Nj",extras:"VkNDNlNDg2OWU2MjU1YWUxYTdmZTU=" w
            privateKey:"6lz8C03UDIC7""#;
        let bundle = parse_bundle(source).unwrap();
        assert_eq!(bundle.app_id, "798273057");
        assert_eq!(bundle.secrets, vec!["f69a7734686cb9427629378a4b7ac381".to_string()]);
        assert_eq!(bundle.private_key.as_deref(), Some("6lz8C03UDIC7"));
    }
}
