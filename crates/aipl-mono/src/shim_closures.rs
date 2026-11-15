//! Shims whose operations are bound to closures.
//!
//! `shim prints { print = |text| { set seen.push(text); } } { body }` installs a
//! stand-in for `print` at every call site below the shim, so the stand-in is a
//! plain function: an address in a runtime slot, with nowhere to carry what it
//! captured. This pass lowers the closure to such a function and moves what it
//! captures into *cells* for the shim's extent.
//!
//! ```text
//!     shim eff { op = |args| closure_body } { body }
//!
//!   becomes
//!
//!     __shim_cell_set(0, seen);                    // each captured variable, in
//!     let value = shim eff { op = closure } {     // a cell of its own
//!         body
//!     };
//!     set seen = __shim_cell_get(0);               // what the closure set, back
//!     value
//!
//!     fn closure(args) {
//!         mut seen = __shim_cell_get(0);           // a copy of each capture
//!         let result = closure_body;
//!         __shim_cell_set(0, seen);                // what it set, kept
//!         result
//!     }
//! ```
//!
//! A variable is a cell shared by every closure of the shim that captures it, so
//! two operations that both record into one array write to the same one. The
//! checker keeps the shim's own body from using a variable a closure sets
//! (`check_shim_closures`), which is what makes the copy at the end the whole
//! story: while the shim runs, the closures are the only code that sees it.

use super::*;

impl Mono<'_> {}
