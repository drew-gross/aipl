//! Rebuild an [`aipl_syntax::ast::Program`] from the [`FfiValue`] the AIPL
//! parser hands back.
//!
//! This is the far side of the bridge stage 5a of `PARSER_LIBRARY.md` chose:
//! `grammar_aipl.aipl` lowers a parse to the AST declared in `ast.aipl`, that
//! value crosses the FFI, and this reconstructs the `Program` the compiler's
//! passes actually run on.
//!
//! **It is a mapping, not a translation.** Every name here is the one the AIPL
//! declaration uses, and the AIPL declaration's names are the Rust ones behind a
//! per-type tag — `Type::Optional` is `TyOptional`, `ExprKind::Call` is `ECall`.
//! So a case this file does not know about is a case the two declarations
//! disagree about, and it is reported as exactly that rather than guessed at.
//!
//! What crosses, by shape: a struct as `Struct` with its field names, a variant
//! as `Variant` with its constructor name and positional payload, an optional as
//! `Opt`, an array as `Array`, a `bool` as `Int` 0/1, a `char` as its codepoint,
//! and a tuple as a `Struct` whose fields are `_0`, `_1`, … .

use crate::FfiValue;
use aipl_syntax::ast;
use aipl_syntax::Span;

type R<T> = Result<T, String>;

/// The `Program` behind a `Res(Ok(..))` returned by `aipl_program`.
///
/// A parse failure arrives as the `Err` side and is handed back as its message,
/// so a caller sees "did not parse" and "did not marshal" as the same kind of
/// answer.
pub fn program_from_ffi(v: &FfiValue) -> R<ast::Program> {
    match v {
        FfiValue::Res(Ok(inner)) => program(inner),
        FfiValue::Res(Err(e)) => Err(parse_error_message(e)),
        other => Err(format!("expected a result, got {}", shape(other))),
    }
}

/// A `ParseError` struct as its message, with the span left out — a caller that
/// wants the span has the value.
fn parse_error_message(v: &FfiValue) -> String {
    match field(v, "message").and_then(text) {
        Ok(m) => m,
        Err(e) => e,
    }
}

// ---------- the readers ----------

fn program(v: &FfiValue) -> R<ast::Program> {
    Ok(ast::Program {
        items: each(field(v, "items")?, item)?,
        // A single parsed file has no merged-source map; the loader fills one
        // in when it flattens several files together.
        sources: Vec::new(),
    })
}

fn item(v: &FfiValue) -> R<ast::Item> {
    let (case, payload) = variant(v)?;
    Ok(match case {
        "ItemFn" => ast::Item::Fn(function(at(payload, 0, case)?)?),
        "ItemStruct" => ast::Item::Struct(struct_decl(at(payload, 0, case)?)?),
        "ItemVariant" => ast::Item::Variant(variant_decl(at(payload, 0, case)?)?),
        "ItemImport" => ast::Item::Import(import_decl(at(payload, 0, case)?)?),
        other => return Err(unknown("Item", other)),
    })
}

fn function(v: &FfiValue) -> R<ast::Function> {
    let sig = field(v, "sig")?;
    Ok(ast::Function {
        name: text(field(v, "name")?)?,
        is_pub: flag(field(v, "is_pub")?)?,
        sig: ast::Signature {
            type_vars: each(field(sig, "type_vars")?, type_param)?,
            params: each(field(sig, "params")?, param)?,
            effects: each(field(sig, "effects")?, text)?,
            return_ty: maybe(field(sig, "return_ty")?, ty)?,
        },
        body: expr(field(v, "body")?)?,
        test_body: maybe(field(v, "test_body")?, expr)?,
        doc: maybe(field(v, "doc")?, text)?,
    })
}

fn param(v: &FfiValue) -> R<ast::Param> {
    Ok(ast::Param {
        name: text(field(v, "name")?)?,
        ty: ty(field(v, "ty")?)?,
        mutable: flag(field(v, "mutable")?)?,
        variadic: flag(field(v, "variadic")?)?,
        default: maybe(field(v, "default")?, expr)?,
        implicit_some: flag(field(v, "implicit_some")?)?,
    })
}

fn type_param(v: &FfiValue) -> R<ast::TypeParam> {
    Ok(ast::TypeParam {
        name: text(field(v, "name")?)?,
        bound: bound(field(v, "bound")?)?,
    })
}

fn bound(v: &FfiValue) -> R<ast::Bound> {
    let (case, _) = variant(v)?;
    Ok(match case {
        "BoundAny" => ast::Bound::Any,
        "BoundOrd" => ast::Bound::Ord,
        "BoundVariant" => ast::Bound::Variant,
        other => return Err(unknown("Bound", other)),
    })
}

