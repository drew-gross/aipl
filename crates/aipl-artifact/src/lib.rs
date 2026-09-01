//! Linking a checked-in AIPL **dogfood artifact** — CLIF text plus a
//! `;`-comment manifest — into any cranelift [`Module`].
//!
//! The compiler dogfoods AIPL: some `.aipl` sources under `crates/*/src/` are
//! compiled ahead of time into checked-in `.clif` artifacts that the compiler
//! then calls through the FFI. Those artifacts get linked in two places, and
//! this crate is the single implementation both use:
//!
//! * **At build time**, `aipl-codegen`'s `build.rs` links them into an
//!   [`cranelift_object::ObjectModule`], producing machine code that ships
//!   inside the `aipl` binary — so a normal run pays no Cranelift cost at all.
//! * **At run time**, `Compilation::from_artifact` links them into a
//!   `JITModule`. That path is what the `AIPL_DOGFOOD_IR` / `AIPL_FMT_IR`
//!   staging overrides use, so candidate IR can be validated across the corpus
//!   before it is promoted.
//!
//! Keeping one implementation is the point. The declaration order below is not
//! incidental — it reproduces the exact id↔name mapping the artifact's
//! `u0:<id>` references were emitted against — and two copies of that rule
//! would drift into a linker error nobody could read.
//!
//! This crate deliberately knows nothing about AIPL types: the manifest's
//! `; struct` / `; variant` lines and the FFI type tags on `; entry` lines are
//! handed back as raw text for `aipl-codegen` to interpret, because only the
//! runtime side needs them and pulling `aipl-syntax` in here would put it in
//! every build-script dependency graph.

use std::collections::{BTreeMap, HashMap};

use cranelift::codegen::ir::UserFuncName;
use cranelift::prelude::{types, AbiParam, Signature};
use cranelift_module::{DataDescription, FuncId, Linkage, Module};

/// A `; struct` or `; variant` manifest line, handed back unparsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeLine {
    /// Body of a `; struct <body>` line.
    Struct(String),
    /// Body of a `; variant <body>` line.
    Variant(String),
}

/// One FFI-callable function the artifact exposes (a `; entry` line). `params`
/// and `ret` are the raw FFI type tags; this crate never interprets them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub id: u32,
    pub params: Vec<String>,
    pub ret: String,
}

/// A static data object (`; data`) — string literals that outgrew the inline
/// small-string threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataObject {
    pub id: u32,
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The artifact's `;`-comment header, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Runtime builtin imports, by the `FuncId` the CLIF refers to them by.
    pub imports: BTreeMap<u32, String>,
    /// FFI-callable functions, in manifest order.
    pub entries: Vec<Entry>,
    /// Static data objects, sorted by id.
    pub data: Vec<DataObject>,
    /// `; struct` / `; variant` lines, in manifest order, unparsed.
    pub types: Vec<TypeLine>,
}

/// Parse the `;`-comment manifest at the head of an artifact.
///
/// Scanning stops at the first `function ` line: everything the manifest
/// carries precedes the CLIF bodies, and the bodies themselves contain `;`
/// comments (Cranelift annotates each function with its mangled name) that
/// would otherwise be re-examined for no reason. On the prebuilt path this is
/// the *only* work done to the artifact text, so it stays proportional to the
/// header rather than to the megabytes of IR behind it.
pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let mut m = Manifest::default();
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with("function ") {
            break;
        }
        let Some(body) = line.strip_prefix(';') else {
            continue;
        };
        let body = body.trim();
        if let Some(rest) = body.strip_prefix("struct ") {
            m.types.push(TypeLine::Struct(rest.to_string()));
        } else if let Some(rest) = body.strip_prefix("variant ") {
            m.types.push(TypeLine::Variant(rest.to_string()));
        } else if let Some(rest) = body.strip_prefix("import ") {
            let mut it = rest.split_whitespace();
            let id = parse_id(it.next(), "import")?;
            let sym = it
                .next()
                .ok_or_else(|| "`; import` line missing symbol".to_string())?;
            m.imports.insert(id, sym.to_string());
        } else if let Some(rest) = body.strip_prefix("entry ") {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            let name = toks
                .first()
                .ok_or_else(|| "`; entry` line missing name".to_string())?;
            let id = parse_id(toks.get(1).copied(), "entry")?;
            let arrow = toks
                .iter()
                .position(|t| *t == "->")
                .ok_or_else(|| "`; entry` line missing `->`".to_string())?;
            let ret = toks
                .get(arrow + 1)
                .ok_or_else(|| "`; entry` line missing return type".to_string())?;
            m.entries.push(Entry {
                name: name.to_string(),
                id,
                params: toks[2..arrow].iter().map(|t| t.to_string()).collect(),
                ret: ret.to_string(),
            });
        } else if let Some(rest) = body.strip_prefix("data ") {
            // `data <id> <name> <hex-bytes>`
            let mut it = rest.splitn(3, ' ');
            let id = parse_id(it.next(), "data")?;
            let name = it
                .next()
                .ok_or_else(|| "`; data` line missing name".to_string())?
                .to_string();
            let hex = it
                .next()
                .ok_or_else(|| "`; data` line missing bytes".to_string())?;
            if hex.len() % 2 != 0 {
                return Err("`; data` hex string has odd length".to_string());
            }
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| {
                    u8::from_str_radix(&hex[i..i + 2], 16)
                        .map_err(|_| format!("`; data` invalid hex at byte {i}"))
                })
                .collect::<Result<Vec<u8>, _>>()?;
            m.data.push(DataObject { id, name, bytes });
        }
    }
    m.data.sort_by_key(|d| d.id);
    Ok(m)
}

