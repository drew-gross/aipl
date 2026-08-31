//! TEMPORARY spike for `STR_REPR.md`: does Cranelift give us the ABI a 24-byte
//! `str` needs? Delete once its findings are recorded in that document.
//!
//! Four questions, in the order the plan asks them:
//!   1. can a Cranelift function take and return three words, and can another
//!      Cranelift function call it (the AIPL-to-AIPL case)?
//!   2. does that same signature lower for x86-64, not just the aarch64 host?
//!   3. can Cranelift call a Rust `extern "C"` function that takes a str by
//!      pointer and returns one through an out pointer (the runtime boundary)?
//!   4. can it instead pass the three words as three scalar arguments, which
//!      would be cheaper than the pointer for arguments?

use cranelift::{
    codegen::{ir::Function, ir::UserFuncName, isa::CallConv, Context},
    prelude::{
        settings, types, AbiParam, Configurable, FunctionBuilder, FunctionBuilderContext,
        InstBuilder, MemFlagsData, Signature, Value,
    },
};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};

/// The proposed value: base, data, and `len | tag << 56`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AiplStr {
    base: *const u8,
    data: *const u8,
    meta: u64,
}

const BUFFER_TAG: u64 = 0;

fn meta(len: u64, tag: u64) -> u64 {
    len | (tag << 56)
}

/// Runtime-boundary shape (question 3): read a str through a pointer, write the
/// result through an out pointer. `extern "C"` on both sides, no struct passing.
extern "C" fn spike_slice_out(s: *const AiplStr, lo: i64, hi: i64, out: *mut AiplStr) {
    let s = unsafe { &*s };
    let n = (hi - lo) as u64;
    unsafe {
        *out = AiplStr {
            base: s.base,
            data: s.data.wrapping_add(lo as usize),
            meta: meta(n, BUFFER_TAG),
        };
    }
}

/// Scalar-argument shape (question 4): the same three words as three ordinary
/// C arguments. Returns the new length so the caller can check it.
extern "C" fn spike_len_of_words(_base: *const u8, _data: *const u8, meta: u64) -> u64 {
    meta & 0x00FF_FFFF_FFFF_FFFF
}

fn host_isa_flags() -> settings::Flags {
    let mut b = settings::builder();
    b.set("use_colocated_libcalls", "false").unwrap();
    b.set("is_pic", "false").unwrap();
    settings::Flags::new(b)
}

/// `fn(i64, i64, i64) -> (i64, i64, i64)` — three words in, three words out.
fn three_word_sig(call_conv: CallConv) -> Signature {
    let mut sig = Signature::new(call_conv);
    for _ in 0..3 {
        sig.params.push(AbiParam::new(types::I64));
    }
    for _ in 0..3 {
        sig.returns.push(AbiParam::new(types::I64));
    }
    sig
}

/// Build `callee(a, b, c) -> (a + 1, b + 2, c + 3)`.
fn build_callee(
    ctx: &mut Context,
    fbx: &mut FunctionBuilderContext,
    sig: Signature,
    cfg: cranelift::codegen::isa::TargetFrontendConfig,
) {
    ctx.func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
    let mut b = FunctionBuilder::new(&mut ctx.func, fbx);
    let blk = b.create_block();
    b.append_block_params_for_function_params(blk);
    b.switch_to_block(blk);
    b.seal_block(blk);
    let p: Vec<Value> = b.block_params(blk).to_vec();
    let r0 = b.ins().iadd_imm_s(p[0], 1);
    let r1 = b.ins().iadd_imm_s(p[1], 2);
    let r2 = b.ins().iadd_imm_s(p[2], 3);
    b.ins().return_(&[r0, r1, r2]);
    b.finalize(cfg);
}

