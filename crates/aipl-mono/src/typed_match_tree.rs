//! A `match` with nested patterns, compiled from the typed tree to a decision tree of the simple
//! forms every later pass knows: a `match` per constructor test, an `if` per literal test, a
//! `let` per binder. The checker has proved the arms exhaustive and each reachable, which is what
//! lets the tree be built without a fallthrough.
//!
//! The scrutinee is bound to a temporary once and the tree reads it through further temporaries,
//! one per payload slot or tuple field it looks at, so no sub-value is computed twice. A tuple
//! literal scrutinee every arm takes apart is matched element by element and never becomes a
//! struct.
//!
//! The tree is Maranget's: the first column the first row constrains is tested, the matrix is
//! specialized per constructor (or per literal, with the wildcard rows as the default), and a row
//! whose remaining patterns are all wildcards is a leaf. A leaf that reaches an arm through several
//! paths gets its own copy of the arm's body.

use std::collections::HashMap;

use aipl_syntax::ast::{Callee, ConcreteType, Primitive, Type};
use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TPattern, TyRepr};
use aipl_syntax::Span;

use crate::typed_adapter::Typed;
use crate::Mono;

type Pattern = TPattern<ConcreteType>;

/// One row of the matrix: a pattern per column, the arm it belongs to, and the binders collected
/// so far, each the user's name for a column temporary.
#[derive(Clone)]
struct Row {
    pats: Vec<Pattern>,
    arm: usize,
    binds: Vec<(String, String)>,
}

type Columns = Vec<(String, ConcreteType)>;

/// Whether a pattern uses any of the nested forms, which an arm has to be compiled away from.
pub(crate) fn is_nested<T: TyRepr>(pattern: &TPattern<T>) -> bool {
    matches!(
        pattern,
        TPattern::Tuple(_)
            | TPattern::Nested { .. }
            | TPattern::ArrayNested(_)
            | TPattern::Int(_)
            | TPattern::Bind(_)
    )
}

/// Whether every arm takes a tuple scrutinee of `arity` apart, so the tuple need never exist.
pub(crate) fn matches_elementwise<T: TyRepr>(arms: &[TMatchArm<T>], arity: usize) -> bool {
    arms.iter().all(|a| {
        matches!(&a.pattern, TPattern::Tuple(ps) if ps.len() == arity)
            || matches!(a.pattern, TPattern::Wildcard)
    })
}