fn parse_id(tok: Option<&str>, kind: &str) -> Result<u32, String> {
    tok.and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed `; {kind}` line: missing/invalid id"))
}

/// How to name the functions an artifact defines when declaring them on a
/// module.
///
/// The JIT does not care — nothing outside the module ever names those symbols,
/// so everything is [`Linkage::Local`] under a generated name. A prebuilt object
/// does care: the handful of `; entry` functions have to survive as real,
/// stably-named symbols for the Rust side to declare `extern` and call, while
/// the several hundred others stay local so they neither collide between
/// artifacts nor clutter the binary's symbol table.
pub struct LinkNames<'a> {
    /// Symbol prefix for locally-defined functions; the id is appended.
    pub local_prefix: &'a str,
    /// Ids to export instead, and the symbol name to export each under.
    pub exports: HashMap<u32, String>,
}

impl LinkNames<'_> {
    /// Everything local — the JIT case.
    pub fn local(prefix: &str) -> LinkNames<'_> {
        LinkNames {
            local_prefix: prefix,
            exports: HashMap::new(),
        }
    }
}

/// Declare and define every function and data object the artifact carries,
/// returning the resulting [`FuncId`]s indexed by artifact id.
///
/// Every function carries its real id in its `function u0:<id>` header, and the
/// bodies refer to each other by that id — so correctness here rests entirely on
/// declaring ids in ascending order, defined functions and runtime imports
/// interleaved exactly as the artifact numbered them. Each declaration is
/// checked against the id it was supposed to get, so a numbering break is a
/// clear error rather than a call landing on the wrong function.
///
/// The caller finalizes: `finalize_definitions` for a JIT module, `finish` for
/// an object one.
pub fn link_artifact<M: Module>(
    text: &str,
    manifest: &Manifest,
    module: &mut M,
    names: &LinkNames<'_>,
) -> Result<Vec<FuncId>, String> {
    // Re-declare static data objects in id order, so the `u1:<id>` references in
    // the function bodies resolve to the right bytes.
    for d in &manifest.data {
        let got = module
            .declare_data(&d.name, Linkage::Local, false, false)
            .map_err(|e| format!("declare artifact data {}: {e}", d.name))?;
        if got.as_u32() != d.id {
            return Err(format!(
                "artifact data id mismatch: expected {}, got {} (declaration order broke)",
                d.id,
                got.as_u32()
            ));
        }
        let mut desc = DataDescription::new();
        desc.set_align(8);
        desc.define(d.bytes.clone().into_boxed_slice());
        module
            .define_data(got, &desc)
            .map_err(|e| format!("define artifact data {}: {e}", d.name))?;
    }

    let parsed =
        cranelift_reader::parse_functions(text).map_err(|e| format!("parse artifact IR: {e}"))?;

    // Each parsed function's id is encoded in its `function u0:<id>` header.
    let mut defined: HashMap<u32, usize> = HashMap::new();
    for (i, f) in parsed.iter().enumerate() {
        let id = match &f.name {
            UserFuncName::User(u) if u.namespace == 0 => u.index,
            other => return Err(format!("artifact function has unexpected name {other:?}")),
        };
        if defined.insert(id, i).is_some() {
            return Err(format!("artifact defines function id {id} twice"));
        }
    }

    let max_id = defined
        .keys()
        .chain(manifest.imports.keys())
        .copied()
        .max()
        .ok_or_else(|| "artifact has no functions".to_string())?;

    let mut ids: Vec<FuncId> = Vec::with_capacity(max_id as usize + 1);
    for id in 0..=max_id {
        let got = match (manifest.imports.get(&id), defined.get(&id)) {
            (Some(sym), None) => {
                let sig = builtin_import_sig(module, sym);
                module
                    .declare_function(sym, Linkage::Import, &sig)
                    .map_err(|e| format!("declare artifact import {sym}: {e}"))?
            }
            (None, Some(&i)) => {
                let sig = parsed[i].signature.clone();
                let (name, linkage) = match names.exports.get(&id) {
                    Some(sym) => (sym.clone(), Linkage::Export),
                    None => (format!("{}{id}", names.local_prefix), Linkage::Local),
                };
                module
                    .declare_function(&name, linkage, &sig)
                    .map_err(|e| format!("declare artifact fn {id}: {e}"))?
            }
            (None, None) => {
                return Err(format!(
                    "artifact has neither a function nor an import for id {id}"
                ))
            }
            (Some(_), Some(_)) => {
                return Err(format!(
                    "artifact id {id} is both a defined function and an import"
                ))
            }
        };
        if got.as_u32() != id {
            return Err(format!(
                "artifact id mismatch: expected {id}, declaration produced {} \
                 (declaration order broke)",
                got.as_u32()
            ));
        }
        ids.push(got);
    }

    let mut ctx = module.make_context();
    for f in &parsed {
        let id = match &f.name {
            UserFuncName::User(u) => u.index,
            _ => unreachable!("validated above"),
        };
        ctx.func = f.clone();
        module
            .define_function(ids[id as usize], &mut ctx)
            .map_err(|e| format!("define artifact fn {id}: {e:?}"))?;
        module.clear_context(&mut ctx);
    }
    Ok(ids)
}

