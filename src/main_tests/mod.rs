//! The binary's unit tests, kept in the crate (not `tests/`) because they
//! exercise items that are private to `main.rs`.

mod catalog;
mod nav;
mod playlist;
mod search;
mod theme;

/// Ignored read-only catalog scenarios require MYX_TEST_CONFIG and a ready,
/// explicitly authorized test daemon. Missing setup is a failure, never a pass.
mod live;
