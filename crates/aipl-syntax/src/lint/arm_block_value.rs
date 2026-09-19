use crate::ast::{Expr, ExprKind};
use crate::Error;

/// `A => { expr }` — a match arm whose block holds nothing but its value. The
/// braces add a level of nesting and say nothing: `A => expr` is the same arm.
///
/// A block is transparent in the AST — `{ expr }` lowers to `expr` — so which
/// spelling was written is recovered from the source around the body: a `{`
/// between the arm's `=>` and where the body starts, and a `}` right after it
/// ends. Only a body that *is* its value qualifies: a block with a statement
/// in it lowers to a `Let`/`Seq`/… chain, and those need their braces. A block
/// holding a comment is left alone too — the comment has no place to go once
/// the braces are gone.
///
/// The hit spans the closing brace, which is the one line that carries an
/// `#[allow]` whatever the layout: an arm on one line ends there, and a broken
/// block's `}` sits on its own line. A marker after the opening `{`, or after
/// the value's first line, is moved by `aipl fmt` to where it squelches
/// nothing.
pub(super) fn arm_block_value(e: &Expr, src: &str, hits: &mut Vec<Error>) {
    let ExprKind::Match(_, arms) = &e.kind else {
        return;
    };
    for arm in arms {
        // An alternation `A | B => body` is one arm per pattern here, sharing
        // the body; only the last pattern is followed by the `=>`, so the
        // shared block is reported once.
        let Some(after_arrow) = src
            .get(arm.span.end..)
            .map(str::trim_start)
            .and_then(|s| s.strip_prefix("=>"))
        else {
            continue;
        };
        let body_text = after_arrow.trim_start();
        if !body_text.starts_with('{') {
            continue; // already the bare form
        }
        let open = src.len() - body_text.len();
        // A chain from a statement is a block that needs its braces.
        if matches!(
            arm.body.kind,
            ExprKind::Let(..)
                | ExprKind::LetMut(..)
                | ExprKind::Assign(..)
                | ExprKind::Seq(..)
                | ExprKind::For(..)
                | ExprKind::While(..)
                | ExprKind::Return(..)
        ) {
            continue;
        }
        let (start, end) = (arm.body.span.start, arm.body.span.end);
        // Anything but blanks between the braces and the value is a comment.
        let Some(lead) = src.get(open + 1..start) else {
            continue;
        };
        if !lead.trim().is_empty() {
            continue;
        }
        let Some(tail) = src.get(end..) else {
            continue;
        };
        let after_value = tail.trim_start();
        if !after_value.starts_with('}') {
            continue;
        }
        let close = src.len() - after_value.len();
        hits.push(Error::at(
            "this arm's block holds nothing but its value — write the value as the arm's body, \
             without the braces (or append #[allow] to this line to keep it)"
                .to_string(),
            close..close + 1,
        ));
    }
}