fn struct_decl(v: &FfiValue) -> R<ast::StructDecl> {
    Ok(ast::StructDecl {
        name: text(field(v, "name")?)?,
        doc: maybe(field(v, "doc")?, text)?,
        type_vars: each(field(v, "type_vars")?, type_param)?,
        fields: each(field(v, "fields")?, field_decl)?,
    })
}

fn field_decl(v: &FfiValue) -> R<ast::FieldDecl> {
    Ok(ast::FieldDecl {
        name: text(field(v, "name")?)?,
        ty: ty(field(v, "ty")?)?,
        default: maybe(field(v, "default")?, expr)?,
    })
}

fn variant_decl(v: &FfiValue) -> R<ast::VariantDecl> {
    Ok(ast::VariantDecl {
        name: text(field(v, "name")?)?,
        doc: maybe(field(v, "doc")?, text)?,
        type_vars: each(field(v, "type_vars")?, type_param)?,
        cases: each(field(v, "cases")?, variant_case)?,
    })
}

fn variant_case(v: &FfiValue) -> R<ast::VariantCase> {
    Ok(ast::VariantCase {
        name: text(field(v, "name")?)?,
        doc: maybe(field(v, "doc")?, text)?,
        payload: each(field(v, "payload")?, case_param)?,
    })
}

fn case_param(v: &FfiValue) -> R<ast::CaseParam> {
    Ok(ast::CaseParam {
        name: maybe(field(v, "name")?, text)?,
        ty: ty(field(v, "ty")?)?,
        default: maybe(field(v, "default")?, expr)?,
        implicit_some: flag(field(v, "implicit_some")?)?,
    })
}

fn import_decl(v: &FfiValue) -> R<ast::ImportDecl> {
    Ok(ast::ImportDecl {
        names: each(field(v, "names")?, import_name)?,
        source: import_source(field(v, "source")?)?,
    })
}

fn import_name(v: &FfiValue) -> R<ast::ImportName> {
    Ok(ast::ImportName {
        name: text(field(v, "name")?)?,
        alias: maybe(field(v, "alias")?, text)?,
        span: span(field(v, "span")?)?,
    })
}

fn import_source(v: &FfiValue) -> R<ast::ImportSource> {
    let (case, payload) = variant(v)?;
    Ok(match case {
        "ImportPath" => ast::ImportSource::Path {
            path: text(at(payload, 0, case)?)?,
            span: span(at(payload, 1, case)?)?,
        },
        "ImportBuiltins" => ast::ImportSource::Builtins {
            span: span(at(payload, 0, case)?)?,
        },
        other => return Err(unknown("ImportSource", other)),
    })
}

fn ty(v: &FfiValue) -> R<ast::Type> {
    use ast::Type as T;
    let (case, payload) = variant(v)?;
    let one = |i: usize| -> R<Box<T>> { Ok(Box::new(ty(at(payload, i, case)?)?)) };
    Ok(match case {
        "TyUnit" => T::Unit,
        "TyPrimitive" => T::Primitive(primitive(at(payload, 0, case)?)?),
        "TyNamed" => T::Named(text(at(payload, 0, case)?)?),
        "TyCase" => T::Case(one(0)?),
        "TyTypeVar" => T::TypeVar(text(at(payload, 0, case)?)?),
        "TyOptional" => T::Optional(one(0)?),
        "TyArray" => T::Array(one(0)?),
        "TySet" => T::Set(one(0)?),
        "TyDict" => T::Dict(one(0)?, one(1)?),
        "TyResult" => T::Result(one(0)?, one(1)?),
        "TyFn" => T::Fn(each(at(payload, 0, case)?, ty)?, one(1)?),
        "TyTuple" => T::Tuple(each(at(payload, 0, case)?, ty)?),
        "TyGeneric" => T::Generic(
            text(at(payload, 0, case)?)?,
            each(at(payload, 1, case)?, ty)?,
        ),
        "TyAny" => T::Any,
        other => return Err(unknown("Ty", other)),
    })
}

fn primitive(v: &FfiValue) -> R<ast::Primitive> {
    use ast::Primitive as P;
    let (case, _) = variant(v)?;
    Ok(match case {
        "I8" => P::I8,
        "I16" => P::I16,
        "I32" => P::I32,
        "I64" => P::I64,
        "U8" => P::U8,
        "U16" => P::U16,
        "U32" => P::U32,
        "U64" => P::U64,
        "Bool" => P::Bool,
        "Char" => P::Char,
        "Str" => P::Str,
        other => return Err(unknown("Prim", other)),
    })
}