/// Signature of a runtime import `sym`. All take/return i64 (pointers, ints, and
/// `bool`/`char` as i64); they differ only in arity and whether they return.
pub fn builtin_import_sig<M: Module>(module: &mut M, sym: &str) -> Signature {
    let sig = |params: usize, ret: bool| {
        let mut s = module.make_signature();
        for _ in 0..params {
            s.params.push(AbiParam::new(types::I64));
        }
        if ret {
            s.returns.push(AbiParam::new(types::I64));
        }
        s
    };
    match sym {
        "aipl_print"
        | "aipl_print_error"
        | "aipl_inc"
        | "aipl_dec"
        | "aipl_array_dec"
        | "aipl_arr_inc"
        | "aipl_rec_inc_strong"
        | "aipl_rec_dec_strong"
        | "aipl_rec_inc_weak"
        | "aipl_rec_dec_weak"
        | "aipl_count_insns"
        | "aipl_count_call"
        | "aipl_test_begin"
        | "aipl_test_fail" => sig(1, false),
        // Test-runner hooks: `__test_end()`/`__test_begin(name)` return nothing;
        // `__test_summary()` returns the exit code; `__assert(cond, loc)`.
        "aipl_test_end" | "aipl_test_fail_none" => sig(0, false),
        // Takes nothing, returns the reading.
        "aipl_test_summary" | "aipl_now_nanos" | "aipl_monotonic_now" => sig(0, true),
        // Shim slots: read one, or write one (returns nothing).
        "aipl_shim_get" => sig(1, true),
        "aipl_shim_set" => sig(2, false),
        "aipl_assert" => sig(2, false),
        "aipl_arr_drop_str"
        | "aipl_arr_drop_arr"
        | "aipl_arr_retain_ptr"
        | "aipl_arr_drop_opt_str"
        | "aipl_arr_drop_opt_arr"
        | "aipl_arr_retain_opt"
        | "aipl_str_iter_init" => sig(2, false),
        "aipl_arr_load_bit" => sig(2, true),
        "aipl_arr_elem_ptr" => sig(3, true),
        // sret ptr + (program, args): writes the whole composite `Result` via
        // the hidden pointer, so it returns nothing.
        "aipl_execute_program" => sig(3, false),
        "aipl_str_alloc"
        | "aipl_i64_len"
        | "aipl_u64_len"
        | "aipl_str_len"
        | "aipl_trim"
        | "aipl_trim_mut"
        | "aipl_str_hash"
        | "aipl_str_iter_next"
        | "aipl_read_file_to_string"
        | "aipl_list_files"
        | "aipl_str_reverse"
        | "aipl_str_sort" => sig(1, true),
        "aipl_rec_alloc"
        | "aipl_str_repeat"
        | "aipl_str_eq"
        | "aipl_str_cmp"
        | "aipl_str_starts_with"
        | "aipl_str_ends_with"
        | "aipl_str_contains"
        | "aipl_concat"
        | "aipl_concat_lazy"
        | "aipl_concat_mut"
        | "aipl_char_at"
        | "aipl_str_data"
        | "aipl_str_split"
        | "aipl_str_join"
        | "aipl_write_i64"
        | "aipl_write_u64"
        | "aipl_write_string_to_file" => sig(2, true),
        "aipl_write_bytes"
        | "aipl_array_new"
        | "aipl_array_with_cap"
        | "aipl_str_slice"
        | "aipl_str_starts_with_at" => sig(3, true),
        "aipl_set_contains" | "aipl_dict_get" | "aipl_dict_contains_key" | "aipl_arr_reverse" => {
            sig(4, true)
        }
        "aipl_array_push"
        | "aipl_array_push_mut"
        | "aipl_arr_sort"
        | "aipl_arr_reserve"
        | "aipl_arr_extend" => sig(5, true),
        "aipl_set_insert" | "aipl_set_union" | "aipl_set_union_mut" | "aipl_dict_insert"
        | "aipl_arr_slice" => sig(6, true),
        // The wide-`str` (`aipl2_*`) convention, mirroring `builtin_import_sig`
        // in `aipl-codegen`. Every `str` argument is a `*const Str`, so it is one
        // word like any other; a `str` *result* is written through a leading out
        // pointer, which is why the producers below take one more argument than
        // their `aipl_*` counterparts and return nothing.
        "aipl2_inc" | "aipl2_dec" | "aipl2_print" | "aipl2_print_error"
        | "aipl2_test_begin" | "aipl2_test_fail" => sig(1, false),
        "aipl2_assert" => sig(2, false),
        "aipl2_str_len"
        | "aipl2_str_hash"
        | "aipl2_str_write_ptr"
        | "aipl2_str_iter_next" => sig(1, true),
        "aipl2_str_iter_init"
        | "aipl2_arr_drop_str"
        | "aipl2_arr_retain_str"
        | "aipl2_arr_drop_opt_str"
        | "aipl2_arr_retain_opt_str"
        | "aipl2_str_grew"
        // ...and the producers of exactly one `str` from none or one:
        | "aipl2_trim"
        | "aipl2_str_reverse"
        | "aipl2_str_sort"
        | "aipl2_str_alloc" => sig(2, false),
        "aipl2_str_eq"
        | "aipl2_str_cmp"
        | "aipl2_str_starts_with"
        | "aipl2_str_ends_with"
        | "aipl2_str_contains"
        | "aipl2_char_at"
        | "aipl2_str_data" => sig(2, true),
        "aipl2_concat" | "aipl2_str_repeat" | "aipl2_str_join" => sig(3, false),
        "aipl2_str_split" => sig(2, true),
        "aipl2_str_starts_with_at" => sig(3, true),
        "aipl2_str_slice" => sig(4, false),
        other => panic!("unknown builtin import symbol {other:?}"),
    }
}

/// A cheap, stable fingerprint of an artifact's text (FNV-1a).
///
/// It exists so a *test* can catch a prebuilt object that has drifted from the
/// `.clif` it was supposed to be built from. Cargo normally prevents that on its
/// own — `build.rs` declares the artifacts as `rerun-if-changed` inputs — but
/// the consequence if it ever didn't is a compiler silently parsing with last
/// week's dogfood code, which is exactly the kind of quiet wrongness the
/// artifacts are not allowed to have. Hand-rolled rather than `DefaultHasher`
/// because the two sides hash in different processes (build script, test), and
/// this one is specified rather than merely currently-stable.
pub fn fingerprint(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