/// The element patterns of an array pattern, in either form.
fn array_elems(pattern: &Pattern) -> Option<Vec<Pattern>> {
    match pattern {
        TPattern::ArrayNested(ps) => Some(ps.clone()),
        TPattern::Array(elems) => elems
            .iter()
            .map(|e| match &e.kind {
                TExprKind::Ident(n) if n == "_" => Some(TPattern::Wildcard),
                TExprKind::Ident(n) => Some(TPattern::Bind(n.clone())),
                TExprKind::Str(s) => Some(TPattern::Str(s.clone())),
                TExprKind::Int(n) => Some(TPattern::Int(*n)),
                TExprKind::Char(c) => Some(TPattern::Char(*c)),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// A case name without its `@Variant` qualification.
fn bare(name: &str) -> &str {
    name.split('@').next().unwrap_or(name)
}

fn replace_col(row: &Row, col: usize, subs: Vec<Pattern>) -> Row {
    let mut pats = row.pats[..col].to_vec();
    pats.extend(subs);
    pats.extend(row.pats[col + 1..].iter().cloned());
    Row {
        pats,
        arm: row.arm,
        binds: row.binds.clone(),
    }
}

fn drop_col(row: &Row, col: usize) -> Row {
    replace_col(row, col, Vec::new())
}

fn splice_cols(
    cols: &[(String, ConcreteType)],
    col: usize,
    subs: &[(String, ConcreteType)],
) -> Columns {
    let mut out = cols[..col].to_vec();
    out.extend(subs.iter().cloned());
    out.extend(cols[col + 1..].iter().cloned());
    out
}

struct Tree<'m, 'a> {
    mono: &'m mut Mono<'a>,
    bodies: &'m [Typed],
    span: Span,
    /// The type of the whole `match`, which every node of the tree has.
    result: ConcreteType,
    /// The type of every column temporary made so far, for the binders a leaf reads them with.
    known: HashMap<String, ConcreteType>,
}

impl Tree<'_, '_> {
    /// A fresh temporary of type `ty`. `$` keeps it out of any user's namespace.
    fn column(&mut self, ty: ConcreteType) -> (String, ConcreteType) {
        let var = self.mono.names.temp("mt");
        self.known.insert(var.clone(), ty.clone());
        (var, ty)
    }

    fn ident(&self, var: &str, ty: &ConcreteType) -> Typed {
        TExpr::new(
            TExprKind::Ident(var.to_string()),
            ty.clone(),
            self.span.clone(),
        )
    }

    fn node(&self, kind: TExprKind<ConcreteType>) -> Typed {
        TExpr::new(kind, self.result.clone(), self.span.clone())
    }

    fn concrete(t: Type) -> ConcreteType {
        t.to_concrete()
            .unwrap_or_else(|| panic!("a column type is concrete: {t:?}"))
    }

    fn compile(&mut self, rows: Vec<Row>, cols: &[(String, ConcreteType)]) -> Typed {
        assert!(!rows.is_empty(), "a nested match compiled to an empty case");
        // Binders and `_` constrain nothing: absorb them into the row's binds and treat the column
        // as a wildcard for it.
        let rows: Vec<Row> = rows
            .into_iter()
            .map(|mut row| {
                for (p, (var, _)) in row.pats.iter_mut().zip(cols) {
                    if let TPattern::Bind(name) = p {
                        row.binds.push((name.clone(), var.clone()));
                        *p = TPattern::Wildcard;
                    }
                }
                row
            })
            .collect();
        // A leaf: the first row's remaining patterns are all wildcards, so it is the arm every
        // value reaching here takes.
        let Some(col) = rows[0]
            .pats
            .iter()
            .position(|p| !matches!(p, TPattern::Wildcard))
        else {
            return self.leaf(&rows[0]);
        };
        let (var, ty) = cols[col].clone();
        let head = rows[0].pats[col].clone();
        match head {
            TPattern::Tuple(ps) => {
                // The one constructor of a tuple: every row splits into the element columns, read
                // out of the tuple's fields once here.
                let field_tys: Vec<ConcreteType> = self
                    .mono
                    .tuple_field_tys(&ty.to_type())
                    .map(|ts| ts.into_iter().map(Self::concrete).collect())
                    .unwrap_or_else(|| vec![ConcreteType::Primitive(Primitive::I64); ps.len()]);
                let sub_cols: Columns = field_tys.into_iter().map(|t| self.column(t)).collect();
                let arity = sub_cols.len();
                let spec: Vec<Row> = rows
                    .iter()
                    .filter_map(|row| {
                        let subs = match &row.pats[col] {
                            TPattern::Tuple(qs) if qs.len() == arity => qs.clone(),
                            TPattern::Wildcard => vec![TPattern::Wildcard; arity],
                            _ => return None,
                        };
                        Some(replace_col(row, col, subs))
                    })
                    .collect();
                let new_cols = splice_cols(cols, col, &sub_cols);
                let mut out = self.compile(spec, &new_cols);
                for (i, (sub_var, sub_ty)) in sub_cols.iter().enumerate().rev() {
                    let field = TExpr::new(
                        TExprKind::Field(Box::new(self.ident(&var, &ty)), format!("_{i}")),
                        sub_ty.clone(),
                        self.span.clone(),
                    );
                    out = TExpr::new(
                        TExprKind::Let(sub_var.clone(), Box::new(field), Box::new(out.clone())),
                        out.ty,
                        self.span.clone(),
                    );
                }
                out
            }
            TPattern::Ctor { .. } | TPattern::Nested { .. } => {
                // A tagged column: one `match` arm per case of the type, each binding its payload
                // to fresh columns.
                let cases = self.mono.other_cases(&ty.to_type(), "");
                let mut arms = Vec::with_capacity(cases.len());
                for (case, payload) in cases {
                    let sub_cols: Columns = payload
                        .into_iter()
                        .map(|t| self.column(Self::concrete(t)))
                        .collect();
                    let arity = sub_cols.len();
                    let spec: Vec<Row> = rows
                        .iter()
                        .filter_map(|row| {
                            let subs = match &row.pats[col] {
                                TPattern::Ctor {
                                    name,
                                    bindings,
                                    ignore_payload,
                                } if bare(name) == case => {
                                    if *ignore_payload {
                                        vec![TPattern::Wildcard; arity]
                                    } else {
                                        bindings
                                            .iter()
                                            .map(|b| {
                                                if b == "_" {
                                                    TPattern::Wildcard
                                                } else {
                                                    TPattern::Bind(b.clone())
                                                }
                                            })
                                            .collect()
                                    }
                                }
                                TPattern::Nested { name, args } if bare(name) == case => {
                                    args.clone()
                                }
                                TPattern::Wildcard => vec![TPattern::Wildcard; arity],
                                _ => return None,
                            };
                            Some(replace_col(row, col, subs))
                        })
                        .collect();
                    if spec.is_empty() {
                        // No row reaches this case. The checker proved that cannot happen for a
                        // value (only a refinement can leave a case unmatched, and erasure has
                        // made it a plain variant again), so it gets no arm.
                        continue;
                    }
                    let new_cols = splice_cols(cols, col, &sub_cols);
                    let body = self.compile(spec, &new_cols);
                    arms.push(TMatchArm {
                        pattern: TPattern::Ctor {
                            name: case,
                            bindings: sub_cols.iter().map(|(v, _)| v.clone()).collect(),
                            ignore_payload: false,
                        },
                        body,
                        span: self.span.clone(),
                    });
                }
                self.node(TExprKind::Match(Box::new(self.ident(&var, &ty)), arms))
            }
            TPattern::Int(_) | TPattern::Str(_) | TPattern::Char(_) => {
                // A literal column: test each distinct literal in row order, falling through to
                // the rows that accept anything.
                let mut lits: Vec<Pattern> = Vec::new();
                for row in &rows {
                    let p = &row.pats[col];
                    if !matches!(p, TPattern::Wildcard) && !lits.contains(p) {
                        lits.push(p.clone());
                    }
                }
                let default: Vec<Row> = rows
                    .iter()
                    .filter(|row| matches!(row.pats[col], TPattern::Wildcard))
                    .map(|row| drop_col(row, col))
                    .collect();
                let rest_cols = splice_cols(cols, col, &[]);
                let mut out = self.compile(default, &rest_cols);
                for lit in lits.into_iter().rev() {
                    let spec: Vec<Row> = rows
                        .iter()
                        .filter(|row| {
                            row.pats[col] == lit || matches!(row.pats[col], TPattern::Wildcard)
                        })
                        .map(|row| drop_col(row, col))
                        .collect();
                    let then = self.compile(spec, &rest_cols);
                    let value = match &lit {
                        TPattern::Int(n) => TExprKind::Int(*n),
                        TPattern::Str(s) => TExprKind::Str(s.clone()),
                        TPattern::Char(c) => TExprKind::Char(*c),
                        _ => unreachable!("only literals are collected"),
                    };
                    let test = TExpr::new(
                        TExprKind::Call {
                            callee: Callee::Equal,
                            type_args: Vec::new(),
                            args: vec![
                                self.ident(&var, &ty),
                                TExpr::new(value, ty.clone(), self.span.clone()),
                            ],
                            method_style: false,
                        },
                        ConcreteType::Primitive(Primitive::Bool),
                        self.span.clone(),
                    );
                    out = self.node(TExprKind::If(Box::new(test), Box::new(then), Box::new(out)));
                }
                out
            }
            TPattern::Array(_) | TPattern::ArrayNested(_) => {
                // Dispatch on length, using the simple array pattern: one arm per distinct length
                // among the rows, then a `_` arm for everything else. Each arm binds its elements
                // to fresh columns and the sub-patterns are compiled against those, so nothing
                // here has to index an array or unwrap the optional an index would yield.
                let elem = match &ty {
                    ConcreteType::Array(e) => (**e).clone(),
                    _ => ConcreteType::Primitive(Primitive::Char), // str, as char[]
                };
                let mut lens: Vec<usize> = Vec::new();
                for row in &rows {
                    if let Some(ps) = array_elems(&row.pats[col]) {
                        if !lens.contains(&ps.len()) {
                            lens.push(ps.len());
                        }
                    }
                }
                let mut arms = Vec::with_capacity(lens.len() + 1);
                for n in lens {
                    let sub_cols: Columns = (0..n).map(|_| self.column(elem.clone())).collect();
                    let spec: Vec<Row> = rows
                        .iter()
                        .filter_map(|row| {
                            let subs = match array_elems(&row.pats[col]) {
                                Some(qs) if qs.len() == n => qs,
                                Some(_) => return None,
                                None => match &row.pats[col] {
                                    TPattern::Wildcard => vec![TPattern::Wildcard; n],
                                    _ => return None,
                                },
                            };
                            Some(replace_col(row, col, subs))
                        })
                        .collect();
                    let new_cols = splice_cols(cols, col, &sub_cols);
                    let body = self.compile(spec, &new_cols);
                    arms.push(TMatchArm {
                        pattern: TPattern::Array(
                            sub_cols.iter().map(|(v, t)| self.ident(v, t)).collect(),
                        ),
                        body,
                        span: self.span.clone(),
                    });
                }
                // Every other length, and every row that accepts any.
                let default: Vec<Row> = rows
                    .iter()
                    .filter(|row| matches!(row.pats[col], TPattern::Wildcard))
                    .map(|row| drop_col(row, col))
                    .collect();
                let rest_cols = splice_cols(cols, col, &[]);
                let body = if default.is_empty() {
                    self.leaf(&rows[0])
                } else {
                    self.compile(default, &rest_cols)
                };
                arms.push(TMatchArm {
                    pattern: TPattern::Wildcard,
                    body,
                    span: self.span.clone(),
                });
                self.node(TExprKind::Match(Box::new(self.ident(&var, &ty)), arms))
            }
            TPattern::ArrayRest(_) => {
                unreachable!("the checker refuses a `..` array in a nested match")
            }
            TPattern::Wildcard | TPattern::Bind(_) => unreachable!("absorbed above"),
        }
    }

    /// The arm `row` reaches: its body under the binders the row collected.
    fn leaf(&mut self, row: &Row) -> Typed {
        let mut out =
            crate::typed_util::distinct_binders(&self.bodies[row.arm], &mut self.mono.names);
        for (name, var) in row.binds.iter().rev() {
            let value = self.ident(var, &self.known[var]);
            let body_ty = out.ty.clone();
            out = TExpr::new(
                TExprKind::Let(name.clone(), Box::new(value), Box::new(out)),
                body_ty,
                self.span.clone(),
            );
        }
        out
    }
}

/// The names a pattern binds, nested ones included.
fn binders(pattern: &Pattern, out: &mut Vec<String>) {
    match pattern {
        TPattern::Bind(n) => out.push(n.clone()),
        TPattern::Ctor {
            bindings,
            ignore_payload: false,
            ..
        } => out.extend(bindings.iter().filter(|b| *b != "_").cloned()),
        TPattern::Nested { args, .. } => args.iter().for_each(|p| binders(p, out)),
        TPattern::Tuple(ps) | TPattern::ArrayNested(ps) => ps.iter().for_each(|p| binders(p, out)),
        TPattern::Array(_) | TPattern::ArrayRest(_) => {
            if let Some(elems) = array_elems(pattern) {
                elems.iter().for_each(|p| binders(p, out));
            }
        }
        TPattern::Ctor { .. }
        | TPattern::Str(_)
        | TPattern::Char(_)
        | TPattern::Int(_)
        | TPattern::Wildcard => {}
    }
}

impl Mono<'_> {
    /// Whether a `match` is compiled to a decision tree: an arm uses a nested pattern, or the
    /// scrutinee is a tuple literal every arm takes apart.
    pub(crate) fn is_nested_match(
        scrutinee: &TExpr<ConcreteType>,
        arms: &[TMatchArm<ConcreteType>],
    ) -> bool {
        arms.iter().any(|a| is_nested(&a.pattern))
            || matches!(&scrutinee.kind, TExprKind::TupleLit(es) if matches_elementwise(arms, es.len()))
    }

    /// A `match` (or an `if let`, as the two-arm `match` it means) with nested patterns, as the
    /// decision tree of simple forms it stands for. The arm bodies are lowered in the scope their
    /// binders give them; the tree around them is made of nodes that bind nothing the program
    /// could move out of, so it is not lowered again.
    pub(crate) fn lower_nested_match(
        &mut self,
        scrutinee: TExpr<ConcreteType>,
        mut arms: Vec<TMatchArm<ConcreteType>>,
        ty: ConcreteType,
        span: Span,
    ) -> TExpr<ConcreteType> {
        let elementwise = matches!(&scrutinee.kind, TExprKind::TupleLit(es) if matches_elementwise(&arms, es.len()));
        let mut scrutinee = scrutinee;
        let parts: Option<Vec<Typed>> = if elementwise {
            let TExprKind::TupleLit(elems) =
                std::mem::replace(&mut scrutinee.kind, TExprKind::Unit)
            else {
                unreachable!("checked above");
            };
            let mut elems = elems;
            for e in elems.iter_mut() {
                self.lower_in_place(e);
            }
            Some(elems)
        } else {
            self.lower_in_place(&mut scrutinee);
            None
        };
        for arm in arms.iter_mut() {
            let depth = self.scope_names.len();
            let mut names = Vec::new();
            binders(&arm.pattern, &mut names);
            self.scope_names.extend(names);
            self.lower_in_place(&mut arm.body);
            self.scope_names.truncate(depth);
        }
        let bodies: Vec<Typed> = arms.iter().map(|a| a.body.clone()).collect();
        let mut tree = Tree {
            mono: self,
            bodies: &bodies,
            span: span.clone(),
            result: ty,
            known: HashMap::new(),
        };
        let (cols, rows, prelude): (Columns, Vec<Row>, Vec<(String, Typed)>) = match parts {
            Some(parts) => {
                let cols: Columns = parts.iter().map(|p| tree.column(p.ty.clone())).collect();
                let rows = arms
                    .iter()
                    .enumerate()
                    .map(|(i, a)| Row {
                        pats: match &a.pattern {
                            TPattern::Tuple(ps) => ps.clone(),
                            _ => vec![TPattern::Wildcard; cols.len()],
                        },
                        arm: i,
                        binds: Vec::new(),
                    })
                    .collect();
                let prelude = cols
                    .iter()
                    .zip(parts)
                    .map(|((var, _), value)| (var.clone(), value))
                    .collect();
                (cols, rows, prelude)
            }
            None => {
                let column = tree.column(scrutinee.ty.clone());
                let rows = arms
                    .iter()
                    .enumerate()
                    .map(|(i, a)| Row {
                        pats: vec![a.pattern.clone()],
                        arm: i,
                        binds: Vec::new(),
                    })
                    .collect();
                let var = column.0.clone();
                (vec![column], rows, vec![(var, scrutinee)])
            }
        };
        let mut out = tree.compile(rows, &cols);
        for (var, value) in prelude.into_iter().rev() {
            let body_ty = out.ty.clone();
            out = TExpr::new(
                TExprKind::Let(var, Box::new(value), Box::new(out)),
                body_ty,
                span.clone(),
            );
        }
        out
    }
}
