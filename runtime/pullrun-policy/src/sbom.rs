// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! SBOM (Software Bill of Materials) evaluation.
//!
//! Pullrun stores an optional SBOM alongside each image as a raw blob in the
//! DAG store. The blob digest is derived from the manifest digest with a
//! magic prefix:
//! `sha256("pullrun.sbom.v1\n" + manifest_digest)` hex-encoded.
//!
//! Producers push the rkyv-serialized bytes via
//! `MmapStore::put_blob_blocking(digest, bytes)`. The blob is read back
//! via `MmapStore::get_blob(digest)` and parsed as `SbomBlob`.

use std::sync::Arc;

use rkyv::{Archive, Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tracing::debug;

use pullrun_store::{Digest, MmapStore, StoreError};

use crate::PolicyError;

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SbomBlob {
    pub format: String,
    pub components: Vec<SbomComponent>,
    pub vulnerabilities: Vec<Vulnerability>,
}

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct SbomComponent {
    pub name: String,
    pub version: String,
    pub licenses: Vec<String>,
}

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct Vulnerability {
    pub id: String,
    pub component: String,
    pub cvss: f32,
}

pub enum SbomReport {
    Missing,
    Found(SbomData),
}

pub struct SbomData {
    pub components: Vec<SbomComponent>,
    pub vulnerabilities: Vec<Vulnerability>,
    pub licenses: Vec<String>,
}

impl SbomData {
    pub fn max_cvss(&self) -> Option<f32> {
        self.vulnerabilities
            .iter()
            .map(|v| v.cvss)
            .fold(None, |acc, x| Some(acc.map_or(x, |y| if x > y { x } else { y })))
    }
}

pub fn sbom_digest_for(manifest_digest: &str) -> Digest {
    let mut h = Sha256::new();
    h.update(b"pullrun.sbom.v1\n");
    h.update(manifest_digest.as_bytes());
    let result: [u8; 32] = h.finalize().into();
    Digest(result)
}

/// Encode an `SbomBlob` to rkyv bytes. Helper for producers/tests.
pub fn encode_sbom(blob: &SbomBlob) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let bytes = rkyv::to_bytes::<_, 256>(blob)?;
    Ok(bytes.to_vec())
}

