//! A process-wide cache of compiled functions, for object builds.
//!
//! Every case compiles the AIPL it imports again, and the compiler's own sources are most of what
//! the corpus imports, so a process that checks many files compiles the same function bodies
//! over and over. Cranelift's incremental cache keys a compiled function by its IR (less the
//! names of what it calls, which are fixed up afterwards), the target and its flags, and the
//! Cranelift version, so a body seen before comes back as bytes and relocations without running
//! register allocation again.
//!
//! The cache is used while an [`ObjectCompilation`](crate::ObjectCompilation) is built
//! ([`Scope`]) and nowhere else. The JIT backend records more than bytes when it defines a function
//! (the code ranges the profiler maps addresses back to names with), and `define_function_bytes`
//! does not, so the JIT keeps compiling as it did. An object backend's `define_function` is
//! exactly compile, alignment, relocations and `define_function_bytes`, which is what
//! [`define_function`] does with the cache in the middle, so what a hit defines is what a miss
//! would have.
//!
//! The store is bounded: past [`LIMIT`] bytes it stops taking new functions, so a long-lived
//! process does not grow without end. `AIPL_CACHE_STATS=1` prints what the cache has done, and
//! `AIPL_NO_COMPILE_CACHE=1` turns it off.

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::incremental_cache::CacheKvStore;
use cranelift_codegen::Context;
use cranelift_module::{FuncId, Module, ModuleError, ModuleReloc, ModuleResult};
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// How many bytes of compiled functions the store holds before it takes no more.
const LIMIT: usize = 768 * 1024 * 1024;

#[derive(Default)]
struct Store {
    entries: HashMap<Vec<u8>, Vec<u8>>,
    bytes: usize,
    hits: u64,
    misses: u64,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

/// The store, as Cranelift reads and writes it.
struct Shared;

impl CacheKvStore for Shared {
    fn get(&self, key: &[u8]) -> Option<Cow<'_, [u8]>> {
        let store = store().lock().expect("compile cache lock");
        store
            .entries
            .get(key)
            .map(|value| Cow::Owned(value.clone()))
    }

    fn insert(&mut self, key: &[u8], value: Vec<u8>) {
        let mut store = store().lock().expect("compile cache lock");
        if store.bytes + value.len() + key.len() <= LIMIT {
            store.bytes += value.len() + key.len();
            store.entries.insert(key.to_vec(), value);
        }
    }
}

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// While it lives, functions defined on this thread go through the cache.
pub(crate) struct Scope(bool);

impl Scope {
    pub(crate) fn begin() -> Scope {
        let on = std::env::var_os("AIPL_NO_COMPILE_CACHE").is_none();
        Scope(ACTIVE.with(|active| active.replace(on)))
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(self.0));
        if std::env::var_os("AIPL_CACHE_STATS").is_some() {
            let store = store().lock().expect("compile cache lock");
            eprintln!(
                "compile cache: {} hit(s), {} miss(es), {} function(s), {} KiB",
                store.hits,
                store.misses,
                store.entries.len(),
                store.bytes / 1024
            );
        }
    }
}

/// `module.define_function(id, ctx)`, through the cache when a [`Scope`] is open.
pub(crate) fn define_function<M: Module>(
    module: &mut M,
    id: FuncId,
    ctx: &mut Context,
) -> ModuleResult<()> {
    if !ACTIVE.with(Cell::get) {
        return module.define_function(id, ctx);
    }
    let hit = ctx
        .compile_with_cache(module.isa(), &mut Shared, &mut ControlPlane::default())
        .map_err(|e| ModuleError::Compilation(e.inner))?
        .1;
    {
        let mut store = store().lock().expect("compile cache lock");
        if hit {
            store.hits += 1;
        } else {
            store.misses += 1;
        }
    }
    let compiled = ctx
        .compiled_code()
        .expect("a function that was just compiled");
    let alignment = compiled.buffer.min_alignment as u64;
    let relocs: Vec<ModuleReloc> = compiled
        .buffer
        .relocs()
        .iter()
        .map(|reloc| ModuleReloc::from_mach_reloc(reloc, &ctx.func, id))
        .collect();
    module.define_function_bytes(id, alignment, compiled.buffer.data(), &relocs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift::codegen::ir::{types, AbiParam, InstBuilder, UserFuncName};
    use cranelift::frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_codegen::settings::Configurable;
    use cranelift_module::Linkage;
    use cranelift_object::{ObjectBuilder, ObjectModule};

    fn module() -> ObjectModule {
        let mut flags = cranelift::codegen::settings::builder();
        flags.set("opt_level", "none").expect("flag");
        let isa = cranelift_native::builder()
            .expect("host")
            .finish(cranelift::codegen::settings::Flags::new(flags))
            .expect("isa");
        let builder = ObjectBuilder::new(isa, "test", cranelift_module::default_libcall_names())
            .expect("object builder");
        ObjectModule::new(builder)
    }

    /// `name(x) = x + `add``: the same body whatever it is called.
    fn define_adder(module: &mut ObjectModule, name: &str, add: i64) -> FuncId {
        let mut ctx = module.make_context();
        ctx.func.signature.params.push(AbiParam::new(types::I64));
        ctx.func.signature.returns.push(AbiParam::new(types::I64));
        let id = module
            .declare_function(name, Linkage::Export, &ctx.func.signature)
            .expect("declare");
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let x = b.block_params(entry)[0];
            let sum = b.ins().iadd_imm_s(x, add);
            b.ins().return_(&[sum]);
            b.seal_all_blocks();
            b.finalize(module.target_config());
        }
        define_function(module, id, &mut ctx).expect("define");
        id
    }

    fn counts() -> (u64, u64) {
        let store = store().lock().expect("lock");
        (store.hits, store.misses)
    }

    #[test]
    fn a_function_seen_before_is_defined_from_the_cache_and_comes_out_the_same() {
        // The reference: no scope, so the module compiles it itself.
        let mut plain = module();
        define_adder(&mut plain, "adder_a", 7);
        let reference = plain.finish().emit().expect("emit");

        // Inside a scope the first is compiled and kept, and the second, with another name, is
        // the same body and is read back.
        let (hits, misses) = counts();
        let _scope = Scope::begin();
        let mut cached = module();
        define_adder(&mut cached, "adder_a", 7);
        define_adder(&mut cached, "adder_b", 7);
        let (hits_after, misses_after) = counts();
        assert_eq!(misses_after - misses, 1);
        assert_eq!(hits_after - hits, 1);

        // And what it defines is what compiling would have: the first function's bytes match the
        // reference object's.
        let mut again = module();
        define_adder(&mut again, "adder_a", 7);
        let from_cache = again.finish().emit().expect("emit");
        assert_eq!(from_cache, reference);
    }
}
