//! `start --trust-cert <PEM>`: extra trust anchors for one WKWebView instance.
//!
//! The question a test asks is "would Safari accept this site once the user
//! trusts this certificate?", so the anchors are added to the system roots and
//! every other check Apple applies stays (the 825-day limit, the host name, the
//! extended key usage). Nothing goes into the keychain: the anchors live in the
//! daemon's memory and are gone at `stop`. The macOS side is in
//! `platform::wk_delegate`.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::OnceLock;

/// One certificate from a `--trust-cert` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub file: String,
    pub der: Vec<u8>,
    pub sha256: String,
}

const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const END: &str = "-----END CERTIFICATE-----";

/// The DER bytes of every `CERTIFICATE` block in a PEM text.
pub fn parse_pem(text: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let body = &rest[start + BEGIN.len()..];
        let end = body
            .find(END)
            .ok_or_else(|| format!("a `{BEGIN}` block has no `{END}` line"))?;
        let b64: String = body[..end].split_whitespace().collect();
        let der = B64
            .decode(b64.as_bytes())
            .map_err(|e| format!("a certificate block is not valid base64: {e}"))?;
        if der.is_empty() {
            return Err("a certificate block is empty".into());
        }
        out.push(der);
        rest = &body[end + END.len()..];
    }
    if out.is_empty() {
        return Err(format!("no `{BEGIN}` block found"));
    }
    Ok(out)
}

pub fn sha256_hex(der: &[u8]) -> String {
    format!("{:x}", Sha256::digest(der))
}

/// Every certificate in the PEM file at `path`.
pub fn read(path: &Path) -> Result<Vec<Anchor>, String> {
    let file = path.display().to_string();
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read --trust-cert {file}: {e}"))?;
    let ders = parse_pem(&text).map_err(|e| format!("--trust-cert {file}: {e}"))?;
    Ok(ders
        .into_iter()
        .map(|der| Anchor {
            file: file.clone(),
            sha256: sha256_hex(&der),
            der,
        })
        .collect())
}

/// Check `--trust-cert` before a daemon is spawned, and make every path
/// absolute: the daemon does not run in the caller's directory. `Err` is the
/// JSON to print.
pub fn check_start(
    paths: &[String],
    engine: crate::engine::Engine,
    macos: bool,
) -> Result<Vec<String>, Value> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if !macos {
        return Err(json!({
            "status": "failed",
            "error": "UNSUPPORTED_PLATFORM",
            "message": "--trust-cert works only with the WKWebView engine on macOS",
            "hint": "WebView2 and Chrome have no per-instance trust anchor that keeps the other \
                     certificate checks. `--browser-arg=--ignore-certificate-errors` accepts any \
                     certificate instead, which answers a different question.",
        }));
    }
    if engine != crate::engine::Engine::WebView {
        return Err(json!({
            "status": "failed",
            "error": "UNSUPPORTED_ON_ENGINE",
            "engine": engine.as_str(),
            "message": "--trust-cert works only with the WKWebView engine (the default on macOS)",
            "hint": "`--browser-arg=--ignore-certificate-errors` makes Chrome accept any \
                     certificate, which answers a different question.",
        }));
    }
    let mut absolute = Vec::new();
    for p in paths {
        let path = std::path::absolute(p).unwrap_or_else(|_| p.into());
        if let Err(message) = read(&path) {
            return Err(json!({ "status": "failed", "error": "USAGE", "message": message }));
        }
        absolute.push(path.display().to_string());
    }
    Ok(absolute)
}

static ANCHORS: OnceLock<Vec<Anchor>> = OnceLock::new();

/// The certificates this daemon trusts, set once before the webview exists.
pub fn set_anchors(anchors: Vec<Anchor>) {
    let _ = ANCHORS.set(anchors);
}

pub fn anchors() -> &'static [Anchor] {
    ANCHORS.get().map(Vec::as_slice).unwrap_or_default()
}

static REPORT: OnceLock<Vec<Value>> = OnceLock::new();

/// Record what the daemon trusts, once, at start.
pub fn set_report(anchors: Vec<Value>) {
    let _ = REPORT.set(anchors);
}

/// `[{subject, sha256, file}]` for `start` and `list`; `null` without the
/// flag, so the short output leaves it out.
pub fn report() -> Value {
    match REPORT.get() {
        Some(anchors) if !anchors.is_empty() => json!(anchors),
        _ => Value::Null,
    }
}

/// The SHA-256 of every anchor a live daemon reported, to compare with a new
/// `start`.
pub fn reported_hashes(info: &Value) -> Vec<String> {
    let mut v: Vec<String> = info["trusted_certs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["sha256"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// The SHA-256 of every certificate in these files.
pub fn hashes_of(paths: &[String]) -> Vec<String> {
    let mut v: Vec<String> = paths
        .iter()
        .flat_map(|p| read(Path::new(p)).unwrap_or_default())
        .map(|a| a.sha256)
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pem(der: &[u8]) -> String {
        let b64 = B64.encode(der);
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!("{BEGIN}\n{}\n{END}\n", lines.join("\n"))
    }

    #[test]
    fn every_certificate_block_in_a_chain_file_is_read() {
        let leaf = vec![0x30u8; 100];
        let ca = vec![0x31u8; 80];
        let text = format!(
            "subject=CN = leaf\n{}\r\nBag Attributes\n{}",
            pem(&leaf).replace('\n', "\r\n"),
            pem(&ca)
        );
        assert_eq!(parse_pem(&text).unwrap(), vec![leaf, ca]);
    }

    #[test]
    fn a_file_without_a_usable_certificate_is_refused_with_the_reason() {
        assert!(
            parse_pem("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----")
                .unwrap_err()
                .contains("no `-----BEGIN CERTIFICATE-----` block")
        );
        assert!(parse_pem(&format!("{BEGIN}\nAAAA"))
            .unwrap_err()
            .contains("no `-----END CERTIFICATE-----` line"));
        assert!(parse_pem(&format!("{BEGIN}\n!!!!\n{END}"))
            .unwrap_err()
            .contains("not valid base64"));
    }

    #[test]
    fn the_hash_is_the_sha256_of_the_der_bytes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn start_refuses_the_flag_where_it_cannot_apply() {
        use crate::engine::Engine;
        let paths = vec!["ca.pem".to_string()];
        assert_eq!(
            check_start(&paths, Engine::WebView, false).unwrap_err()["error"],
            "UNSUPPORTED_PLATFORM"
        );
        assert_eq!(
            check_start(&paths, Engine::Chrome, true).unwrap_err()["error"],
            "UNSUPPORTED_ON_ENGINE"
        );
        assert_eq!(check_start(&[], Engine::Chrome, false), Ok(Vec::new()));
        let missing = vec!["no-such-file.pem".to_string()];
        let e = check_start(&missing, Engine::WebView, true).unwrap_err();
        assert_eq!(e["error"], "USAGE");
        assert!(e["message"]
            .as_str()
            .unwrap()
            .contains("cannot read --trust-cert"));
    }

    #[test]
    fn a_live_daemon_and_a_new_start_compare_by_hash() {
        let info = json!({ "trusted_certs": [
            { "sha256": "bb", "subject": "b" }, { "sha256": "aa", "subject": "a" }
        ]});
        assert_eq!(reported_hashes(&info), vec!["aa", "bb"]);
        assert!(reported_hashes(&json!({})).is_empty());
    }
}
