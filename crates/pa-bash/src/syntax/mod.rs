//! The shell syntax every check reads: one parser turning a script into a
//! typed tree ([`ast`]), and the walks over it.

pub(crate) mod ast;
pub(crate) mod parse;
pub(crate) mod walk;
