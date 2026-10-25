//! macOS-specific knowledge. One module, and it is shape knowledge
//! rather than a backend: the snapshot backend (`platform::snapshot`)
//! serves enumeration and readers on this target, and arena release
//! lives in `platform::memory`.

pub(crate) mod bundle;
