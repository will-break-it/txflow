//! txflow core: the experiment spec, pattern expansion and the graph rules.
//!
//! An experiment describes the UTxO graph to create (which transaction spends
//! which output) with no tx ids. [`expand::expand`] turns a spec into the
//! symbolic graph IR and derives conflicts, orphans and a content hash.
//! [`check`] runs the same steps on spec text and reports the result in a form
//! a person or a language model can act on. Building and submitting
//! transactions is not part of this crate.

pub mod catalogue;
pub mod check;
pub mod cost;
pub mod derive;
pub mod error;
pub mod expand;
pub mod expr;
pub mod ir;
pub mod limits;
pub mod metadata;
pub mod network;
pub mod patterns;
pub mod schedule;
pub mod schema;
pub mod spec;
pub mod validity;
