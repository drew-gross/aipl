//! `[[1, 2], [3, 4]]` where a grid is expected, as the grid it is.
//!
//! The literal is already a valid `i64[][]`, so nothing in its own syntax says
//! which it is: the type it is written *for* does. This pass reads that off the
//! places a type is declared — a `let` or `mut` annotation, a function's
//! parameters and return type, a struct's fields, the declared type of a name
//! being assigned — and rewrites a nested literal found where a `T[,]` is
//! expected into the grid it stands for:
//!
//! ```text
//! let g: i64[,] = [[1, 2], [3, 4]];   →   let g: i64[,] = grid_from(2, [1, 2, 3, 4]);
//! ```
//!
//! Anywhere else the literal stays an array of arrays, so code that never
//! mentions a grid reads exactly as it did. A literal whose rows differ in length
//! is refused here, where the rows are still written out and the offender can be
//! pointed at. It runs before generics are lowered, while a grid type is still
//! the `Grid<T>` application the parser wrote `T[,]` as.

use std::collections::HashMap;

use aipl_syntax::ast::{Callee, Expr, ExprKind, Function, Item, Program, Type};
use aipl_syntax::Error;

use crate::children_mut;

/// What the program declares that an expected type can be read from.
struct Declared<'a> {
    fns: HashMap<&'a str, &'a Function>,
    structs: HashMap<&'a str, HashMap<&'a str, &'a Type>>,
}

/// `program` with each nested literal in a grid position written as a grid.
pub fn lower_grid_literals(program: &Program) -> Result<Program, Error> {
    let mut declared = Declared {
        fns: HashMap::new(),
        structs: HashMap::new(),
    };
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                declared.fns.insert(f.name.as_str(), f);
            }
            Item::Struct(s) if !s.is_generic() => {
                declared.structs.insert(
                    s.name.as_str(),
                    s.fields.iter().map(|f| (f.name.as_str(), &f.ty)).collect(),
                );
            }
            _ => {}
        }
    }
    let mut items = Vec::with_capacity(program.items.len());
    for item in &program.items {
        items.push(match item {
            Item::Fn(f) => {
                let mut f = f.clone();
                let mut env: HashMap<String, Type> = f
                    .sig
                    .params
                    .iter()
                    .map(|p| (p.name.clone(), p.ty.clone()))
                    .collect();
                let ret = f.sig.return_ty.clone();
                lower(&mut f.body, ret.as_ref(), ret.as_ref(), &mut env, &declared)?;
                if let Some(test) = f.test_body.as_mut() {
                    lower(test, None, None, &mut env, &declared)?;
                }
                Item::Fn(f)
            }
            other => other.clone(),
        });
    }
    Ok(Program {
        items,
        sources: program.sources.clone(),
        doc: program.doc.clone(),
    })
}

/// The element type of `ty` if it is a grid.
fn grid_element(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Generic(name, args)
            if args.len() == 1 && aipl_syntax::strip_mangle_prefix(name) == "Grid" =>
        {
            Some(&args[0])
        }
        _ => None,
    }
}

