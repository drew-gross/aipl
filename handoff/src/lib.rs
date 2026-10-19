//! The part of `cargo handoff` the test suite shares with it.
//!
//! Only [`protocol`]: the lines a failing test prints to say "a refill fixes
//! this". The gate (`main.rs`) is the only reader; the tests under `tests/` are
//! the writers, and a dev-dependency on this crate is how both ends come to
//! spell the line from one definition rather than from two copies of a format.

pub mod protocol;