fn pattern(v: &FfiValue) -> R<ast::Pattern> {
    let (case, payload) = variant(v)?;
    Ok(match case {
        "PatCtor" => ast::Pattern::Ctor {
            name: text(at(payload, 0, case)?)?,
            bindings: each(at(payload, 1, case)?, text)?,
            ignore_payload: flag(at(payload, 2, case)?)?,
        },
        "PatStr" => ast::Pattern::Str(text(at(payload, 0, case)?)?),
        "PatChar" => ast::Pattern::Char(byte(at(payload, 0, case)?)?),
        "PatArray" => ast::Pattern::Array(each(at(payload, 0, case)?, expr)?),
        "PatWildcard" => ast::Pattern::Wildcard,
        other => return Err(unknown("Pat", other)),
    })
}

fn match_arm(v: &FfiValue) -> R<ast::MatchArm> {
    Ok(ast::MatchArm {
        pattern: pattern(field(v, "pattern")?)?,
        body: expr(field(v, "body")?)?,
        span: span(field(v, "span")?)?,
    })
}

fn lambda_param(v: &FfiValue) -> R<ast::LambdaParam> {
    Ok(ast::LambdaParam {
        name: text(field(v, "name")?)?,
        ty: maybe(field(v, "ty")?, ty)?,
        span: span(field(v, "span")?)?,
    })
}

fn field_init(v: &FfiValue) -> R<ast::FieldInit> {
    Ok(ast::FieldInit {
        name: text(field(v, "name")?)?,
        value: expr(field(v, "value")?)?,
    })
}

fn expr(v: &FfiValue) -> R<ast::Expr> {
    Ok(ast::Expr::new(
        expr_kind(field(v, "kind")?)?,
        span(field(v, "span")?)?,
    ))
}

fn expr_kind(v: &FfiValue) -> R<ast::ExprKind> {
    use ast::ExprKind as K;
    let (case, payload) = variant(v)?;
    let sub = |i: usize| -> R<Box<ast::Expr>> { Ok(Box::new(expr(at(payload, i, case)?)?)) };
    let name = |i: usize| -> R<String> { text(at(payload, i, case)?) };
    Ok(match case {
        "ENum" => K::Num(int(at(payload, 0, case)?)?),
        "EBool" => K::Bool(flag(at(payload, 0, case)?)?),
        "EStr" => K::Str(name(0)?),
        "EChar" => K::Char(byte(at(payload, 0, case)?)?),
        "EIdent" => K::Ident(name(0)?),
        "ECall" => K::Call(
            name(0)?,
            each(at(payload, 1, case)?, expr)?,
            flag(at(payload, 2, case)?)?,
        ),
        "ENeg" => K::Neg(sub(0)?),
        "EIf" => K::If(sub(0)?, sub(1)?, sub(2)?),
        "EConstruct" => K::Construct(name(0)?, each(at(payload, 1, case)?, field_init)?),
        "EField" => K::Field(sub(0)?, name(1)?),
        "ELet" => K::Let(
            name(0)?,
            maybe(at(payload, 1, case)?, ty)?,
            sub(2)?,
            sub(3)?,
        ),
        "ELetMut" => K::LetMut(
            name(0)?,
            maybe(at(payload, 1, case)?, ty)?,
            sub(2)?,
            sub(3)?,
        ),
        "EAssign" => K::Assign(sub(0)?, sub(1)?, sub(2)?),
        "EFor" => K::For(name(0)?, sub(1)?, sub(2)?),
        "EWhile" => K::While(sub(0)?, sub(1)?),
        "EShim" => K::Shim(name(0)?, each(at(payload, 1, case)?, name_pair)?, sub(2)?),
        "ENone" => K::None,
        "EMatch" => K::Match(sub(0)?, each(at(payload, 1, case)?, match_arm)?),
        "EIfLet" => K::IfLet(
            Box::new(match_arm(at(payload, 0, case)?)?),
            sub(1)?,
            sub(2)?,
        ),
        "EArrayLit" => K::ArrayLit(each(at(payload, 0, case)?, expr)?),
        "ESpread" => K::Spread(sub(0)?),
        "ESetLit" => K::SetLit(each(at(payload, 0, case)?, expr)?),
        "EDictLit" => K::DictLit(each(at(payload, 0, case)?, expr_pair)?),
        "EIndex" => K::Index(sub(0)?, sub(1)?),
        "ESlice" => K::Slice(
            sub(0)?,
            sub(1)?,
            maybe(at(payload, 2, case)?, expr)?.map(Box::new),
        ),
        "ETry" => K::Try(sub(0)?),
        "EUnit" => K::Unit,
        "ESeq" => K::Seq(sub(0)?, sub(1)?),
        "EReturn" => K::Return(sub(0)?),
        "ELambda" => K::Lambda(each(at(payload, 0, case)?, lambda_param)?, sub(1)?),
        "ETupleLit" => K::TupleLit(each(at(payload, 0, case)?, expr)?),
        "EKwArg" => K::KwArg(name(0)?, sub(1)?, flag(at(payload, 2, case)?)?),
        other => return Err(unknown("ExprKind", other)),
    })
}

