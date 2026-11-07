// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! ed25519 image signature verification for OCI image DAGs.
//!
//! A signature is a small DAG blob containing:
//! ```text
//! SignatureBlob {
//!   key_id:   "<fingerprint of the public key>"
//!   payload:  "<image_ref>\n<manifest_digest>\n"
//!   signature: ed25519 signature over `payload`
//! }
//! ```
//!
//! The signature blob's digest is deterministic: it is
//! `sha256(format!("{}.{}.{}.{}", key_id, manifest, payload_size, sig_size))`
//! hex-encoded, lowercased, prefixed with the OCI image ref so the same
//! image can have different signature blobs for different keys.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rkyv::{Archive, Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use pullrun_store::Digest;

#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("invalid signature length: {0}")]
    BadLength(usize),
    #[error("ed25519 verification failed: {0}")]
    Verify(String),
    #[error("public key malformed: {0}")]
    Key(String),
}

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SignatureBlob {
    pub key_id: String,
    pub payload: String,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TrustedKey {
    pub id: String,
    pub verifying_key: VerifyingKey,
}

impl TrustedKey {
    pub fn from_base64(id: impl Into<String>, pk_b64: &str) -> Result<Self, SignatureError> {
        let bytes = B64.decode(pk_b64.trim())?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| SignatureError::BadLength(v.len()))?;
        let vk =
            VerifyingKey::from_bytes(&bytes).map_err(|e| SignatureError::Key(e.to_string()))?;
        Ok(Self {
            id: id.into(),
            verifying_key: vk,
        })
    }

    /// Generate a fresh random keypair — DO NOT use in production.
    pub fn for_testing() -> Self {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        Self {
            id: format!("test-key-{}", hex::encode(vk.to_bytes()[..4].as_ref())),
            verifying_key: vk,
        }
    }
}

pub fn verify_signature(
    key: &TrustedKey,
    payload: &[u8],
    signature_bytes: &[u8],
) -> Result<bool, SignatureError> {
    let sig_bytes: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| SignatureError::BadLength(signature_bytes.len()))?;
    let sig = Signature::from_bytes(&sig_bytes);
    match key.verifying_key.verify(payload, &sig) {
        Ok(()) => Ok(true),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("signature") || msg.contains("verify") || msg.contains("Invalid") {
                Ok(false)
            } else {
                Err(SignatureError::Verify(msg))
            }
        }
    }
}

/// Owned view of a stored signature, for reporting to clients.
#[derive(Debug, Clone)]
pub struct SignatureInfo {
    pub key_id: String,
    pub payload: String,
}

/// Read and decode the signature recorded for `image_ref`, if any.
///
/// Returns `Ok(None)` when no signature blob is present.
pub fn read_signature(
    store: &std::sync::Arc<pullrun_store::MmapStore>,
    image_ref: &str,
) -> Result<Option<SignatureInfo>, pullrun_store::StoreError> {
    let digest = signature_digest_for(image_ref);
    let bytes = match store.get_blob(&digest) {
        Ok(b) => b,
        Err(pullrun_store::StoreError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    match rkyv::check_archived_root::<SignatureBlob>(&bytes[..]) {
        Ok(blob) => Ok(Some(SignatureInfo {
            key_id: blob.key_id.to_string(),
            payload: blob.payload.to_string(),
        })),
        // A corrupt blob is reported as "no signature" rather than an
        // error: the caller only needs to know whether one is usable.
        Err(_) => Ok(None),
    }
}

/// Serialize a `SignatureBlob` to the rkyv bytes stored in the DAG.
///
/// The blob is stored at `signature_digest_for(image_ref)`; the runtime
/// derives that key itself, so the write path and the verification path
/// cannot disagree about where a signature lives.
pub fn encode_signature(
    blob: &SignatureBlob,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let bytes = rkyv::to_bytes::<_, 256>(blob)?;
    Ok(bytes.to_vec())
}

/// Compute the deterministic DAG digest for the signature blob of an image.
///
/// The key depends only on the image reference, so verification can find the
/// signature from the reference alone; the manifest digest is checked inside
/// the signed payload, which makes a stale signature fail closed instead of
/// authenticating a different image.
pub fn signature_digest_for(image_ref: &str) -> Digest {
    let mut h = Sha256::new();
    // Domain separator. Renamed from the earlier "cosign" salt while nothing
    // could yet produce a signature (there was no ingestion path), so no
    // existing attestation is invalidated by the change.
    h.update(b"pullrun.image-signature.v1\n");
    h.update(image_ref.as_bytes());
    h.update(b"\n");
    let result: [u8; 32] = h.finalize().into();
    Digest(result)
}

/// Build the canonical payload string that gets signed for a given image + manifest.
pub fn canonical_payload(image_ref: &str, manifest_digest: &str) -> String {
    format!("pullrun-image-v1\n{image_ref}\n{manifest_digest}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn fresh_keypair() -> (TrustedKey, SigningKey) {
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let key = TrustedKey {
            id: format!("test-key-{}", hex::encode(vk.to_bytes()[..4].as_ref())),
            verifying_key: vk,
        };
        (key, sk)
    }

    #[test]
    fn test_signature_round_trip() {
        let (key, sk) = fresh_keypair();
        let payload = b"hello world";
        let sig = sk.sign(payload);
        assert!(verify_signature(&key, payload, &sig.to_bytes()).unwrap());
    }

    #[test]
    fn test_signature_tampered_rejected() {
        let (key, sk) = fresh_keypair();
        let payload = b"hello world";
        let mut sig = sk.sign(payload).to_bytes();
        sig[0] ^= 1;
        let ok = verify_signature(&key, payload, &sig).unwrap();
        assert!(!ok);
    }

    #[test]
    fn test_signature_digest_is_deterministic() {
        let a = signature_digest_for("alpine@sha256:abc");
        let b = signature_digest_for("alpine@sha256:abc");
        assert_eq!(a, b);
        let c = signature_digest_for("alpine@sha256:def");
        assert_ne!(a, c);
    }

    #[test]
    fn test_canonical_payload_format() {
        let p = canonical_payload("alpine:latest", "deadbeef");
        assert!(p.starts_with("pullrun-image-v1\n"));
        assert!(p.contains("alpine:latest"));
        assert!(p.contains("deadbeef"));
    }
}
