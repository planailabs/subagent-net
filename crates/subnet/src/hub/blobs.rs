//! Blob store: content-addressed payloads referred to as `blob:<sha256>`.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Hub, HubError};

pub const MAX_BLOB: usize = 64 << 20;
/// Blobs unused for this long are deleted.
pub const KEEP_DAYS: i64 = 7;
/// What `blob_get` hands a model at most.
const MODEL_LIMIT: usize = 256 << 10;

pub fn hash(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// `blob:<hash>` → `<hash>` (a bare hash is accepted too).
pub fn parse_ref(r: &str) -> Result<&str, HubError> {
    let h = r.strip_prefix("blob:").unwrap_or(r);
    if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(h)
    } else {
        Err(HubError::Bad(format!("not a blob reference: {r:?}")))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BlobRef {
    /// `blob:<sha256>`
    #[serde(rename = "ref")]
    pub reference: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Blob {
    pub mime: String,
    pub size: u64,
    pub base64: String,
}

impl Hub {
    pub async fn put_blob(&self, data: &[u8], mime: &str) -> Result<BlobRef, HubError> {
        if data.len() > MAX_BLOB {
            return Err(HubError::Bad(format!("blob of {} bytes exceeds {MAX_BLOB}", data.len())));
        }
        let h = hash(data);
        self.db.put_blob(&h, mime, data).await?;
        Ok(BlobRef { reference: format!("blob:{h}"), size: data.len() as u64 })
    }

    /// Stores bytes a node claims have hash `h`, checking the claim.
    pub(crate) async fn put_blob_checked(&self, h: &str, mime: &str, data: &[u8]) -> Result<(), HubError> {
        if hash(data) != h {
            return Err(HubError::Bad("blob hash mismatch".into()));
        }
        self.put_blob(data, mime).await.map(|_| ())
    }

    pub async fn get_blob(&self, r: &str) -> Result<(String, Vec<u8>), HubError> {
        let h = parse_ref(r)?;
        self.db.get_blob(h).await?.ok_or_else(|| HubError::NotFound(format!("no blob {h}")))
    }

    /// For models: UTF-8 text as-is, anything else as base64; long blobs are cut.
    pub(crate) async fn blob_for_model(&self, r: &str) -> Result<Value, HubError> {
        let (mime, data) = self.get_blob(r).await?;
        let size = data.len();
        let cut = size > MODEL_LIMIT;
        let data = &data[..size.min(MODEL_LIMIT)];
        Ok(match std::str::from_utf8(data) {
            Ok(text) => json!({"mime": mime, "size": size, "truncated": cut, "text": text}),
            Err(_) => json!({"mime": mime, "size": size, "truncated": cut, "base64": B64.encode(data)}),
        })
    }

    /// Deletes blobs nobody used for `KEEP_DAYS`. Runs for the hub's life.
    pub(crate) async fn blob_gc(self: std::sync::Arc<Self>) {
        let mut every = tokio::time::interval(Duration::from_secs(3600));
        loop {
            every.tick().await;
            match self.db.gc_blobs(KEEP_DAYS).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(deleted = n, "old blobs deleted"),
                Err(e) => tracing::warn!(error = %e, "blob gc failed"),
            }
        }
    }
}

pub fn decode_b64(s: &str) -> Result<Vec<u8>, HubError> {
    B64.decode(s).map_err(|e| HubError::Bad(format!("bad base64: {e}")))
}

pub fn encode_b64(b: &[u8]) -> String {
    B64.encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs() {
        let h = hash(b"x");
        assert_eq!(parse_ref(&format!("blob:{h}")).unwrap(), h);
        assert_eq!(parse_ref(&h).unwrap(), h);
        assert!(parse_ref("blob:nope").is_err());
        assert!(parse_ref("blob:../../etc").is_err());
    }
}
