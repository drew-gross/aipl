//! FFI and dogfooded-IR suites, merged into one test target.
//! See `tests/compiler.rs` for why these are `mod`s rather than separate targets.
//! Test names are module-qualified — `dogfood_ir::fill_staged_ir`.
#[path = "suites/dogfood_ir.rs"]
mod dogfood_ir;
#[path = "suites/ffi.rs"]
mod ffi;
#[path = "suites/lexer_dogfood.rs"]
mod lexer_dogfood;