#[test]
fn q1_three_words_in_and_out_between_cranelift_functions() {
    let mut module = {
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(host_isa_flags())
            .unwrap();
        JITModule::new(JITBuilder::with_isa(
            isa,
            cranelift_module::default_libcall_names(),
        ))
    };
    let cc = module.isa().default_call_conv();
    let mut fbx = FunctionBuilderContext::new();

    // The callee: three words in, three words out.
    let callee_sig = three_word_sig(cc);
    let callee_id = module
        .declare_function("spike_callee", Linkage::Local, &callee_sig)
        .unwrap();
    let mut ctx = module.make_context();
    build_callee(
        &mut ctx,
        &mut fbx,
        callee_sig.clone(),
        module.target_config(),
    );
    module.define_function(callee_id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);

    // The caller: an ABI Rust can spell — `fn(a, b, c, out: *mut i64)` — which
    // calls the three-return callee and stores its results. This is the shape
    // the compiler itself would use at an AIPL-to-AIPL call site.
    let mut caller_sig = Signature::new(cc);
    for _ in 0..4 {
        caller_sig.params.push(AbiParam::new(types::I64));
    }
    let caller_id = module
        .declare_function("spike_caller", Linkage::Export, &caller_sig)
        .unwrap();
    let mut ctx = module.make_context();
    let cfg = module.target_config();
    ctx.func = Function::with_name_signature(UserFuncName::user(0, 1), caller_sig);
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbx);
        let blk = b.create_block();
        b.append_block_params_for_function_params(blk);
        b.switch_to_block(blk);
        b.seal_block(blk);
        let p: Vec<Value> = b.block_params(blk).to_vec();
        let callee_ref = module.declare_func_in_func(callee_id, b.func);
        let call = b.ins().call(callee_ref, &[p[0], p[1], p[2]]);
        let rets: Vec<Value> = b.inst_results(call).to_vec();
        assert_eq!(rets.len(), 3, "cranelift kept three SSA results");
        let flags = MemFlagsData::trusted();
        for (i, r) in rets.iter().enumerate() {
            b.ins().store(flags, *r, p[3], (i * 8) as i32);
        }
        b.ins().return_(&[]);
        b.finalize(cfg);
    }
    module.define_function(caller_id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);
    module.finalize_definitions().unwrap();

    let ptr = module.get_finalized_function(caller_id);
    let caller: extern "C" fn(i64, i64, i64, *mut i64) =
        unsafe { std::mem::transmute::<*const u8, _>(ptr) };
    let mut out = [0i64; 3];
    caller(10, 20, 30, out.as_mut_ptr());
    assert_eq!(out, [11, 22, 33], "three-word return round-tripped");
}

/// Every target the compiler must build for. The host is aarch64; x86-64 is
/// where a host-only design breaks.
const TRIPLES: [&str; 3] = [
    "x86_64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
];

fn isa_for(triple: &str) -> std::sync::Arc<dyn cranelift::codegen::isa::TargetIsa> {
    let mut b = settings::builder();
    b.set("use_colocated_libcalls", "false").unwrap();
    b.set("is_pic", "true").unwrap();
    cranelift::codegen::isa::lookup(triple.parse().unwrap())
        .unwrap_or_else(|e| panic!("{triple}: no isa: {e}"))
        .finish(settings::Flags::new(b))
        .unwrap_or_else(|e| panic!("{triple}: isa finish: {e}"))
}

fn object_module(triple: &str) -> cranelift_object::ObjectModule {
    use cranelift_object::{ObjectBuilder, ObjectModule};
    ObjectModule::new(
        ObjectBuilder::new(
            isa_for(triple),
            "spike",
            cranelift_module::default_libcall_names(),
        )
        .unwrap(),
    )
}

/// Three I64 returns lower on aarch64 (see `q1`) and are **refused on x86-64**:
/// SysV has two integer return registers, and cranelift will not synthesize a
/// return area on its own. Since the checked-in `.clif` artifacts are one text
/// compiled for whichever host loads them, a per-target signature is not an
/// option — so this rules multi-value returns out for `str` everywhere.
#[test]
fn q2_three_word_returns_are_refused_on_x86_64() {
    let mut module = object_module("x86_64-unknown-linux-gnu");
    let cc = module.isa().default_call_conv();
    let sig = three_word_sig(cc);
    let id = module
        .declare_function("spike_callee", Linkage::Export, &sig)
        .unwrap();
    let mut ctx = module.make_context();
    let mut fbx = FunctionBuilderContext::new();
    let cfg = module.target_config();
    build_callee(&mut ctx, &mut fbx, sig, cfg);
    let err = module
        .define_function(id, &mut ctx)
        .expect_err("x86-64 cannot return three words in registers");
    let msg = err.to_string();
    assert!(
        msg.contains("Too many return values") && msg.contains("StructReturn"),
        "unexpected refusal: {msg}"
    );
}

