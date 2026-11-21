// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

pub mod base;
pub mod confinement;
pub mod cow;
pub mod converter;
pub mod dag_export;
pub mod dag_import;
pub mod dockerfile;
pub mod materializer;
pub mod oci_layout;
pub mod puller;
pub mod push;

pub use base::{bundle_from_manifest, prepare_bundle_from_base, BaseStore, BaseTree};
pub use confinement::{ConfineError, RelPath, RootDir};
pub use cow::{CloneStats, CowMode};
pub use converter::{DirectoryEntry, ManifestData, OciToDagConverter};

pub use dag_export::export_dag_to_tar;
pub use dag_import::import_dag_from_tar;
pub use dockerfile::{
    build_dag_from_directory, build_dag_from_directory_with_platform, BuildStage, DagDirectory,
    Dockerfile, Instruction,
};
pub use materializer::{MaterializedBundle, OciMaterializer};
pub use puller::{
    current_arch, empty_json_descriptor, parse_platform, CompressionFormat, OciAuth, OciDescriptor,
    OciError, OciImageConfig, OciImageIndex, OciManifest, OciPuller, PulledImage, PulledImageList,
};
pub use push::DagPusher;
