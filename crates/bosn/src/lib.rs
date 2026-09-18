//! Bosn: a daemon that owns, bounds, and garbage-collects Docker development
//! resources, so agents can use Docker all day without filling the disk.
//!
//! In the workspace this facade re-exports the internal crates. Release
//! packaging (`ci/publish_amalgamate.py`) rewrites it into one self-contained
//! crate with the same module paths, and adds the `bosn` command-line binary.

pub use bosn_core as core;
pub use bosn_engine as engine;
pub use bosn_generation as generation;
pub use bosn_registry as registry;
pub use bosn_service as service;
pub use bosn_setup as setup;
