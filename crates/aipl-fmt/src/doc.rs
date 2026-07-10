//! A Wadler/Prettier-style pretty-printing document algebra: the formatter
//! builds a [`Doc`] tree describing *possible* layouts, and [`print`] picks
//! one — each [`Doc::Group`] renders on one line ("flat") when it fits within
//! the width limit, and breaks at its `Line`/`SoftLine` points otherwise.

/// Number of spaces per indentation level.
pub const INDENT: usize = 4;

#[derive(Debug, Clone)]
pub enum Doc {
    /// Literal text. May contain newlines (a verbatim multi-line atom like a
    /// raw string or template literal): such text is emitted exactly as-is —
    /// never re-indented, since its layout is part of its value — and forces
    /// every enclosing group to break.
    Text(String),
    Concat(Vec<Doc>),
    /// A space when flat, a newline when broken.
    Line,
    /// Nothing when flat, a newline when broken.
    SoftLine,
    /// Always a newline.
    HardLine,
    /// Always a blank line (two newlines) — the separator between items.
    BlankLine,
    /// One more indentation level for any line breaks inside.
    Indent(Box<Doc>),
    /// Render flat if the content fits in the remaining width, else broken.
    /// `forced` (computed by [`group`]) marks content that can never be flat —
    /// it contains a hard break or multi-line text.
    Group {
        forced: bool,
        inner: Box<Doc>,
    },
    /// Text emitted only when the enclosing group is broken — the trailing
    /// comma of a block-indented list.
    IfBroken(String),
    /// Zero-width, but forces every enclosing group to break. Attached after
    /// a trailing `// comment`: the comment must be the last thing on its
    /// line, so nothing may be flattened onto the line after it — the
    /// *existing* separators then supply the actual newline.
    BreakParent,
}

pub fn text(s: impl Into<String>) -> Doc {
    Doc::Text(s.into())
}

pub fn concat(docs: Vec<Doc>) -> Doc {
    Doc::Concat(docs)
}

pub fn indent(d: Doc) -> Doc {
    Doc::Indent(Box::new(d))
}

/// Wrap `d` in a group, pre-computing whether it is forced to break (see
/// [`Doc::Group`]). Forcing is what makes a multi-statement block, or any
/// construct holding a line comment, lay out vertically no matter the width.
pub fn group(d: Doc) -> Doc {
    let forced = has_hard(&d);
    Doc::Group {
        forced,
        inner: Box::new(d),
    }
}

/// Whether `d` contains content that can never render flat: a hard/blank
/// line, or multi-line verbatim text. Recurses through nested groups — a
/// forced inner group means the outer can't be flat either.
fn has_hard(d: &Doc) -> bool {
    match d {
        Doc::Text(s) => s.contains('\n'),
        Doc::Concat(ds) => ds.iter().any(has_hard),
        Doc::Line | Doc::SoftLine | Doc::IfBroken(_) => false,
        Doc::HardLine | Doc::BlankLine | Doc::BreakParent => true,
        Doc::Indent(inner) => has_hard(inner),
        Doc::Group { forced, .. } => *forced,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

/// Render `doc` at the given width limit. Lines are indented in units of
/// [`INDENT`] spaces; trailing whitespace is stripped from every line and the
/// result does not carry a trailing newline (the caller adds the final one).
pub fn print(doc: &Doc, max_width: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    // (indent level, mode, doc) — a work stack, last-in first-out.
    let mut stack: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Break, doc)];
    while let Some((ind, mode, d)) = stack.pop() {
        match d {
            Doc::Text(s) => {
                out.push_str(s);
                col = match s.rfind('\n') {
                    // Multi-line verbatim text: the column restarts after its
                    // last newline.
                    Some(i) => s.len() - i - 1,
                    None => col + s.len(),
                };
            }
            Doc::Concat(ds) => {
                for child in ds.iter().rev() {
                    stack.push((ind, mode, child));
                }
            }
            Doc::Line => match mode {
                Mode::Flat => {
                    out.push(' ');
                    col += 1;
                }
                Mode::Break => col = newline(&mut out, ind),
            },
            Doc::SoftLine => {
                if mode == Mode::Break {
                    col = newline(&mut out, ind);
                }
            }
            Doc::HardLine => col = newline(&mut out, ind),
            Doc::BlankLine => {
                out.push('\n');
                col = newline(&mut out, ind);
            }
            Doc::Indent(inner) => stack.push((ind + 1, mode, inner)),
            Doc::Group { forced, inner } => {
                let flat = !*forced && fits(max_width.saturating_sub(col), inner, &stack);
                stack.push((ind, if flat { Mode::Flat } else { Mode::Break }, inner));
            }
            Doc::IfBroken(s) => {
                if mode == Mode::Break {
                    out.push_str(s);
                    col += s.len();
                }
            }
            // Zero-width; its work happened at group() time (forcing).
            Doc::BreakParent => {}
        }
    }
    // Belt-and-braces: no emitted line may carry trailing whitespace (the
    // language rejects it), and blank separator lines are truly empty.
    let mut cleaned: String = out
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    // `str::lines` drops a trailing newline; none of our docs end with one,
    // so nothing to restore — but preserve a final newline if `out` had one.
    if out.ends_with('\n') {
        cleaned.push('\n');
    }
    cleaned
}

