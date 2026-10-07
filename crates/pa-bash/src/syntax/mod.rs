//! Lexical primitives the guards share: how shell text reads (quotes,
//! escapes, substitutions, continuations, heredocs), the Python string and
//! path semantics the guards were specified in, and the guards' regular
//! expressions.

pub(crate) mod chars;
pub(crate) mod mention;
pub(crate) mod pyre;
