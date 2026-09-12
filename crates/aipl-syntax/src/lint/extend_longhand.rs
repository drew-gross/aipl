use crate::ast::{Expr, ExprKind, ImportSource, Item, Program};
use crate::Error;

/// What [`extend_longhand`] needs from the file's imports: every local spelling
/// of the builtin `concat` (`+++`, or the bare name), and what `extend` is
/// called here.
pub(super) struct ExtendNames {
    concat: Vec<String>,
    extend: Option<String>,
}

/// Read those out of the import list. `+++` is bound by import like every
/// operator, so a file whose `+++` is a function of its own has a
/// `set x = x +++ e;` that is not a concatenation at all — the lint stays
/// quiet there. Only a builtins import counts, in either of its forms: the
/// operator alias and the bare call `concat(x, e)` are one call to the loader,
/// and one accumulate to this lint.
pub(super) fn extend_names(program: &Program) -> ExtendNames {
    let mut names = ExtendNames {
        concat: Vec::new(),
        extend: None,
    };
    for item in &program.items {
        let Item::Import(decl) = item else {
            continue;
        };
        if !matches!(decl.source, ImportSource::Builtins { .. }) {
            continue;
        }
        for n in &decl.names {
            match n.name.as_str() {
                "concat" => names.concat.push(n.local().to_string()),
                "extend" => names.extend = Some(n.local().to_string()),
                _ => {}
            }
        }
    }
    names
}

impl ExtendNames {
    /// Nothing to flag anywhere — the driver skips the walk entirely.
    pub(super) fn is_empty(&self) -> bool {
        self.concat.is_empty()
    }
}

/// `set x = x +++ e;` — an append written as a rebuild; `set x.extend(e);` is
/// the form for it.
///
/// The rebuild spelling says "compute the concatenation, then store it", and
/// that is what it costs: a fresh value of the combined length (or a rope node
/// deferring one) on every append, with `x`'s old buffer released afterwards.
/// `extend` grows `x`'s own buffer in place when nothing else holds it, and
/// reserves the destination once for the whole source rather than per element —
/// for a `str` and a `T[]` alike. It also reads as what it is: one binding
/// growing, not two values combined.
///
/// A left-leaning chain counts too. `set x = x +++ a +++ b;` parses as
/// `(x +++ a) +++ b`, so its leftmost operand is still `x` and the rest is what
/// is appended; the advice is one `extend` per operand, which appends both
/// without building `a +++ b` first.
///
/// Only a bare-identifier receiver is flagged, for the reason `extend` takes one
/// at all: it writes the grown value back into that binding's slot, and a field
/// path has no slot to write back to. And only with `x` on the *left* —
/// `set x = e +++ x;` prepends, which `extend` cannot spell.
pub(super) fn extend_longhand(e: &Expr, names: &ExtendNames, hits: &mut Vec<Error>) {
    let ExprKind::Assign(lhs, value, _) = &e.kind else {
        return;
    };
    let ExprKind::Ident(name) = &lhs.kind else {
        return;
    };
    // An operator use is a call named for the spelling written.
    let is_concat = |e: &Expr| match &e.kind {
        ExprKind::Call(f, args, _) if args.len() == 2 => names.concat.iter().any(|c| c == f),
        _ => false,
    };
    if !is_concat(value) {
        return;
    }
    // Walk down the left spine of the chain to its first operand.
    let mut leftmost: &Expr = value;
    let mut operands = 1;
    while is_concat(leftmost) {
        let ExprKind::Call(_, args, _) = &leftmost.kind else {
            unreachable!("is_concat matched a call");
        };
        leftmost = &args[0];
        operands += 1;
    }
    if !matches!(&leftmost.kind, ExprKind::Ident(n) if n == name) {
        return;
    }
    let per_operand = if operands > 2 {
        " (one per appended operand)"
    } else {
        ""
    };
    // Name the import too when it's missing: following the advice without it
    // only trades this error for an unknown-name one.
    let (extend, import) = match &names.extend {
        Some(local) => (local.as_str(), String::new()),
        None => ("extend", ", importing `extend` from builtins".to_string()),
    };
    hits.push(Error::at(
        format!(
            "\"{name}\" is rebuilt to append to it — grow it in place with \
             \"set {name}.{extend}(...);\"{per_operand}{import} (or append #[allow] to this \
             line to keep it)"
        ),
        e.span.clone(),
    ));
}