/// The shape that replaces it, and the one the compiler already uses for
/// composite returns (`lib.rs:6034-6100`, "a normal *leading* i64 param — and
/// returns nothing"): `str` arguments as three scalar words, the result written
/// through a plain leading out pointer. Must lower on every target.
#[test]
fn q2b_scalar_params_plus_out_pointer_lower_everywhere() {
    for triple in TRIPLES {
        let mut module = object_module(triple);
        let cc = module.isa().default_call_conv();

        // fn(out: *mut AiplStr, base, data, meta)
        let mut sig = Signature::new(cc);
        for _ in 0..4 {
            sig.params.push(AbiParam::new(types::I64));
        }
        let id = module
            .declare_function("spike_out_ptr", Linkage::Export, &sig)
            .unwrap();
        let mut ctx = module.make_context();
        let mut fbx = FunctionBuilderContext::new();
        let cfg = module.target_config();
        ctx.func = Function::with_name_signature(UserFuncName::user(0, 4), sig);
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbx);
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            b.seal_block(blk);
            let p: Vec<Value> = b.block_params(blk).to_vec();
            let flags = MemFlagsData::trusted();
            for i in 0..3 {
                b.ins().store(flags, p[i + 1], p[0], (i * 8) as i32);
            }
            b.ins().return_(&[]);
            b.finalize(cfg);
        }
        module
            .define_function(id, &mut ctx)
            .unwrap_or_else(|e| panic!("{triple}: define: {e:?}"));
        module.clear_context(&mut ctx);
        assert!(!module.finish().emit().unwrap().is_empty(), "{triple}");
    }
}

/// Cranelift's *formal* `StructReturn` marker also lowers on every target, but
/// only as a parameter with **no** matching return: 0.134 rejects an explicit
/// `StructReturn` return value outright ("Explicit StructReturn return value not
/// allowed"), so the C convention of handing the pointer back is not expressible
/// here. Either way the callee returns nothing, which makes the plain out
/// pointer of `q2b` and this marker the same shape in practice.
#[test]
fn q2d_formal_struct_return_lowers_as_a_param_with_no_return() {
    use cranelift::codegen::ir::ArgumentPurpose;
    for triple in TRIPLES {
        let mut module = object_module(triple);
        let cc = module.isa().default_call_conv();
        let mut sig = Signature::new(cc);
        let mut sret = AbiParam::new(types::I64);
        sret.purpose = ArgumentPurpose::StructReturn;
        sig.params.push(sret);
        for _ in 0..3 {
            sig.params.push(AbiParam::new(types::I64));
        }
        let id = module
            .declare_function("spike_sret", Linkage::Export, &sig)
            .unwrap();
        let mut ctx = module.make_context();
        let mut fbx = FunctionBuilderContext::new();
        let cfg = module.target_config();
        ctx.func = Function::with_name_signature(UserFuncName::user(0, 6), sig);
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbx);
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            b.seal_block(blk);
            let p: Vec<Value> = b.block_params(blk).to_vec();
            let flags = MemFlagsData::trusted();
            for i in 0..3 {
                b.ins().store(flags, p[i + 1], p[0], (i * 8) as i32);
            }
            b.ins().return_(&[]);
            b.finalize(cfg);
        }
        module
            .define_function(id, &mut ctx)
            .unwrap_or_else(|e| panic!("{triple}: define: {e:?}"));
        module.clear_context(&mut ctx);
        assert!(!module.finish().emit().unwrap().is_empty(), "{triple}");
    }
}

/// Six scalar arguments — two `str`s passed as words — lower on every target
/// too, which is the case that would exhaust x86-64's integer argument
/// registers if cranelift did not spill for us.
#[test]
fn q2c_six_scalar_params_lower_everywhere() {
    for triple in TRIPLES {
        let mut module = object_module(triple);
        let cc = module.isa().default_call_conv();
        let mut sig = Signature::new(cc);
        for _ in 0..8 {
            sig.params.push(AbiParam::new(types::I64));
        }
        sig.returns.push(AbiParam::new(types::I64));
        let id = module
            .declare_function("spike_many_params", Linkage::Export, &sig)
            .unwrap();
        let mut ctx = module.make_context();
        let mut fbx = FunctionBuilderContext::new();
        let cfg = module.target_config();
        ctx.func = Function::with_name_signature(UserFuncName::user(0, 5), sig);
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbx);
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            b.seal_block(blk);
            let p: Vec<Value> = b.block_params(blk).to_vec();
            let mut acc = p[0];
            for v in &p[1..] {
                acc = b.ins().iadd(acc, *v);
            }
            b.ins().return_(&[acc]);
            b.finalize(cfg);
        }
        module
            .define_function(id, &mut ctx)
            .unwrap_or_else(|e| panic!("{triple}: define: {e}"));
        module.clear_context(&mut ctx);
        assert!(!module.finish().emit().unwrap().is_empty(), "{triple}");
    }
}

