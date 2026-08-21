//! Cross-crate invariants of the effect-shim machinery.
//!
//! The behavior of shims is covered by `tests/cases/effects/shim_*.aipl`. What
//! can't be expressed there is the agreement between the compiler's slot
//! numbering and the *two* runtimes that reserve those slots — the JIT runtime
//! in `aipl-codegen` and the no-std linker runtime, which is compiled to a
//! staticlib and so can't be asked about its array size at runtime from here.

use aipl_syntax::{effect_operations, shim_operations, shim_slot_index, SHIM_SLOT_COUNT};

/// `SHIM_SLOT_COUNT` is what both runtimes size their slot array by, so it must
/// match the number of operations the compiler will hand out indices for. Adding
/// an effect to `SHIMMABLE_EFFECTS` without bumping it would let codegen emit an
/// index past the end of the array, where a shim silently never installs.
#[test]
fn slot_count_covers_every_operation() {
    let ops: Vec<_> = shim_operations().collect();
    assert_eq!(
        ops.len(),
        SHIM_SLOT_COUNT,
        "SHIM_SLOT_COUNT ({SHIM_SLOT_COUNT}) doesn't match the {} shimmable \
         operation(s) ({ops:?}); update it and both runtimes' SHIM_SLOTS arrays",
        ops.len()
    );
    // Indices are dense and unique — they index an array directly.
    let mut seen: Vec<usize> = ops
        .iter()
        .map(|(_, op)| shim_slot_index(op).expect("a listed operation has a slot"))
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, (0..ops.len()).collect::<Vec<_>>());
}

/// The linker runtime is a separate no-std staticlib, so its array size can only
/// be checked against the source. It must reserve exactly `SHIM_SLOT_COUNT`
/// slots — a short array means an out-of-range index reads as "no shim" and the
/// shim silently doesn't apply in AOT builds while working fine under the JIT.
#[test]
fn linker_runtime_reserves_every_slot() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/crates/aipl-linker/runtime/aipl_runtime.rs"
    ))
    .expect("read the linker runtime source");
    let expected = format!("static SHIM_SLOTS: [core::sync::atomic::AtomicI64; {SHIM_SLOT_COUNT}]");
    assert!(
        src.contains(&expected),
        "the linker runtime must declare `{expected}` to match SHIM_SLOT_COUNT \
         ({SHIM_SLOT_COUNT}); found a different slot array"
    );
}

/// Every operation named in the shimmable table has to be a real builtin, since
/// a shim is checked against the operation's declared signature and codegen
/// routes that builtin's call site through the slot. A typo here would surface
/// as a confusing "unknown function" much later.
#[test]
fn every_operation_is_a_builtin() {
    for (effect, op) in shim_operations() {
        assert!(
            aipl_syntax::IMPORTABLE_BUILTINS.contains(&op),
            "effect \"!{effect}\" lists operation {op:?}, which is not an importable builtin"
        );
    }
}

/// A non-shimmable effect has no operation list — that's exactly what the
/// checker keys "this effect can't be shimmed" off.
#[test]
fn unshimmable_effects_have_no_operations() {
    assert!(effect_operations("prints").is_none());
    assert!(effect_operations("clock").is_some());
}
