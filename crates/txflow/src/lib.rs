//! `txflow`: build the transactions of an experiment.
//!
//! [`txflow_core`] reads a spec and expands it into a network-free graph IR.
//! [`materialise`] binds that IR to a chain context ([`context`]) and builds
//! and signs every transaction offline.
//!
//! See `specs/` for the design.

pub use txflow_core::{
    catalogue, check, derive, error, expand, expr, ir, limits, metadata, network, patterns,
    schedule, schema, spec,
};

pub mod context;
pub mod funds;
pub mod materialise;
