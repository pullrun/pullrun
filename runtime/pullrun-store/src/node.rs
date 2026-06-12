// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use rkyv::{Archive, Deserialize, Serialize};

use crate::Digest;

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct DagNode {
    pub kind: NodeKind,
    pub edges: Vec<Digest>,
    pub inline_data: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug, PartialEq, Eq))]
pub enum NodeKind {
    Blob,
    Tree,
    Layer,
    Manifest,
    ManifestList,
}

impl DagNode {
    pub fn new(kind: NodeKind, edges: Vec<Digest>, inline_data: Vec<u8>) -> Self {
        Self {
            kind,
            edges,
            inline_data,
        }
    }

    pub fn blob(data: Vec<u8>) -> Self {
        Self {
            kind: NodeKind::Blob,
            edges: vec![],
            inline_data: data,
        }
    }

    pub fn tree(edges: Vec<Digest>, inline_data: Vec<u8>) -> Self {
        Self {
            kind: NodeKind::Tree,
            edges,
            inline_data,
        }
    }

    pub fn layer(edges: Vec<Digest>, inline_data: Vec<u8>) -> Self {
        Self {
            kind: NodeKind::Layer,
            edges,
            inline_data,
        }
    }

    pub fn manifest(edges: Vec<Digest>, inline_data: Vec<u8>) -> Self {
        Self {
            kind: NodeKind::Manifest,
            edges,
            inline_data,
        }
    }

    pub fn manifest_list(edges: Vec<Digest>, inline_data: Vec<u8>) -> Self {
        Self {
            kind: NodeKind::ManifestList,
            edges,
            inline_data,
        }
    }
}

impl ArchivedDagNode {
    pub fn is_blob(&self) -> bool {
        matches!(self.kind, ArchivedNodeKind::Blob)
    }

    pub fn is_tree(&self) -> bool {
        matches!(self.kind, ArchivedNodeKind::Tree)
    }

    pub fn is_layer(&self) -> bool {
        matches!(self.kind, ArchivedNodeKind::Layer)
    }

    pub fn is_manifest(&self) -> bool {
        matches!(self.kind, ArchivedNodeKind::Manifest)
    }

    pub fn is_manifest_list(&self) -> bool {
        matches!(self.kind, ArchivedNodeKind::ManifestList)
    }
}