/// A `(str, str)` tuple — a shim's `operation = function` binding.
fn name_pair(v: &FfiValue) -> R<(String, String)> {
    Ok((text(field(v, "_0")?)?, text(field(v, "_1")?)?))
}

/// An `(Expr, Expr)` tuple — one `key: value` of a dict literal.
fn expr_pair(v: &FfiValue) -> R<(ast::Expr, ast::Expr)> {
    Ok((expr(field(v, "_0")?)?, expr(field(v, "_1")?)?))
}

// ---------- the shape helpers ----------

fn field<'a>(v: &'a FfiValue, name: &str) -> R<&'a FfiValue> {
    let FfiValue::Struct(fields) = v else {
        return Err(format!("expected a struct, got {}", shape(v)));
    };
    fields
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, value)| value)
        .ok_or_else(|| format!("no field {name:?} on this struct"))
}

fn variant(v: &FfiValue) -> R<(&str, &[FfiValue])> {
    match v {
        FfiValue::Variant(case, payload) => Ok((case.as_str(), payload.as_slice())),
        other => Err(format!("expected a variant, got {}", shape(other))),
    }
}

fn at<'a>(payload: &'a [FfiValue], i: usize, case: &str) -> R<&'a FfiValue> {
    payload
        .get(i)
        .ok_or_else(|| format!("{case} has no payload slot {i}"))
}

fn each<T>(v: &FfiValue, f: impl Fn(&FfiValue) -> R<T>) -> R<Vec<T>> {
    let FfiValue::Array(elems) = v else {
        return Err(format!("expected an array, got {}", shape(v)));
    };
    elems.iter().map(f).collect()
}

fn maybe<T>(v: &FfiValue, f: impl Fn(&FfiValue) -> R<T>) -> R<Option<T>> {
    match v {
        FfiValue::Opt(None) => Ok(None),
        FfiValue::Opt(Some(inner)) => Ok(Some(f(inner)?)),
        other => Err(format!("expected an optional, got {}", shape(other))),
    }
}

fn text(v: &FfiValue) -> R<String> {
    match v {
        FfiValue::Str(s) => Ok(s.clone()),
        other => Err(format!("expected a string, got {}", shape(other))),
    }
}

fn int(v: &FfiValue) -> R<i64> {
    match v {
        FfiValue::Int(n) => Ok(*n),
        other => Err(format!("expected an integer, got {}", shape(other))),
    }
}

/// A `bool`, which crosses at its `i64` ABI.
fn flag(v: &FfiValue) -> R<bool> {
    Ok(int(v)? != 0)
}

/// A `char`, which crosses as its codepoint. AIPL rejects anything above 0x7F
/// at lex time, so the narrowing cannot lose a character — but it is checked
/// rather than assumed, since a silent truncation here would be a wrong AST.
fn byte(v: &FfiValue) -> R<u8> {
    let n = int(v)?;
    u8::try_from(n).map_err(|_| format!("char literal {n} is not a byte"))
}

fn span(v: &FfiValue) -> R<Span> {
    let start = int(field(v, "start")?)?;
    let end = int(field(v, "end")?)?;
    Ok(start as usize..end as usize)
}

fn unknown(ty_name: &str, case: &str) -> String {
    format!(
        "{ty_name} case {case:?} has no counterpart in `aipl_syntax::ast` — the two \
         declarations have drifted"
    )
}

/// What a value is, for a message about what was expected instead.
fn shape(v: &FfiValue) -> &'static str {
    match v {
        FfiValue::Int(_) => "an integer",
        FfiValue::Str(_) => "a string",
        FfiValue::Opt(_) => "an optional",
        FfiValue::Res(_) => "a result",
        FfiValue::Struct(_) => "a struct",
        FfiValue::Variant(..) => "a variant",
        FfiValue::Array(_) => "an array",
    }
}
