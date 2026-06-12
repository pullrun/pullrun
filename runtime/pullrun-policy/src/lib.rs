// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tracing::{debug, warn};

pub use pullrun_store::{Digest as BlobDigest, MmapStore};

pub mod cosign;
pub mod sbom;

pub use cosign::{verify_cosign_signature, CosignKey, SignatureBlob};
pub use sbom::{evaluate_sbom, SbomReport, Vulnerability};

/// Declarative policy applied to a workload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    pub required_signature: bool,
    pub readonly_rootfs: bool,
    pub no_new_privileges: bool,
    pub seccomp_profile: Option<String>,
    pub allowed_syscalls: Vec<String>,
    pub labels: HashMap<String, String>,

    pub max_cvss_score: Option<f32>,
    pub trusted_signers: Vec<String>,
    pub deny_licenses: Vec<String>,
    pub require_sbom: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny(String),
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("Policy violation: {0}")]
    Violation(String),
    #[error("Signature required but not present")]
    MissingSignature,
    #[error("Invalid signature: {0}")]
    InvalidSignature(String),
    #[error("SBOM missing or unreadable: {0}")]
    SbomMissing(String),
    #[error("Invalid seccomp profile: {0}")]
    InvalidSeccompProfile(String),
    #[error("Store error: {0}")]
    Store(String),
}

impl From<pullrun_store::StoreError> for PolicyError {
    fn from(e: pullrun_store::StoreError) -> Self {
        PolicyError::Store(e.to_string())
    }
}

#[derive(Clone)]
pub struct PolicyEngine {
    default_policy: Policy,
    trusted_keys: Vec<CosignKey>,
}

impl PolicyEngine {
    pub fn new(default_policy: Policy) -> Self {
        Self {
            default_policy,
            trusted_keys: Vec::new(),
        }
    }

    pub fn with_trusted_keys(mut self, keys: Vec<CosignKey>) -> Self {
        self.trusted_keys = keys;
        self
    }

    pub fn add_trusted_key(&mut self, key: CosignKey) {
        self.trusted_keys.push(key);
    }

    pub fn default_policy(&self) -> &Policy {
        &self.default_policy
    }

    pub fn trusted_keys(&self) -> &[CosignKey] {
        &self.trusted_keys
    }

    /// Evaluate policy for a workload whose OCI image has already been
    /// pulled and converted into the DAG store.
    ///
    /// `image_ref` is the canonical image reference (e.g. `alpine@sha256:abc...`).
    /// `manifest_digest` is the rkyv manifest digest in the store.
    pub fn evaluate_for_image(
        &self,
        policy: &Policy,
        store: &Arc<MmapStore>,
        image_ref: &str,
        manifest_digest: &str,
    ) -> Result<PolicyDecision, PolicyError> {
        debug!(%image_ref, %manifest_digest, "evaluating policy");

        if policy.required_signature {
            match self.check_signature(store, image_ref)? {
                SignatureCheck::Valid => {}
                SignatureCheck::Missing => {
                    return Ok(PolicyDecision::Deny(format!(
                        "image {image_ref} is not signed by a trusted key"
                    )));
                }
                SignatureCheck::Invalid(reason) => {
                    return Ok(PolicyDecision::Deny(format!(
                        "signature for {image_ref} is invalid: {reason}"
                    )));
                }
            }
        }

        if policy.require_sbom || policy.max_cvss_score.is_some() || !policy.deny_licenses.is_empty() {
            match evaluate_sbom(store, manifest_digest)? {
                SbomReport::Missing => {
                    if policy.require_sbom {
                        return Ok(PolicyDecision::Deny(format!(
                            "no SBOM found for {image_ref}"
                        )));
                    }
                }
                SbomReport::Found(report) => {
                    if let Some(max) = policy.max_cvss_score {
                        if let Some(highest) = report.max_cvss() {
                            if highest >= max {
                                return Ok(PolicyDecision::Deny(format!(
                                    "image {image_ref} has vulnerability with CVSS {highest:.1} \
                                     (policy max {max:.1})"
                                )));
                            }
                        }
                    }
                    for banned in &policy.deny_licenses {
                        if report.licenses.iter().any(|l| l.eq_ignore_ascii_case(banned)) {
                            return Ok(PolicyDecision::Deny(format!(
                                "image {image_ref} contains banned license: {banned}"
                            )));
                        }
                    }
                }
            }
        }

        if !policy.allowed_syscalls.is_empty() && policy.seccomp_profile.is_none() {
            warn!(
                "allowed_syscalls set but no seccomp_profile — ignoring syscall whitelist"
            );
        }

        if let Some(profile) = &policy.seccomp_profile {
            if profile != "default" && profile != "unconfined" && !profile.starts_with("pullrun:") {
                return Ok(PolicyDecision::Deny(format!(
                    "unknown seccomp profile: {profile}"
                )));
            }
        }

        Ok(PolicyDecision::Allow)
    }

