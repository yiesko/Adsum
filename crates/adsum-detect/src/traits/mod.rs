//! Platform seams as contracts: the shapes every backend must fill.
//!
//! One trait today (`ProcessEnumerator`); it lives in its own module so
//! the seam is grep-able and the next one (a watcher abstraction, if a
//! platform ever grows event delivery) has an obvious home.

mod enumerator;

pub(crate) use enumerator::ProcessEnumerator;
