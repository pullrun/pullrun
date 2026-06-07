pub mod node;
pub mod store;

pub use node::{ArchivedDagNode, DagNode, NodeKind};
pub use store::{MmapStore, StoreError};

pub type Digest = String;