    pub fn evaluate(&self, workload_policy: &Policy) -> PolicyDecision {
        if workload_policy.required_signature && self.trusted_keys.is_empty() {
            return PolicyDecision::Deny(
                "signature required but no trusted keys configured".into(),
            );
        }
        PolicyDecision::Allow
    }

    fn check_signature(
        &self,
        store: &Arc<MmapStore>,
        image_ref: &str,
    ) -> Result<SignatureCheck, PolicyError> {
        let sig_digest = cosign::signature_digest_for(image_ref);
        let sig_mmap = match store.get_blob(&sig_digest) {
            Ok(m) => m,
            Err(pullrun_store::StoreError::NotFound(_)) => return Ok(SignatureCheck::Missing),
            Err(e) => return Err(e.into()),
        };

        let blob = rkyv::check_archived_root::<SignatureBlob>(&sig_mmap[..])
            .map_err(|e| PolicyError::InvalidSignature(format!("corrupt signature blob: {e}")))?;
        let payload = blob.payload.as_str().as_bytes().to_vec();
        let signature = blob.signature.to_vec();

        for key in &self.trusted_keys {
            if key.id != blob.key_id {
                continue;
            }
            match verify_cosign_signature(key, &payload, &signature) {
                Ok(true) => return Ok(SignatureCheck::Valid),
                Ok(false) => continue,
                Err(e) => return Ok(SignatureCheck::Invalid(e.to_string())),
            }
        }

        Ok(SignatureCheck::Invalid(
            "no trusted key matched the signature key_id".into(),
        ))
    }
}

#[derive(Debug)]
enum SignatureCheck {
    Valid,
    Missing,
    Invalid(String),
}

/// Helper: SHA-256 of a string, hex-encoded. Used to derive the DAG
/// digest for a cosign signature blob stored alongside the image.
pub fn sha256_hex(input: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(input);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_policy_allows() {
        let engine = PolicyEngine::new(Policy::default());
        let decision = engine.evaluate(&Policy::default());
        assert_eq!(decision, PolicyDecision::Allow);
    }

    #[test]
    fn test_signature_required_without_keys_denies() {
        let engine = PolicyEngine::new(Policy::default());
        let policy = Policy {
            required_signature: true,
            ..Default::default()
        };
        let decision = engine.evaluate(&policy);
        assert!(matches!(decision, PolicyDecision::Deny(_)));
    }

    #[test]
    fn test_signature_required_with_keys_allows_in_evaluate() {
        use crate::cosign::{self, SignatureBlob};
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let dir = std::env::temp_dir().join("pullrun-test-policy-sig-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(MmapStore::new(dir));

        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key().clone();
        let key = CosignKey {
            id: "test-1".into(),
            verifying_key: vk,
        };

        let image_ref = "alpine:latest";
        let manifest = "deadbeef";
        let payload = cosign::canonical_payload(image_ref, manifest);
        let sig = sk.sign(payload.as_bytes());
        let blob = SignatureBlob {
            key_id: key.id.clone(),
            payload: payload.clone(),
            signature: sig.to_bytes().to_vec(),
        };
        let bytes = rkyv::to_bytes::<_, 256>(&blob).unwrap();
        let digest = cosign::signature_digest_for(image_ref);
        store.put_blob_blocking(&digest, &bytes).unwrap();

        let engine = PolicyEngine::new(Policy::default()).with_trusted_keys(vec![key]);
        let policy = Policy {
            required_signature: true,
            ..Default::default()
        };
        let d = engine
            .evaluate_for_image(&policy, &store, image_ref, manifest)
            .unwrap();
        assert_eq!(d, PolicyDecision::Allow);
    }

    #[test]
    fn test_seccomp_profile_default_ok() {
        let engine = PolicyEngine::new(Policy::default());
        let mut policy = Policy::default();
        policy.seccomp_profile = Some("default".into());
        let store = Arc::new(MmapStore::new(std::env::temp_dir().join("pullrun-test-policy")));
        let d = engine
            .evaluate_for_image(&policy, &store, "alpine:latest", "deadbeef")
            .unwrap();
        assert_eq!(d, PolicyDecision::Allow);
    }

    #[test]
    fn test_seccomp_profile_unknown_denies() {
        let engine = PolicyEngine::new(Policy::default());
        let mut policy = Policy::default();
        policy.seccomp_profile = Some("bogus-profile".into());
        let store = Arc::new(MmapStore::new(std::env::temp_dir().join("pullrun-test-policy")));
        let d = engine
            .evaluate_for_image(&policy, &store, "alpine:latest", "deadbeef")
            .unwrap();
        assert!(matches!(d, PolicyDecision::Deny(_)));
    }
}
