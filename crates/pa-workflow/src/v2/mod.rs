//! Workflow V2: the durable workflow protocol family
//! (`prime.workflow.*/v2`). This module holds its closed wire — the strict
//! codec, the definition and projection semantic validators, canonical
//! digests, and the retained-host capability profile — shared by every V2
//! boundary.

pub mod capability;
pub mod json;
pub mod projection;
pub mod schema;
pub mod wire;