#[test]
fn q3_runtime_boundary_by_pointer_and_out_pointer() {
    let mut module = {
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(host_isa_flags())
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("spike_slice_out", spike_slice_out as *const u8);
        jb.symbol("spike_len_of_words", spike_len_of_words as *const u8);
        JITModule::new(jb)
    };
    let cc = module.isa().default_call_conv();

    // extern "C" fn(*const AiplStr, i64, i64, *mut AiplStr)
    let mut rt_sig = Signature::new(cc);
    for _ in 0..4 {
        rt_sig.params.push(AbiParam::new(types::I64));
    }
    let rt_id = module
        .declare_function("spike_slice_out", Linkage::Import, &rt_sig)
        .unwrap();

    // The AIPL-side caller: `fn(in: *const AiplStr, out: *mut AiplStr)`, which
    // forwards to the runtime with a constant slice range.
    let mut sig = Signature::new(cc);
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    let id = module
        .declare_function("spike_call_rt", Linkage::Export, &sig)
        .unwrap();
    let mut ctx = module.make_context();
    let mut fbx = FunctionBuilderContext::new();
    let cfg = module.target_config();
    ctx.func = Function::with_name_signature(UserFuncName::user(0, 2), sig);
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbx);
        let blk = b.create_block();
        b.append_block_params_for_function_params(blk);
        b.switch_to_block(blk);
        b.seal_block(blk);
        let p: Vec<Value> = b.block_params(blk).to_vec();
        let lo = b.ins().iconst(types::I64, 2);
        let hi = b.ins().iconst(types::I64, 5);
        let rt = module.declare_func_in_func(rt_id, b.func);
        b.ins().call(rt, &[p[0], lo, hi, p[1]]);
        b.ins().return_(&[]);
        b.finalize(cfg);
    }
    module.define_function(id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);
    module.finalize_definitions().unwrap();

    let bytes = b"hello world\0";
    let input = AiplStr {
        base: bytes.as_ptr(),
        data: bytes.as_ptr(),
        meta: meta(11, BUFFER_TAG),
    };
    let mut out = AiplStr {
        base: std::ptr::null(),
        data: std::ptr::null(),
        meta: 0,
    };
    let f: extern "C" fn(*const AiplStr, *mut AiplStr) =
        unsafe { std::mem::transmute::<*const u8, _>(module.get_finalized_function(id)) };
    f(&input, &mut out);
    assert_eq!(out.base, bytes.as_ptr(), "base carried through");
    assert_eq!(out.meta, meta(3, BUFFER_TAG), "len and tag carried through");
    let seen =
        unsafe { std::slice::from_raw_parts(out.data, (out.meta & 0xFF_FFFF_FFFF_FFFF) as usize) };
    assert_eq!(seen, b"llo", "the window points where it should");
    assert_eq!(std::mem::size_of::<AiplStr>(), 24, "value is three words");
}

#[test]
fn q4_runtime_boundary_by_three_scalar_arguments() {
    let mut module = {
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(host_isa_flags())
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("spike_len_of_words", spike_len_of_words as *const u8);
        JITModule::new(jb)
    };
    let cc = module.isa().default_call_conv();

    // extern "C" fn(*const u8, *const u8, u64) -> u64 — a str passed as three
    // ordinary C arguments.
    let mut rt_sig = Signature::new(cc);
    for _ in 0..3 {
        rt_sig.params.push(AbiParam::new(types::I64));
    }
    rt_sig.returns.push(AbiParam::new(types::I64));
    let rt_id = module
        .declare_function("spike_len_of_words", Linkage::Import, &rt_sig)
        .unwrap();

    let mut sig = Signature::new(cc);
    for _ in 0..3 {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig.returns.push(AbiParam::new(types::I64));
    let id = module
        .declare_function("spike_call_words", Linkage::Export, &sig)
        .unwrap();
    let mut ctx = module.make_context();
    let mut fbx = FunctionBuilderContext::new();
    let cfg = module.target_config();
    ctx.func = Function::with_name_signature(UserFuncName::user(0, 3), sig);
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbx);
        let blk = b.create_block();
        b.append_block_params_for_function_params(blk);
        b.switch_to_block(blk);
        b.seal_block(blk);
        let p: Vec<Value> = b.block_params(blk).to_vec();
        let rt = module.declare_func_in_func(rt_id, b.func);
        let call = b.ins().call(rt, &[p[0], p[1], p[2]]);
        let r = b.inst_results(call)[0];
        b.ins().return_(&[r]);
        b.finalize(cfg);
    }
    module.define_function(id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);
    module.finalize_definitions().unwrap();

    let f: extern "C" fn(i64, i64, i64) -> i64 =
        unsafe { std::mem::transmute::<*const u8, _>(module.get_finalized_function(id)) };
    assert_eq!(f(0, 0, meta(42, BUFFER_TAG) as i64), 42);
}