/// Decode an rkyv byte slice as an `SbomBlob`.
pub fn decode_sbom(
    bytes: &[u8],
) -> Result<&ArchivedSbomBlob, Box<dyn std::error::Error>> {
    Ok(rkyv::check_archived_root::<SbomBlob>(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?)
}

pub fn evaluate_sbom(
    store: &Arc<MmapStore>,
    manifest_digest: &str,
) -> Result<SbomReport, PolicyError> {
    let digest = sbom_digest_for(manifest_digest);

    let mmap = match store.get_blob(&digest) {
        Ok(m) => m,
        Err(StoreError::NotFound(_)) => {
            debug!(%manifest_digest, "no SBOM found in DAG");
            return Ok(SbomReport::Missing);
        }
        Err(e) => return Err(e.into()),
    };

    let blob = decode_sbom(&mmap[..]).map_err(|e| PolicyError::SbomMissing(e.to_string()))?;
    let components: Vec<SbomComponent> = blob
        .components
        .iter()
        .map(|c| SbomComponent {
            name: c.name.to_string(),
            version: c.version.to_string(),
            licenses: c.licenses.iter().map(|l| l.as_str().to_string()).collect(),
        })
        .collect();
    let vulnerabilities: Vec<Vulnerability> = blob
        .vulnerabilities
        .iter()
        .map(|v| Vulnerability {
            id: v.id.to_string(),
            component: v.component.to_string(),
            cvss: v.cvss,
        })
        .collect();
    let mut licenses: Vec<String> = components
        .iter()
        .flat_map(|c| c.licenses.iter().cloned())
        .collect();
    licenses.sort();
    licenses.dedup();

    Ok(SbomReport::Found(SbomData {
        components,
        vulnerabilities,
        licenses,
    }))
}

mod cd {
    use serde::Deserialize as SerdeDeserialize;

    #[derive(Debug, SerdeDeserialize)]
    pub struct CycloneDx {
        #[serde(default)]
        pub components: Vec<CdxComponent>,
        #[serde(default)]
        pub vulnerabilities: Vec<CdxVulnerability>,
    }

    #[derive(Debug, SerdeDeserialize)]
    pub struct CdxComponent {
        pub name: String,
        #[serde(default)]
        pub version: String,
        #[serde(default)]
        pub licenses: Vec<CdxLicense>,
    }

    #[derive(Debug, SerdeDeserialize)]
    pub struct CdxLicense {
        pub license: CdxLicenseInner,
    }

    #[derive(Debug, SerdeDeserialize)]
    pub struct CdxLicenseInner {
        #[serde(default)]
        pub id: String,
        #[serde(default)]
        pub name: String,
    }

    #[derive(Debug, SerdeDeserialize)]
    pub struct CdxVulnerability {
        pub id: String,
        #[serde(default)]
        pub ratings: Vec<CdxRating>,
        #[serde(default)]
        pub affects: Vec<CdxAffect>,
    }

    #[derive(Debug, SerdeDeserialize)]
    pub struct CdxRating {
        #[serde(default)]
        pub score: Option<f32>,
    }

    #[derive(Debug, SerdeDeserialize)]
    pub struct CdxAffect {
        #[serde(default, rename = "ref")]
        pub ref_field: String,
    }
}

/// Import a CycloneDX 1.5 JSON document and produce an `SbomBlob` ready to
/// be serialized and written to the DAG store.
pub fn from_cyclonedx_json(json: &str) -> Result<SbomBlob, serde_json::Error> {
    use cd::*;

    let doc: CycloneDx = serde_json::from_str(json)?;
    let components = doc
        .components
        .into_iter()
        .map(|c| SbomComponent {
            name: c.name,
            version: c.version,
            licenses: c
                .licenses
                .into_iter()
                .map(|l| if l.license.id.is_empty() { l.license.name } else { l.license.id })
                .filter(|s| !s.is_empty())
                .collect(),
        })
        .collect();
    let vulnerabilities = doc
        .vulnerabilities
        .into_iter()
        .map(|v| {
            let score = v
                .ratings
                .iter()
                .filter_map(|r| r.score)
                .fold(0.0_f32, f32::max);
            let component = v
                .affects
                .first()
                .map(|a| a.ref_field.clone())
                .unwrap_or_default();
            Vulnerability {
                id: v.id,
                component,
                cvss: score,
            }
        })
        .collect();
    Ok(SbomBlob {
        format: "cyclonedx-1.5".into(),
        components,
        vulnerabilities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sbom_digest_is_deterministic() {
        let a = sbom_digest_for("deadbeef");
        let b = sbom_digest_for("deadbeef");
        assert_eq!(a, b);
        let c = sbom_digest_for("cafebabe");
        assert_ne!(a, c);
    }

    #[test]
    fn test_max_cvss_finds_highest() {
        let data = SbomData {
            components: vec![],
            vulnerabilities: vec![
                Vulnerability {
                    id: "CVE-2024-0001".into(),
                    component: "openssl".into(),
                    cvss: 7.5,
                },
                Vulnerability {
                    id: "CVE-2024-0002".into(),
                    component: "zlib".into(),
                    cvss: 9.8,
                },
            ],
            licenses: vec!["MIT".into()],
        };
        assert_eq!(data.max_cvss(), Some(9.8));
    }

    #[test]
    fn test_max_cvss_empty() {
        let data = SbomData {
            components: vec![],
            vulnerabilities: vec![],
            licenses: vec![],
        };
        assert_eq!(data.max_cvss(), None);
    }

    #[test]
    fn test_evaluate_sbom_missing() {
        let dir = std::env::temp_dir().join("pullrun-policy-sbom-missing");
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(MmapStore::new(dir));
        let r = evaluate_sbom(&store, "nope").unwrap();
        assert!(matches!(r, SbomReport::Missing));
    }

    #[test]
    fn test_evaluate_sbom_present() {
        let dir = std::env::temp_dir().join("pullrun-policy-sbom-present");
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(MmapStore::new(dir));

        let manifest = "abc123";
        let blob = SbomBlob {
            format: "cyclonedx-1.5".into(),
            components: vec![SbomComponent {
                name: "openssl".into(),
                version: "3.0.0".into(),
                licenses: vec!["Apache-2.0".into()],
            }],
            vulnerabilities: vec![Vulnerability {
                id: "CVE-2024-9999".into(),
                component: "openssl".into(),
                cvss: 9.1,
            }],
        };
        let bytes = encode_sbom(&blob).expect("encode");
        let digest = sbom_digest_for(manifest);
        store
            .put_blob_blocking(&digest, &bytes)
            .expect("put sbom blob");

        let r = evaluate_sbom(&store, manifest).unwrap();
        match r {
            SbomReport::Found(d) => {
                assert_eq!(d.components.len(), 1);
                assert_eq!(d.licenses, vec!["Apache-2.0".to_string()]);
                assert_eq!(d.max_cvss(), Some(9.1));
            }
            SbomReport::Missing => panic!("expected SBOM to be found"),
        }
    }

    #[test]
    fn test_from_cyclonedx_json() {
        let json = r#"{
            "bomFormat": "CycloneDX",
            "specVersion": "1.5",
            "components": [
                { "name": "openssl", "version": "3.0.0", "licenses": [{ "license": { "id": "Apache-2.0" }}] }
            ],
            "vulnerabilities": [
                { "id": "CVE-2024-0001", "ratings": [{ "score": 7.5 }], "affects": [{ "ref": "pkg:generic/openssl@3.0.0" }] }
            ]
        }"#;
        let blob = from_cyclonedx_json(json).unwrap();
        assert_eq!(blob.components.len(), 1);
        assert_eq!(blob.components[0].licenses, vec!["Apache-2.0"]);
        assert_eq!(blob.vulnerabilities.len(), 1);
        assert_eq!(blob.vulnerabilities[0].cvss, 7.5);
        assert_eq!(blob.vulnerabilities[0].component, "pkg:generic/openssl@3.0.0");
    }

    #[test]
    fn test_encode_decode_round_trip() {
        let blob = SbomBlob {
            format: "cyclonedx-1.5".into(),
            components: vec![SbomComponent {
                name: "zlib".into(),
                version: "1.2.13".into(),
                licenses: vec!["Zlib".into()],
            }],
            vulnerabilities: vec![],
        };
        let bytes = encode_sbom(&blob).unwrap();
        let archived = decode_sbom(&bytes).unwrap();
        assert_eq!(archived.components.len(), 1);
        assert_eq!(archived.components[0].name, "zlib");
    }
}