fn newline(out: &mut String, ind: usize) -> usize {
    out.push('\n');
    let spaces = ind * INDENT;
    out.push_str(&" ".repeat(spaces));
    spaces
}

/// Would rendering `candidate` flat — followed by the rest of the current
/// line (`rest`, the outer work stack) — stay within `budget` columns? Walks
/// until the budget is exhausted (no) or the line provably ends (yes).
fn fits(budget: usize, candidate: &Doc, rest: &[(usize, Mode, &Doc)]) -> bool {
    let mut remaining = budget as isize;
    // Work stack seeded with the candidate flat, then the outer stack (whose
    // own modes decide whether the line ends at their next break point).
    let mut stack: Vec<(Mode, &Doc)> = rest.iter().map(|(_, m, d)| (*m, *d)).collect();
    stack.push((Mode::Flat, candidate));
    while let Some((mode, d)) = stack.pop() {
        if remaining < 0 {
            return false;
        }
        match d {
            Doc::Text(s) => {
                if s.contains('\n') {
                    // A multi-line atom can't be part of a flat line.
                    return mode != Mode::Flat;
                }
                remaining -= s.len() as isize;
            }
            Doc::Concat(ds) => {
                for child in ds.iter().rev() {
                    stack.push((mode, child));
                }
            }
            Doc::Line => match mode {
                Mode::Flat => remaining -= 1,
                Mode::Break => return true,
            },
            Doc::SoftLine => {
                if mode == Mode::Break {
                    return true;
                }
            }
            Doc::HardLine | Doc::BlankLine => return true,
            Doc::Indent(inner) => stack.push((mode, inner)),
            Doc::Group { forced, inner } => {
                if *forced {
                    // A forced sub-group inside flat content means the flat
                    // rendering is impossible; in already-broken outer content
                    // it simply ends the line.
                    return mode != Mode::Flat;
                }
                // Inherit the ambient mode: a group nested inside the flat
                // candidate is measured flat, but a group encountered in the
                // *following* content keeps its own (broken) mode, so its next
                // line break ends this fit scan. Forcing every such group flat
                // would wrongly count a long, independently-breaking sibling
                // (e.g. the function body after a signature) against the line.
                stack.push((mode, inner));
            }
            Doc::IfBroken(s) => {
                if mode == Mode::Break {
                    remaining -= s.len() as isize;
                }
            }
            Doc::BreakParent => {
                // Can't be part of a flat line; in already-broken content it
                // is nothing.
                if mode == Mode::Flat {
                    return false;
                }
            }
        }
    }
    remaining >= 0
}
