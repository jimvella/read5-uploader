//! The Read5 ingest route: one verified PUT per object.
//!
//! ```text
//! PUT  {url}/{key}   Authorization: Bearer r5_{receiver}.{secret}
//!                    Content-MD5: <base64>    -> 200 with the object's ETag
//! HEAD {url}         same header              -> 2xx while the key is live
//! ```
//!
//! The server stores a body only if it matches the Content-MD5 it was sent,
//! and answers with the stored object's MD5 as the ETag, which we check
//! against our own. A key can write only its own receiver's objects.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use md5::{Digest, Md5};
use reqwest::StatusCode;

#[derive(Clone)]
pub struct Ingest {
    http: reqwest::Client,
    url: String,
    key: String,
}

/// The server understood the request and refused it: retrying soon will
/// not help (unknown or disabled key, a path the key may not write, a body
/// it will not take).
#[derive(Debug)]
pub struct Refused {
    pub status: StatusCode,
    pub body: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why = match self.status.as_u16() {
            401 => " (the upload key is unknown or disabled)",
            403 => " (this key may not write here: is the channel id the one Read5 issued?)",
            413 => " (too large)",
            _ => "",
        };
        write!(f, "ingest route answered {}{why}: {}", self.status, self.body.trim())
    }
}

impl std::error::Error for Refused {}

impl Ingest {
    pub fn new(url: &str, key: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("read5-uploader/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client");
        Self {
            http,
            url: url.trim_end_matches('/').to_string(),
            key: key.to_string(),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// PUT `bytes` at `key`, sending Content-MD5 and requiring the returned
    /// ETag to equal the local MD5, so a mismatch means the bytes did not land
    /// intact.
    pub async fn put_verified(&self, key: &str, bytes: Vec<u8>, content_type: &str) -> Result<()> {
        let md5: [u8; 16] = Md5::digest(&bytes).into();
        let md5_hex = hex(&md5);
        let md5_b64 = base64::engine::general_purpose::STANDARD.encode(md5);
        let resp = self
            .http
            .put(format!("{}/{key}", self.url))
            .bearer_auth(&self.key)
            .header("content-type", content_type)
            .header("content-md5", &md5_b64)
            .body(bytes)
            .send()
            .await
            .map_err(|e| anyhow!("put {key}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS {
                return Err(anyhow::Error::new(Refused { status, body }).context(format!("put {key}")));
            }
            bail!("put {key}: ingest route answered {status}: {}", body.trim());
        }
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("put {key}: no ETag in response"))?
            .trim_matches('"')
            .to_ascii_lowercase();
        if etag != md5_hex {
            bail!("put {key}: ETag {etag} != local md5 {md5_hex}");
        }
        Ok(())
    }

    /// Is the route reachable and the key live?
    pub async fn check_key(&self) -> Result<()> {
        let resp = self
            .http
            .head(&self.url)
            .bearer_auth(&self.key)
            .send()
            .await
            .with_context(|| format!("HEAD {}", self.url))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(Refused { status, body: String::new() }.into())
        }
    }
}

/// The receiver id a key belongs to: `r5_{receiver}.{secret}`.
pub fn key_receiver(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("r5_")?;
    let (rx, secret) = rest.split_once('.')?;
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    (ok(rx) && ok(secret)).then_some(rx)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiver_from_key() {
        assert_eq!(key_receiver("r5_3.abcDEF-_123"), Some("3"));
        assert_eq!(key_receiver("r5_perth-pi.x"), Some("perth-pi"));
        assert_eq!(key_receiver("r5_3_abc"), None);
        assert_eq!(key_receiver("r5_.abc"), None);
        assert_eq!(key_receiver("3.abc"), None);
    }
}