/// Rewrite `e` in place. `expected` is the type its value is declared to have,
/// if anything declares one; `ret` is the enclosing function's return type, which
/// a `return` is held to.
fn lower(
    e: &mut Expr,
    expected: Option<&Type>,
    ret: Option<&Type>,
    env: &mut HashMap<String, Type>,
    declared: &Declared,
) -> Result<(), Error> {
    if let (Some(element), ExprKind::ArrayLit(rows)) =
        (expected.and_then(grid_element), &mut e.kind)
    {
        if is_nested_literal(rows) {
            return rewrite_rows(e, element, ret, env, declared);
        }
    }
    match &mut e.kind {
        ExprKind::Let(name, ty, value, body) | ExprKind::LetMut(name, ty, value, body) => {
            lower(value, ty.as_ref(), ret, env, declared)?;
            match ty {
                Some(ty) => {
                    env.insert(name.clone(), ty.clone());
                }
                None => {
                    env.remove(name);
                }
            }
            lower(body, expected, ret, env, declared)
        }
        ExprKind::Assign(target, value, body) => {
            let target_ty = match &target.kind {
                ExprKind::Ident(name) => env.get(name).cloned(),
                _ => None,
            };
            lower(value, target_ty.as_ref(), ret, env, declared)?;
            lower(body, expected, ret, env, declared)
        }
        ExprKind::Seq(first, rest) => {
            lower(first, None, ret, env, declared)?;
            lower(rest, expected, ret, env, declared)
        }
        ExprKind::If(cond, then, otherwise) => {
            lower(cond, None, ret, env, declared)?;
            lower(then, expected, ret, env, declared)?;
            lower(otherwise, expected, ret, env, declared)
        }
        ExprKind::Match(scrutinee, arms) => {
            lower(scrutinee, None, ret, env, declared)?;
            for arm in arms {
                lower(&mut arm.body, expected, ret, env, declared)?;
            }
            Ok(())
        }
        ExprKind::IfLet(arm, scrutinee, otherwise) => {
            lower(scrutinee, None, ret, env, declared)?;
            lower(&mut arm.body, expected, ret, env, declared)?;
            lower(otherwise, expected, ret, env, declared)
        }
        ExprKind::Return(value) => lower(value, ret, ret, env, declared),
        ExprKind::Call(Callee::User(name), args, _) => {
            let params = declared.fns.get(name.as_str()).map(|f| &f.sig.params);
            for (i, arg) in args.iter_mut().enumerate() {
                let param_ty = params.and_then(|ps| ps.get(i)).map(|p| &p.ty);
                lower(arg, param_ty, ret, env, declared)?;
            }
            Ok(())
        }
        ExprKind::Construct(name, inits) => {
            let fields = declared.structs.get(name.as_str());
            for init in inits {
                let field_ty = fields.and_then(|fs| fs.get(init.name.as_str())).copied();
                lower(&mut init.value, field_ty, ret, env, declared)?;
            }
            Ok(())
        }
        _ => {
            for child in children_mut(e) {
                lower(child, None, ret, env, declared)?;
            }
            Ok(())
        }
    }
}

/// Whether `rows` are all non-empty array literals, which is what a grid literal
/// is written as.
fn is_nested_literal(rows: &[Expr]) -> bool {
    !rows.is_empty()
        && rows
            .iter()
            .all(|r| matches!(&r.kind, ExprKind::ArrayLit(cells) if !cells.is_empty()))
}

/// `e`, a literal of rows, as `grid_from(width, [every cell, row by row])`.
fn rewrite_rows(
    e: &mut Expr,
    element: &Type,
    ret: Option<&Type>,
    env: &mut HashMap<String, Type>,
    declared: &Declared,
) -> Result<(), Error> {
    let ExprKind::ArrayLit(rows) = &mut e.kind else {
        unreachable!("rewrite_rows is called on an array literal");
    };
    let width = match &rows[0].kind {
        ExprKind::ArrayLit(cells) => cells.len(),
        _ => unreachable!("is_nested_literal checked every row"),
    };
    let mut cells = Vec::new();
    for row in rows.iter_mut() {
        let ExprKind::ArrayLit(row_cells) = &mut row.kind else {
            unreachable!("is_nested_literal checked every row");
        };
        if row_cells.len() != width {
            return Err(Error::at(
                format!(
                    "every row of a grid must have the same length: this row has {}, the first has {width}",
                    row_cells.len()
                ),
                row.span.clone(),
            ));
        }
        for mut cell in std::mem::take(row_cells) {
            lower(&mut cell, Some(element), ret, env, declared)?;
            cells.push(cell);
        }
    }
    let span = e.span.clone();
    let width = Expr::new(ExprKind::Num(width as i64), span.clone());
    let cells = Expr::new(ExprKind::ArrayLit(cells), span.clone());
    e.kind = ExprKind::Call(Callee::GridFrom, vec![width, cells], false);
    Ok(())
}
