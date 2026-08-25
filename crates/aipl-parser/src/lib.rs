//! AIPL lexer and parser: the gazelle grammar, the tokenizer, and the
//! human-friendly rendering of syntax errors. Produces an [`aipl_syntax::ast`]
//! tree from source text.

use std::path::Path;

use gazelle::Precedence;
use gazelle_macros::gazelle;

use aipl_syntax::{join_spans, Error, Span};

use aipl_syntax::ast::{
    Bound, Expr, ExprKind, FieldDecl, FieldInit, Function, ImportDecl, ImportName, ImportSource,
    Item, LambdaParam, MatchArm, Param, Pattern, Primitive, Program, Signature, StructDecl, Type,
    TypeParam, VariantCase, VariantDecl,
};

gazelle! {
    grammar aipl {
        start program;
        terminals {
            // Identifiers and literals carry source spans alongside their values.
            IDENT: _,
            NUM: _,
            STR: _,
            CHAR: _,
            TRUE: _,
            FALSE: _,
            // Keywords (no value)
            FN, IF, ELSE, STRUCT, VARIANT, IMPORT, FROM, AS, PUB, LET, FOR, WHILE, MUT, SET, MATCH,
            RETURN,
            // `shim` carries a span so a shim diagnostic can underline the
            // construct (its block body may be empty and span nothing).
            SHIM: _,
            // `builtins` keyword for `from builtins` imports; carries a
            // span so the loader can point errors at it.
            BUILTINS: _,
            // `none` carries a span so we can point at it on type errors.
            NONE: _,
            // Punctuation
            LPAREN, RPAREN,
            LBRACE, RBRACE,
            // `[` carries a span (see lexer) so empty array literals can
            // still be located in diagnostics; `]` carries nothing.
            LBRACKET: _,
            RBRACKET,
            // `#` leads a set literal `#{ .. }`; carries a span so an empty
            // `#{}` still has a location for diagnostics (like `[`).
            HASH: _,
            COMMA, COLON, ARROW, DOT, SEMI, EQ, QUESTION, FATARROW,
            BANG,
            // `++` — the increment statement `set n++;`. Carries a span so the
            // `+ 1` it desugars to (and the `+` import error it can raise) point
            // at the operator.
            PLUSPLUS: _,
            // `|` — surrounds a lambda's parameter list (`|x| body`). Carries a
            // span for the lambda's start. (`||` is logical-or, lexed separately.)
            PIPE: _,
            // Template literals: `` `text {expr} text` ``
            // HEAD carries the text from `` ` `` to the first `{`.
            // MIDDLE carries the text between a `}` and the next `{`.
            // TAIL carries the text from the last `}` to the closing `` ` ``.
            TEMPLATE_HEAD: _,
            TEMPLATE_MIDDLE: _,
            TEMPLATE_TAIL: _,
            // Operators with runtime precedence
            prec MINUS,
            prec OP: _,
            // `<` / `>` serve double duty: comparison operators (with
            // precedence) and the brackets around generic type params
            // (`fn f<T: any>`). The grammar position disambiguates.
            prec LANGLE,
            prec RANGLE,
            // `||` serves double duty too: infix logical-or inside an
            // expression, and the lead of a no-argument lambda (`|| body`) in
            // argument position, where infix-or is impossible. Position
            // disambiguates — exactly like `<`.
            prec OROR,
            // `..` — the range expression `start..end` (a `Span` constructor).
            // Binds tighter than `==`/`!=` (so `span == 1..2` compares against
            // the whole range) but looser than comparison and arithmetic (so
            // `a + 1..b * 2` is `(a + 1)..(b * 2)`) — see `op_precedence`. Also
            // appears literally in the open-ended slice postfixes (`[a..]`,
            // `[..b]`), like `MINUS` appears literally in unary position.
            prec DOTDOT
        }

        program = item* => program;

        item = function => function | struct_decl => struct_decl
             | variant_decl => variant_decl | import_decl => import_decl;

        import_decl = IMPORT LBRACE import_names RBRACE FROM STR SEMI => import
                    | IMPORT LBRACE import_names RBRACE FROM BUILTINS SEMI => import_builtins;
        // A name, an aliased name, or an operator spelling (operators must be
        // imported from builtins to be used — see the loader's gating).
        import_name = IDENT => plain
                    | IDENT AS IDENT => aliased
                    // A named builtin aliased to an operator: `wrapping_add as +`.
                    | IDENT AS OP => aliased_op
                    | IDENT AS MINUS => aliased_minus
                    | IDENT AS LANGLE => aliased_lt
                    | IDENT AS RANGLE => aliased_gt
                    | IDENT AS OROR => aliased_or
                    | IDENT AS BANG => aliased_bang
                    // `wrapping_increment as ++` — `++` has no bare form either,
                    // so like `+` it is only ever reached through an alias.
                    | IDENT AS PLUSPLUS => aliased_plusplus
                    | OP => op
                    | MINUS => op_minus
                    | LANGLE => op_lt
                    | RANGLE => op_gt
                    | OROR => op_or
                    | BANG => op_bang
                    | PLUSPLUS => op_plusplus;
        import_names = import_name_list => present
                     | import_name_list COMMA => present_trailing
                     | _ => empty;
        import_name_list = import_name => first | import_name_list COMMA import_name => rest;

        // An optional `pub` marks the function importable by other files; its
        // absence makes it file-private (importing it is a loader error).
        vis = PUB => public | _ => private;
        function = vis FN IDENT type_params LPAREN params RPAREN effects return_ty fn_body fn_attrs => function;
        // A function body is an ordinary value block, or the *struct-literal
        // shorthand*: `fn leaf(v: i64) -> Node { v, next: none }` instead of the
        // stuttering `-> Node { Node { v, next: none } }`. The declared return
        // type supplies the name, so the sugar is desugared to the same
        // `Construct` the long form builds (see the `Function` build action).
        //
        // `{ x }` stays a **block** returning `x`: it is the one shape both
        // readings accept, and a block is what it has always meant. So the
        // shorthand needs either an explicit `field: value` or a comma —
        // a single shorthand field is written `{ x, }`. That is also what keeps
        // the two apart for the parser: after `LBRACE IDENT`, one token of
        // lookahead decides (`:`/`,` → shorthand, `}` or anything else → block),
        // and neither `IDENT :` nor `IDENT ,` can start a statement.
        fn_body = block => block
                | LBRACE construct_fields RBRACE => construct;
        // The shorthand's field list. Spelled out from its leading `IDENT`
        // rather than reusing `field_init` so the span starts at the first
        // field *name* — that span is what the "needs a struct return type"
        // error underlines. After `IDENT COLON expr`, one token of lookahead
        // (`,` → `many`, `}` → `one`) picks the production.
        construct_fields = IDENT COLON expr => one
                         | IDENT COLON expr COMMA field_inits => many
                         | IDENT COMMA field_inits => many_shorthand
                         // `{ ..base, field: value }` — the shorthand's spread
                         // form. Unambiguous after `LBRACE`: no statement or
                         // expression starts with `..`, so the token picks this
                         // production outright. At least one field must follow,
                         // which is also the existing rule that a bare
                         // `T { ..x }` is just `x`.
                         | DOTDOT expr COMMA field_inits => spread_first;
        // Zero or more attributes attached to a function, in any order:
        // `.test({ .. })` (a test block the `check` command runs) and
        // `.doc("...")` (documentation surfaced by the `doc` command). The two
        // forms are distinguished by their argument — a `block` (`{ .. }`) vs a
        // `STR` — and the IDENT (`test`/`doc`) is validated in the build action.
        // `FOLLOW(function)` is item-leading keywords or EOF, none of which is
        // `.`, so the (repeatable) suffix is unambiguous.
        fn_attrs = fn_attr_list => present | _ => empty;
        fn_attr_list = fn_attr => first | fn_attr_list fn_attr => rest;
        fn_attr = DOT IDENT LPAREN block RPAREN => test
                | DOT IDENT LPAREN STR RPAREN => doc;
        // Optional `<T, U: ord>` generic parameter list. A bound is optional —
        // a bare `T` defaults to the `any` bound; `T: ord` narrows it. After an
        // `IDENT`, one token of lookahead (`:` → bounded, else → bare) picks the
        // production.
        type_params = LANGLE type_param_list RANGLE => present | _ => empty;
        type_param_list = type_param => first | type_param_list COMMA type_param => rest;
        // `T: variant` spells its bound with the `variant` *keyword*, which
        // does not lex as IDENT — hence a production of its own rather than
        // another name handled in the build action.
        type_param = IDENT COLON IDENT => type_param
                   | IDENT COLON VARIANT => variant_bound
                   | IDENT => bare;

        effects = effect_list => present | _ => empty;
        effect_list = effect => first | effect_list effect => rest;
        effect = BANG IDENT => effect;

        // An optional `<T: any, ..>` makes the struct generic (a template
        // monomorphized per concrete use); shares `type_params` with functions.
        struct_decl = STRUCT IDENT type_params LBRACE fields RBRACE => struct_decl;

        // `variant Shape = Circle(i64) | Rect(i64, i64) | Empty` — a sum type.
        // Cases are `|`-separated; each is a bare name (nullary) or a name with
        // a parenthesized positional payload. No terminator: the next item's
        // leading keyword ends the case list (only `|` continues it). An
        // optional `<T: any, ..>` makes it a generic variant template.
        //
        // A **leading** `|` before the first case is allowed and means nothing
        // (`variant V = | A | B` is `variant V = A | B`), so a broken-across-
        // lines declaration can align every case under one `|` — which is what
        // `aipl fmt` emits when the cases don't fit on one line. Unambiguous
        // after `=`: nothing else in the language starts with `|`.
        variant_decl = VARIANT IDENT type_params EQ variant_cases => variant_decl;
        variant_cases = variant_case => first
                      | PIPE variant_case => first_leading_pipe
                      | variant_cases PIPE variant_case => rest;
        variant_case = IDENT => nullary
                     | IDENT LPAREN ty_arg_list RPAREN => with_payload;

        fields = field_decl_list => present
               | field_decl_list COMMA => present_trailing
               | _ => empty;
        field_decl_list = field_decl => first | field_decl_list COMMA field_decl => rest;
        field_decl = IDENT COLON ty => field_decl
                   | IDENT COLON ty EQ expr => with_default;

        params = param_list => present
               | param_list COMMA => present_trailing
               | _ => empty;
        param_list = param => first | param_list COMMA param => rest;

        // `mut self: T` marks a mutating receiver (see codegen): the function
        // mutates its first parameter and returns nothing.
        // `x: T*` marks a variadic ("zero or more") parameter — the trailing
        // `OP` must be `*` (validated in the build action). The only operator
        // that can follow a complete `ty` in parameter position is this marker,
        // so there's no conflict with expression operators.
        // `k: T = expr` declares a *keyword* parameter (the default is what
        // makes it one — see `ast::Param::default`). After a complete `ty`,
        // one token of lookahead (`EQ` vs `OP` vs FOLLOW(param)) picks the
        // production, so the three forms never conflict.
        param = IDENT COLON ty => param
              | MUT IDENT COLON ty => mut_param
              | IDENT COLON ty OP => variadic_param
              | IDENT COLON ty EQ expr => with_default
              | IDENT COLON ty OP EQ expr => variadic_with_default;

        return_ty = ARROW ty => present | _ => absent;

        // Recursive so types nest arbitrarily: `str[]?`, `str[][]`,
        // `i64?[]`, etc. Left-recursive postfix `?`/`[]` (LR handles it).
        // A function type `(A, B) -> R` (a lambda-parameter type) or a base
        // type. Postfix `?`/`[]` apply only to base types — a function type
        // can't be made optional or arrayed — so they live on `base_ty`, which
        // keeps the two from conflicting. The return `ty` is recursive, so
        // `(A) -> (B) -> C` curries (right-associative).
        ty = base_ty => base
           // `T!E` — a result type. `!` is unused in type position (it's
           // `!expr`/`!=`/`!effect` only elsewhere), and both operands are a
           // `base_ty`, so there's no chaining/recursion ambiguity: after a
           // `base_ty`, one token of lookahead (`!` vs FOLLOW(ty)) decides.
           | base_ty BANG base_ty => result
           // `!E` — a "void with result" type: a result whose Ok side is unit
           // (success carries no value). A leading `!` only starts this in type
           // position, so it doesn't conflict with the `base_ty BANG` form.
           | BANG base_ty => result_void
           | LPAREN ty_args RPAREN ARROW ty => fn_ty
           // `(A, B)` — a tuple type (2+ elements). Validated in the build
           // action. With `ARROW` as lookahead the parser shifts instead of
           // reducing here, so `(A, B) -> R` still parses as fn_ty.
           // With `LBRACKET` as lookahead, shifts to `tuple_array_ty`.
           | LPAREN ty_args RPAREN => tuple_ty
           // `(A, B)[]` — an array of tuples. Shifts over `tuple_ty` when
           // the lookahead is `LBRACKET`.
           | LPAREN ty_args RPAREN LBRACKET RBRACKET => tuple_array_ty
           // `(A, B)!E` — a result whose Ok payload is a tuple. A tuple type
           // isn't a `base_ty` (it can't take postfix `?`/`[]` the same way),
           // so the `base_ty BANG base_ty` form above can't reach it; with
           // `BANG` as lookahead the parser shifts here instead of reducing
           // `tuple_ty`.
           | LPAREN ty_args RPAREN BANG base_ty => tuple_result;
        base_ty = IDENT => named
                // `Foo<A, B>` — a use of a generic struct/variant with concrete
                // type arguments. Only in type position (no expressions here),
                // so the `<`/`>` are unambiguously generic brackets, not
                // comparison operators. After an `IDENT`, one token of lookahead
                // (`<` → generic, else → plain `named`) picks the production.
                | IDENT LANGLE ty_arg_list RANGLE => generic
                | base_ty QUESTION => optional
                | base_ty LBRACKET RBRACKET => array
                // `#{T}` — a set type. The leading `#` (same sigil as the
                // `#{..}` set literal) keeps it from colliding with a brace
                // block (e.g. a function body after `-> #{i64}`).
                | HASH LBRACE ty RBRACE => set
                // `#{K: V}` — a dict type. After `HASH LBRACE ty`, a single
                // token of lookahead (`RBRACE` → set, `COLON` → dict) picks the
                // production, so the two never conflict.
                | HASH LBRACE ty COLON ty RBRACE => dict;
        // Argument types of a function type. Empty for `() -> R`.
        ty_args = ty_arg_list => present | _ => empty;
        ty_arg_list = ty => first | ty_arg_list COMMA ty => rest;

        // A value block: a run of statements followed by an optional trailing
        // expression (its value; absent → unit). Written right-recursively so
        // `expr` appears *exactly once* in the block grammar (`block_body`):
        // the parser parses one expression and only then — via the tiny
        // `block_tail` — decides whether a `;` made it a discarded statement or
        // its absence made it the block's value. Having `expr` reachable from
        // two productions (a trailing value *and* a separate `expr;` statement)
        // is what made gazelle's LR tables explode.
        // `op = f, op2 = g,` — a shim's operation bindings (trailing comma ok).
        shim_bindings = shim_binding_list => present | _ => empty;
        shim_binding_list = shim_binding => first
                          | shim_binding_list COMMA shim_binding => rest
                          | shim_binding_list COMMA => trailing;
        shim_binding = IDENT EQ IDENT => shim_binding;
        block = LBRACE block_body RBRACE => block;
        block_body = _ => empty
                   | expr block_tail => head_expr
                   | kw_stmt block_body => head_stmt;
        // What follows a leading expression in a block: nothing (the expr is
        // the block's value) or `; rest` (discard the expr, continue).
        block_tail = _ => value
                   | SEMI block_body => discard;
        // A loop body is statements only — a bare trailing expression is a
        // parse error (its value could never be observed) — so every
        // expression in it must be an `expr;` statement.
        loop_body = LBRACE loop_inner RBRACE => loop_body;
        loop_inner = _ => empty
                   | expr SEMI loop_inner => expr_seq
                   | kw_stmt loop_inner => stmt_seq;
        // Statements that don't begin with an expression — keyword- or
        // `for`-led, so their FIRST sets stay disjoint from `expr` and the
        // block/loop choice is a single-token decision.
        kw_stmt = let_stmt => let_stmt
                | let_tuple_stmt => let_tuple_stmt
                | let_struct_stmt => let_struct_stmt
                | mut_stmt => mut_stmt
                | assign_stmt => assign_stmt
                | for_stmt => for_stmt
                | for_tuple_stmt => for_tuple_stmt
                | for_indexed_stmt => for_indexed_stmt
                | while_stmt => while_stmt
                | return_stmt => return_stmt;
        // `let x = e;` and the annotated `let x: T = e;`. After `LET IDENT` the
        // three forms are separated by one token of lookahead — EQ (plain),
        // COLON (annotated), LBRACE (`let_struct_stmt`) — so the annotation
        // adds no LR conflict.
        let_stmt = LET IDENT EQ expr SEMI => let_stmt
                 | LET IDENT COLON ty EQ expr SEMI => let_ty_stmt;
        // `let (a, b, c) = expr;` — tuple destructuring. Reuses `match_bindings`
        // (existing left-recursive ident list) to avoid adding new LR states.
        let_tuple_stmt = LET LPAREN match_bindings RPAREN EQ expr SEMI => let_tuple_stmt;
        // `let Point { x, y } = expr;` — struct destructuring. The IDENT after
        // LET is the struct type name; after LET IDENT the LBRACE lookahead
        // unambiguously distinguishes this from the plain `let_stmt` (EQ).
        let_struct_stmt = LET IDENT LBRACE struct_field_bindings RBRACE EQ expr SEMI => let_struct_stmt;
        struct_field_bindings = struct_field_binding_list => present
                              | struct_field_binding_list COMMA => present_trailing;
        struct_field_binding_list = IDENT => first | struct_field_binding_list COMMA IDENT => rest;
        // `return value;` — early-return. A statement (keyword-led, so its FIRST
        // set stays disjoint from `expr`); control never falls through it.
        return_stmt = RETURN expr SEMI => return_stmt;
        mut_stmt = MUT IDENT EQ expr SEMI => mut_stmt
                 | MUT IDENT COLON ty EQ expr SEMI => mut_ty_stmt;
        // `set n = expr;` stores to a mut binding; `set n++;` is sugar for
        // `set n = n + 1;` (so it desugars to a `+`/`wrapping_add` use and is
        // gated on importing `+` like any other operator).
        assign_stmt = SET IDENT EQ expr SEMI => assign_stmt
                    | SET IDENT PLUSPLUS SEMI => incr_stmt
                    // `set recv.method(args);` — writeback form of a mutating
                    // method call. Desugars to `set recv = recv.method(args)`;
                    // the `DOT` after `SET IDENT` disambiguates from the two
                    // forms above (`EQ`/`PLUSPLUS`). Receiver is a simple variable.
                    | SET IDENT DOT IDENT LPAREN args RPAREN SEMI => set_call_stmt
                    // `set recv.f.g = expr;` — update one (arbitrarily nested)
                    // field of a mut struct binding. After `SET IDENT DOT
                    // IDENT`, the next token picks the form: `LPAREN` is the
                    // method call above; `EQ`/`DOT` reduce into `field_path`.
                    | SET IDENT DOT field_path EQ expr SEMI => set_field_stmt;
        field_path = IDENT => first | field_path DOT IDENT => rest;
        for_stmt = FOR LPAREN LET IDENT COLON expr RPAREN loop_body => for_stmt;
        // `for (let (a, b) : expr) { ... }` — destructuring for loop. Desugars to
        // a plain for loop with a synthetic temp var and field-access let bindings
        // prepended to the body; after `FOR LPAREN LET`, the next token (`LPAREN`
        // vs `IDENT`) unambiguously distinguishes this from `for_stmt`.
        for_tuple_stmt = FOR LPAREN LET LPAREN match_bindings RPAREN COLON expr RPAREN loop_body => for_tuple_stmt;
        // `for (let i, x : expr) { ... }` — index-and-item for loop. Desugars to a
        // plain for loop over a counter the block folder introduces; after
        // `FOR LPAREN LET IDENT`, the next token (`COMMA` vs `COLON`)
        // unambiguously distinguishes this from `for_stmt`.
        for_indexed_stmt = FOR LPAREN LET IDENT COMMA IDENT COLON expr RPAREN loop_body => for_indexed_stmt;
        while_stmt = WHILE LPAREN expr RPAREN loop_body => while_stmt;

        // `start..end` — a range expression, constructing the builtin `Span`
        // (`Span { start: .., end: .. }`). Its own production (not a `binop`)
        // so the builder can desugar to the construct; DOTDOT's precedence
        // still drives the LR resolution like any infix operator.
        expr = term => term | expr binop expr => binop | expr DOTDOT expr => range;
        binop = OP => op | MINUS => minus | LANGLE => lt | RANGLE => gt | OROR => or;

        term = unary => unary;

        unary = MINUS unary => neg | BANG unary => not | postfix => postfix;

        postfix = atom => atom
                | postfix DOT IDENT => field_access
                | postfix DOT NUM => tuple_index
                | postfix DOT IDENT LPAREN args RPAREN => method_call
                // `recv[index]` — indexing; a closed slice `recv[a..b]` also
                // arrives here (the index expr is a range), and the builder
                // folds that literal shape back into a `Slice` node.
                | postfix LBRACKET expr RBRACKET => index
                // `recv[start..]` — open-ended slice (to the receiver's length).
                // The range production can't match here (no expr after `..`),
                // so `..` followed by `]` stays unambiguous.
                | postfix LBRACKET expr DOTDOT RBRACKET => slice_open
                // `recv[..end]` — open *start* (from 0 to `end`). The `..`
                // immediately after `[` distinguishes it from the other forms.
                | postfix LBRACKET DOTDOT expr RBRACKET => slice_to
                // `expr?` — error propagation. `?` (QUESTION) is the optional
                // *type* postfix elsewhere, but that's type position; here in
                // expression position it's the try operator.
                | postfix QUESTION => try_op;

        // 1+ extra elements after the first in a tuple literal.
        tuple_more = expr => single | tuple_more COMMA expr => more;

        atom = NUM => num
             | TRUE => true_lit
             | FALSE => false_lit
             | STR => string_lit
             | CHAR => char_lit
             | IDENT => ident
             | IDENT LPAREN args RPAREN => call
             | IDENT LBRACE field_inits RBRACE => construct
             | LPAREN expr RPAREN => paren
             // `(a, b, ...)` — a tuple literal (2+ elements). The COMMA after
             // the first expr unambiguously selects this over `paren`.
             | LPAREN expr COMMA tuple_more RPAREN => tuple_lit
             | IF LPAREN expr RPAREN block ELSE else_branch => if_else
             // Else-less `if` (statement position): yields unit, so its `then`
             // block must be unit-typed. Desugars to `if .. {} else {}`.
             | IF LPAREN expr RPAREN block => if_no_else
             | NONE => none_lit
             | MATCH LPAREN expr RPAREN LBRACE match_arms RBRACE => match_expr
             // `shim <effect> { op = f, .. } { body }` — install shims for an
             // effect's operations over the dynamic extent of `body`. An
             // expression (its value is the body's), like `match`. The leading
             // SHIM keyword makes it a single-token decision.
             | SHIM IDENT LBRACE shim_bindings RBRACE block => shim_expr
             | LBRACKET args RBRACKET => array_lit
             | HASH LBRACE brace_body RBRACE => brace_lit
             // Template literal: `` `text {expr} text` `` with 1+ interpolations.
             // The no-interpolation case is emitted as a plain STR by the lexer.
             | TEMPLATE_HEAD expr template_rest => template_lit
             // A lambda used as a *value* (not a call argument): bound to a
             // local, stored in a struct field, etc. The body must be a
             // brace-delimited block — a bare-expr body would extend greedily
             // and clash with trailing operators (`let f = |x: i64| x + 1` is
             // ambiguous with `(|x| x) + 1`), while a block is self-delimiting.
             // The `||` no-arg form is excluded (its `OROR` lead clashes with
             // `||` as a binary operator).
             | PIPE lambda_params PIPE block => lambda_block;

        // What follows `else`: a plain block (`else { .. }`) or a chained
        // `else if (..) { .. }` — deliberately *only* another `if`, never an
        // arbitrary expression, so an if/else-if/else ladder is the only
        // brace-less `else` form. The chain is just a right-nested `if` in the
        // else position, so it reuses the same dangling-else resolution (shift
        // toward the nearer `else`) as the top-level `atom` if-productions.
        else_branch = block => plain
                    | IF LPAREN expr RPAREN block ELSE else_branch => elif
                    | IF LPAREN expr RPAREN block => elif_no_else;

        // The portion of a template literal after the first interpolation.
        // Either the closing tail (`TEMPLATE_TAIL`) or another interpolation
        // followed by more template_rest.
        template_rest = TEMPLATE_TAIL => tail
                      | TEMPLATE_MIDDLE expr template_rest => middle;

        // `#{ .. }` is a set literal (`#{a, b}`), a dict literal (`#{k: v}`), or
        // an empty of either (`#{}` set, `#{:}` dict). One production handles
        // all four so `expr` is reachable from a *single* place under the
        // brace — having `expr` reachable two ways is what blew up the LR tables
        // for the block grammar. Each entry is a key with an optional `: value`;
        // the builder rejects a set/dict mix and chooses the literal kind.
        brace_body = entry_list => entries
                   | entry_list COMMA => entries_trailing
                   | COLON => empty_dict
                   | _ => empty_set;
        entry_list = entry => first | entry_list COMMA entry => rest;
        // `expr COLON expr` vs `expr` diverge on a single lookahead token
        // (`COLON` → key/value, else → key-only), an LR(1) shift-reduce choice.
        entry = expr => key_only | expr COLON expr => key_value;

        // A trailing comma after the last arm is optional.
        match_arms = match_arm_list => present
                   | match_arm_list COMMA => present_trailing
                   | _ => empty;
        match_arm_list = match_arm => first | match_arm_list COMMA match_arm => rest;
        // Uniform constructor patterns: `Ctor(b0, b1, ...) => body`, a nullary
        // `Ctor => body`, or `none => body`. The scrutinee's type (optional vs
        // variant) decides which `ctor` names are legal (checked downstream). A
        // string-literal arm `"foo" => body` matches a `str` scrutinee; the
        // wildcard `_ => body` arrives as a `nullary_arm` (since `_` lexes as an
        // identifier) and is recognized downstream.
        match_arm = IDENT LPAREN match_bindings RPAREN FATARROW arm_body => ctor_arm
                  // `V.A(b0, ..) => body` / `V.A => body` — a variant-qualified
                  // constructor pattern. After the leading IDENT, one token of
                  // lookahead (`.` → qualified, `(` → ctor_arm, `=>` → nullary)
                  // picks the production.
                  | IDENT DOT IDENT LPAREN match_bindings RPAREN FATARROW arm_body => qualified_ctor_arm
                  | IDENT DOT IDENT FATARROW arm_body => qualified_nullary_arm
                  | IDENT FATARROW arm_body => nullary_arm
                  | NONE FATARROW arm_body => none_arm
                  | STR FATARROW arm_body => str_arm
                  // `'c' => body` — a char-literal arm, matching a `char`
                  // scrutinee by value. Like the `str` form it is open-domain,
                  // so such a match must end in `_`.
                  | CHAR FATARROW arm_body => char_arm
                  | LBRACKET args RBRACKET FATARROW arm_body => array_arm
                  // `A | B | C => body` — several patterns sharing one body.
                  // Only binding-free patterns may be grouped: an alternative
                  // that bound a payload would have to bind the same names, at
                  // the same types, in every branch, and nothing downstream is
                  // set up to check that. So `some(v) | none` is a parse error,
                  // not a silently-wrong binding.
                  //
                  // Expanded here into one `MatchArm` per pattern (the body is
                  // cloned), so no later stage sees an alternation at all —
                  // exhaustiveness, codegen and the lints all keep working on
                  // the shape they already understand.
                  | alt_heads FATARROW arm_body => alt_arm;
        // Two or more binding-free patterns. A *single* pattern is already
        // covered by the productions above, so this starts at a pair — which is
        // also what keeps the grammar LR(1): after an IDENT, one token of
        // lookahead (`|` here, `=>` / `(` / `.` there) picks the production.
        alt_heads = alt_head PIPE alt_head => alt_pair
                  | alt_heads PIPE alt_head => alt_more;
        alt_head = IDENT => alt_nullary
                 | IDENT DOT IDENT => alt_qualified
                 | NONE => alt_none
                 | STR => alt_str
                 | CHAR => alt_char;
        // An arm's body: a single expression, or a brace-delimited statement
        // block whose trailing expression (if any) is the arm's value — exactly
        // an `if` branch's shape. Factored into its own nonterminal rather than
        // doubling all eight `match_arm` productions, which also keeps `expr`
        // reachable from a *single* place here (see the `block_body` note above
        // on what a second `expr` occurrence does to the LR tables). No AST
        // change: `block` already builds an `Expr`, so both forms yield the same
        // `MatchArm.body`. The choice is one token of lookahead — no expression
        // begins with `{`.
        arm_body = expr => expr | block => block;
        match_bindings = binding_list => present;
        binding_list = IDENT => first | binding_list COMMA IDENT => rest;

        args = arg_list => present
             | arg_list COMMA => present_trailing
             | _ => empty;
        arg_list = arg => first | arg_list COMMA arg => rest;
        // An argument is an ordinary expression, a lambda, a bare operator
        // passed as a value (`apply(2, 3, +)`), or a keyword argument
        // (`f(1, k = 2)`). All are confined to argument position (a
        // lambda's/operator-value's only valid use), which keeps the expression
        // grammar — and its operator precedence — untouched. An `OP`-token
        // operator can't begin any other arg form, so it's unambiguous here; it
        // desugars to a binary lambda. A keyword argument is unambiguous too:
        // `=` (EQ) never follows an expression, so after an IDENT the EQ
        // lookahead selects this production over reducing the IDENT to an atom.
        // `..xs` — an array-literal spread. It rides on `arg` (rather than a
        // dedicated array-element rule) so `expr` stays reachable from exactly
        // one place here; a second path to `expr` is what blew the LR tables up
        // for the block grammar. `DOTDOT` can't begin any other arg form, so
        // it's an unambiguous lead. The builder rejects it outside an array
        // literal — `args` is shared with calls and array patterns.
        arg = expr => expr | lambda => lambda | OP => op_value
            | IDENT EQ expr => kw_arg
            | DOTDOT expr => spread;
        lambda = PIPE lambda_params PIPE expr => lambda_expr
               | OROR expr => lambda_noargs
               | OROR block => lambda_noargs_block;
        lambda_params = lambda_param_list => present | _ => empty;
        lambda_param_list = lambda_param => first
                          | lambda_param_list COMMA lambda_param => rest;
        lambda_param = IDENT => untyped | IDENT COLON ty => typed;

        field_inits = field_init_list => present
                    | field_init_list COMMA => present_trailing
                    | _ => empty;
        field_init_list = field_init => first | field_init_list COMMA field_init => rest;
        // `x: expr`, or the Rust-style shorthand `x` — sugar for `x: x`, where the
        // value is the identifier of the same name in scope. After the IDENT, a
        // COLON lookahead selects the explicit form; COMMA/RBRACE the shorthand.
        field_init = IDENT COLON expr => field_init
                   | IDENT => field_init_shorthand
                   // `..base` — a struct spread: every field not given
                   // explicitly is taken from `base`. `DOTDOT` can't begin
                   // either other form, so it's an unambiguous lead (the same
                   // reasoning as the array-literal spread on `arg`). The
                   // builder carries it as a nameless init whose value is an
                   // `ExprKind::Spread`; the loader desugars it away.
                   | DOTDOT expr => field_init_spread;
    }
}

pub struct Build;

impl gazelle::ErrorType for Build {
    // A few productions are intentionally permissive (e.g. `#{ .. }` accepts a
    // mix of set elements and `key: value` pairs) and reject the bad shape in
    // the build action, so build is fallible.
    type Error = Error;
}

/// One `#{ .. }` entry as parsed: a bare key (a set element) or a `key: value`
/// (a dict pair). The builder collects these, rejects a set/dict mix, and emits
/// a `SetLit` or `DictLit`.
pub enum BraceEntry {
    KeyOnly(Expr),
    KeyValue(Expr, Expr),
}

/// The parsed body of a `#{ .. }`: a list of entries (set or dict, decided by
/// their kind), or one of the two empties (`#{}` set, `#{:}` dict).
pub enum BraceLit {
    Entries(Vec<BraceEntry>),
    EmptyDict,
    EmptySet,
}

impl aipl::Types for Build {
    type Ident = (String, Span);
    type Num = (i64, Span);
    type Str = (String, Span);
    type Char = (u8, Span);
    type True = Span;
    type False = Span;
    type None = Span;
    type Lbracket = Span;
    type Shim = Span;
    type Plusplus = Span;
    type Hash = Span;
    type Builtins = Span;
    type Op = (char, Span);
    type Binop = char;
    type Term = Expr;
    type Expr = Expr;
    type Ty = Type;
    type Param = Param;
    type ParamList = Vec<Param>;
    type Params = Vec<Param>;
    type BaseTy = Type;
    type ReturnTy = Option<Type>;
    type FnAttr = ParsedAttr;
    type FnAttrList = Vec<ParsedAttr>;
    type FnAttrs = Vec<ParsedAttr>;
    type TypeParams = Vec<TypeParam>;
    type TypeParamList = Vec<TypeParam>;
    type TypeParam = TypeParam;
    type Block = Expr;
    type FnBody = ParsedBody;
    type ConstructFields = (Vec<FieldInit>, Span);
    type ElseBranch = Expr;
    type LoopBody = Expr;
    type Function = Function;
    type Item = Item;
    type Program = Program;
    type Args = Vec<Expr>;
    type ArgList = Vec<Expr>;
    type Arg = Expr;
    type BraceBody = BraceLit;
    type EntryList = Vec<BraceEntry>;
    type Entry = BraceEntry;
    type Lambda = Expr;
    type LambdaParams = Vec<LambdaParam>;
    type LambdaParamList = Vec<LambdaParam>;
    type LambdaParam = LambdaParam;
    type Pipe = Span;
    type TyArgs = Vec<Type>;
    type TyArgList = Vec<Type>;
    type Unary = Expr;
    type Postfix = Expr;
    type Atom = Expr;
    type StructDecl = StructDecl;
    type VariantDecl = VariantDecl;
    type VariantCases = Vec<VariantCase>;
    type VariantCase = VariantCase;
    type MatchBindings = Vec<String>;
    type BindingList = Vec<String>;
    type FieldDecl = FieldDecl;
    type FieldDeclList = Vec<FieldDecl>;
    type Fields = Vec<FieldDecl>;
    type FieldInit = FieldInit;
    type FieldInitList = Vec<FieldInit>;
    type FieldInits = Vec<FieldInit>;
    type Effect = String;
    type EffectList = Vec<String>;
    type Effects = Vec<String>;
    type Vis = bool;
    type ImportDecl = ImportDecl;
    type ImportName = ImportName;
    type ImportNameList = Vec<ImportName>;
    type ImportNames = Vec<ImportName>;
    type ArmBody = Expr;
    // One written arm can produce several: see the `alt_arm` production.
    type MatchArm = Vec<MatchArm>;
    type AltHeads = Vec<(Pattern, Span)>;
    type AltHead = (Pattern, Span);
    /// A shim's `op = f` binding, and the lists built from it.
    type ShimBinding = (String, String);
    type ShimBindingList = Vec<(String, String)>;
    type ShimBindings = Vec<(String, String)>;
    type MatchArmList = Vec<MatchArm>;
    type MatchArms = Vec<MatchArm>;
    type TupleMore = Vec<Expr>;
    type TemplateHead = (String, Span);
    type TemplateMiddle = (String, Span);
    type TemplateTail = (String, Span);
    // Represents the right-hand portion of a template literal (after the first
    // `{expr}`) already folded into a single Expr via __aipl_concat chains.
    type TemplateRest = Expr;
    type BlockBody = Expr;
    type BlockTail = BlockTail;
    type LoopInner = Expr;
    type KwStmt = StmtSpec;
    type LetStmt = StmtSpec;
    type LetTupleStmt = StmtSpec;
    type LetStructStmt = StmtSpec;
    type StructFieldBindings = Vec<String>;
    type StructFieldBindingList = Vec<String>;
    type MutStmt = StmtSpec;
    type AssignStmt = StmtSpec;
    type FieldPath = Vec<(String, Span)>;
    type ForStmt = StmtSpec;
    type ForTupleStmt = StmtSpec;
    type ForIndexedStmt = StmtSpec;
    type WhileStmt = StmtSpec;
    type ReturnStmt = StmtSpec;
}

/// A block-body statement, in the form the block-builder needs to fold
/// it into the enclosing expression chain.
pub enum StmtSpec {
    Let {
        name: String,
        name_span: Span,
        /// `Some(T)` for the annotated `let name: T = value;` form.
        ty: Option<Type>,
        value: Expr,
        span: Span,
    },
    LetTuple {
        names: Vec<String>,
        value: Expr,
        span: Span,
    },
    LetStruct {
        struct_name: String,
        fields: Vec<String>,
        value: Expr,
        span: Span,
    },
    Mut {
        name: String,
        name_span: Span,
        /// `Some(T)` for the annotated `mut name: T = value;` form.
        ty: Option<Type>,
        value: Expr,
        span: Span,
    },
    Assign {
        /// The place being stored to: a bare `Ident`, or a `Field` chain of
        /// any depth rooted at one (`set a.b.c = v;`).
        lhs: Expr,
        value: Expr,
        span: Span,
    },
    For {
        var: String,
        var_span: Span,
        /// `Some(i)` for the `for (let i, x : xs)` form: the name bound to the
        /// iteration index. The block folder desugars it (it needs the rest of
        /// the block to scope the counter over).
        index: Option<String>,
        iterable: Expr,
        body: Expr,
        span: Span,
    },
    While {
        cond: Expr,
        body: Expr,
        span: Span,
    },
    Return {
        value: Expr,
        span: Span,
    },
}

/// What follows the leading expression of a `block_body`: either the
/// expression *is* the block's trailing value, or a `;` discards it and the
/// rest of the block follows.
pub enum BlockTail {
    /// No `;` — the preceding expression is the block's value.
    Value,
    /// `; <rest>` — discard the preceding expression; `rest` is the folded
    /// remainder of the block.
    Discard(Expr),
}

impl gazelle::Action<aipl::Program<Self>> for Build {
    fn build(&mut self, node: aipl::Program<Self>) -> Result<Program, Self::Error> {
        let aipl::Program::Program(items) = node;
        Ok(Program { items })
    }
}

impl gazelle::Action<aipl::Item<Self>> for Build {
    fn build(&mut self, node: aipl::Item<Self>) -> Result<Item, Self::Error> {
        Ok(match node {
            aipl::Item::Function(f) => Item::Fn(f),
            aipl::Item::StructDecl(s) => Item::Struct(s),
            aipl::Item::VariantDecl(v) => Item::Variant(v),
            aipl::Item::ImportDecl(i) => Item::Import(i),
        })
    }
}

impl gazelle::Action<aipl::ImportDecl<Self>> for Build {
    fn build(&mut self, node: aipl::ImportDecl<Self>) -> Result<ImportDecl, Self::Error> {
        Ok(match node {
            aipl::ImportDecl::Import(names, (from, from_span)) => ImportDecl {
                names,
                source: ImportSource::Path {
                    path: from,
                    span: from_span,
                },
            },
            aipl::ImportDecl::ImportBuiltins(names, builtins_span) => ImportDecl {
                names,
                source: ImportSource::Builtins {
                    span: builtins_span,
                },
            },
        })
    }
}

impl gazelle::Action<aipl::ImportName<Self>> for Build {
    fn build(&mut self, node: aipl::ImportName<Self>) -> Result<ImportName, Self::Error> {
        Ok(match node {
            aipl::ImportName::Plain((name, span)) => ImportName {
                name,
                alias: None,
                span,
            },
            // `name as alias`: the span covers the exported name (where an
            // "is not exported" error should point).
            aipl::ImportName::Aliased((name, span), (alias, _)) => ImportName {
                name,
                alias: Some(alias),
                span,
            },
            // A named builtin aliased to an operator (`wrapping_add as +`): the
            // name carries the span; the operator is the local alias.
            aipl::ImportName::AliasedOp((name, span), (c, _)) => {
                name_as_op(name, span, op_spelling(c))
            }
            aipl::ImportName::AliasedMinus((name, span)) => name_as_op(name, span, "-"),
            aipl::ImportName::AliasedLt((name, span)) => name_as_op(name, span, "<"),
            aipl::ImportName::AliasedGt((name, span)) => name_as_op(name, span, ">"),
            aipl::ImportName::AliasedOr((name, span)) => name_as_op(name, span, "||"),
            aipl::ImportName::AliasedPlusplus((name, span), _) => name_as_op(name, span, "++"),
            aipl::ImportName::AliasedBang((name, span)) => name_as_op(name, span, "!"),
            // Operator imports (`import { equal as ==, less_than as < } from builtins`). An `OP` token
            // carries a span, so keep it — the unused-import lint reports at the
            // imported name, and without a span its caret would land on the
            // file's first line. The bare `-`/`<`/`>`/`||`/`!` tokens below
            // produce no span at all; those fall back to the import statement's
            // own span (see the lint's `unused_imports`).
            aipl::ImportName::Op((c, span)) => op_import_at(op_spelling(c), span),
            aipl::ImportName::OpMinus => op_import("-"),
            aipl::ImportName::OpLt => op_import("<"),
            aipl::ImportName::OpGt => op_import(">"),
            aipl::ImportName::OpOr => op_import("||"),
            aipl::ImportName::OpBang => op_import("!"),
            // `import { ++ } from builtins;` — parsed so the loader can reject it
            // with the "no bare form" message, exactly as it does for a bare `+`:
            // `++` is reached through `wrapping_increment as ++` /
            // `saturating_increment as ++`.
            aipl::ImportName::OpPlusplus(span) => op_import_at("++", span),
        })
    }
}

/// An `ImportName` for an operator whose token carries no span (`-`, `<`, `>`,
/// `||`, `!`). The empty span is a marker: a diagnostic pointing at the name
/// falls back to the enclosing import statement.
fn op_import(spelling: &str) -> ImportName {
    ImportName {
        name: spelling.to_string(),
        alias: None,
        span: 0..0,
    }
}

/// An `ImportName` for an operator token that does carry a span.
fn op_import_at(spelling: &str, span: Span) -> ImportName {
    ImportName {
        name: spelling.to_string(),
        alias: None,
        span,
    }
}

/// An `ImportName` binding builtin `name` to operator `op` (`name as op`).
fn name_as_op(name: String, span: Span, op: &str) -> ImportName {
    ImportName {
        name,
        alias: Some(op.to_string()),
        span,
    }
}

/// The spelling of an `OP`-token operator char (e.g. `'E'` → `"=="`).
fn op_spelling(c: char) -> &'static str {
    match c {
        '+' => "+",
        '*' => "*",
        '/' => "/",
        '%' => "%",
        'E' => "==",
        'N' => "!=",
        'L' => "<=",
        'G' => ">=",
        'A' => "&&",
        // `+++` — the string-concatenation operator.
        'C' => "+++",
        other => unreachable!("unexpected OP char {other:?} in import"),
    }
}

impl gazelle::Action<aipl::ImportNames<Self>> for Build {
    fn build(&mut self, node: aipl::ImportNames<Self>) -> Result<Vec<ImportName>, Self::Error> {
        Ok(match node {
            aipl::ImportNames::Present(list) | aipl::ImportNames::PresentTrailing(list) => list,
            aipl::ImportNames::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::ImportNameList<Self>> for Build {
    fn build(&mut self, node: aipl::ImportNameList<Self>) -> Result<Vec<ImportName>, Self::Error> {
        Ok(match node {
            aipl::ImportNameList::First(name) => vec![name],
            aipl::ImportNameList::Rest(mut prev, name) => {
                prev.push(name);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::StructDecl<Self>> for Build {
    fn build(&mut self, node: aipl::StructDecl<Self>) -> Result<StructDecl, Self::Error> {
        let aipl::StructDecl::StructDecl((name, _), type_params, fields) = node;
        Ok(StructDecl {
            name,
            type_vars: type_params,
            fields,
        })
    }
}

impl gazelle::Action<aipl::VariantDecl<Self>> for Build {
    fn build(&mut self, node: aipl::VariantDecl<Self>) -> Result<VariantDecl, Self::Error> {
        let aipl::VariantDecl::VariantDecl((name, _), type_params, cases) = node;
        Ok(VariantDecl {
            name,
            type_vars: type_params,
            cases,
        })
    }
}

impl gazelle::Action<aipl::VariantCases<Self>> for Build {
    fn build(&mut self, node: aipl::VariantCases<Self>) -> Result<Vec<VariantCase>, Self::Error> {
        Ok(match node {
            aipl::VariantCases::First(c) | aipl::VariantCases::FirstLeadingPipe(_, c) => vec![c],
            aipl::VariantCases::Rest(mut prev, _pipe, c) => {
                prev.push(c);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::VariantCase<Self>> for Build {
    fn build(&mut self, node: aipl::VariantCase<Self>) -> Result<VariantCase, Self::Error> {
        Ok(match node {
            aipl::VariantCase::Nullary((name, _)) => VariantCase {
                name,
                payload: Vec::new(),
            },
            aipl::VariantCase::WithPayload((name, _), payload) => VariantCase { name, payload },
        })
    }
}

impl gazelle::Action<aipl::Fields<Self>> for Build {
    fn build(&mut self, node: aipl::Fields<Self>) -> Result<Vec<FieldDecl>, Self::Error> {
        Ok(match node {
            aipl::Fields::Present(list) | aipl::Fields::PresentTrailing(list) => list,
            aipl::Fields::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::FieldDeclList<Self>> for Build {
    fn build(&mut self, node: aipl::FieldDeclList<Self>) -> Result<Vec<FieldDecl>, Self::Error> {
        Ok(match node {
            aipl::FieldDeclList::First(f) => vec![f],
            aipl::FieldDeclList::Rest(mut prev, f) => {
                prev.push(f);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::FieldDecl<Self>> for Build {
    fn build(&mut self, node: aipl::FieldDecl<Self>) -> Result<FieldDecl, Self::Error> {
        match node {
            aipl::FieldDecl::FieldDecl((name, _), ty) => Ok(FieldDecl {
                name,
                ty,
                default: None,
            }),
            aipl::FieldDecl::WithDefault((name, _), ty, default) => Ok(FieldDecl {
                name,
                ty,
                default: Some(default),
            }),
        }
    }
}

impl gazelle::Action<aipl::FieldInits<Self>> for Build {
    fn build(&mut self, node: aipl::FieldInits<Self>) -> Result<Vec<FieldInit>, Self::Error> {
        Ok(match node {
            aipl::FieldInits::Present(list) | aipl::FieldInits::PresentTrailing(list) => list,
            aipl::FieldInits::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::FieldInitList<Self>> for Build {
    fn build(&mut self, node: aipl::FieldInitList<Self>) -> Result<Vec<FieldInit>, Self::Error> {
        Ok(match node {
            aipl::FieldInitList::First(fi) => vec![fi],
            aipl::FieldInitList::Rest(mut prev, fi) => {
                prev.push(fi);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::FieldInit<Self>> for Build {
    fn build(&mut self, node: aipl::FieldInit<Self>) -> Result<FieldInit, Self::Error> {
        Ok(match node {
            aipl::FieldInit::FieldInit((name, _), value) => FieldInit { name, value },
            // Shorthand `x` desugars to `x: x` — the value is a reference to the
            // in-scope identifier of the same name, spanned at the field name.
            aipl::FieldInit::FieldInitShorthand((name, span)) => {
                let value = Expr::new(ExprKind::Ident(name.clone()), span);
                FieldInit { name, value }
            }
            // `..base`. A spread names no field, so it rides as an init with an
            // empty name whose *value* is the `Spread` — the value is what
            // identifies it, exactly as an array-literal spread is identified by
            // being a `Spread` element. The loader replaces it with the fields
            // it stands for, so nothing downstream sees either the empty name or
            // the `Spread`.
            aipl::FieldInit::FieldInitSpread(base) => {
                // Spanned at the operand, like the array-literal spread, so an
                // error points at what is being spread.
                let span = base.span.clone();
                FieldInit {
                    name: String::new(),
                    value: Expr::new(ExprKind::Spread(Box::new(base)), span),
                }
            }
        })
    }
}

/// A parsed function body: an ordinary value block, or the struct-literal
/// shorthand `{ field: value, .. }`, whose type name comes from the declared
/// return type rather than the source. Resolved into a plain body `Expr` by the
/// `Function` build action, which is the first place both halves are in scope.
/// `pub` only because it surfaces as a gazelle `Action` associated type; not
/// part of the crate's intended API.
pub enum ParsedBody {
    Block(Expr),
    /// The shorthand's field list, spanned over the whole `{ .. }`.
    Construct(Vec<FieldInit>, Span),
}

impl gazelle::Action<aipl::FnBody<Self>> for Build {
    fn build(&mut self, node: aipl::FnBody<Self>) -> Result<ParsedBody, Self::Error> {
        Ok(match node {
            aipl::FnBody::Block(block) => ParsedBody::Block(block),
            aipl::FnBody::Construct((fields, span)) => ParsedBody::Construct(fields, span),
        })
    }
}

impl gazelle::Action<aipl::ConstructFields<Self>> for Build {
    fn build(
        &mut self,
        node: aipl::ConstructFields<Self>,
    ) -> Result<(Vec<FieldInit>, Span), Self::Error> {
        // Every form is spanned from the first field's name to the last field's
        // value, so a diagnostic underlines the whole list.
        let (start, first, rest) = match node {
            // `{ name: value }` — one explicit field, no comma.
            aipl::ConstructFields::One((name, span), value) => {
                (span, FieldInit { name, value }, Vec::new())
            }
            // `{ name: value, rest.. }`.
            aipl::ConstructFields::Many((name, span), value, rest) => {
                (span, FieldInit { name, value }, rest)
            }
            // `{ name, rest.. }` — a leading shorthand field, which is also how
            // the trailing-comma form `{ x, }` spells a lone one. The value is a
            // reference to the identifier of the same name, as in `field_init`.
            aipl::ConstructFields::ManyShorthand((name, span), rest) => {
                let value = Expr::new(ExprKind::Ident(name.clone()), span.clone());
                (span, FieldInit { name, value }, rest)
            }
            // `{ ..base, rest.. }`. The spread rides as a nameless `FieldInit`
            // holding an `ExprKind::Spread`, exactly as in a written struct
            // literal — so the loader's `desugar_struct_spread` expands it with
            // no knowledge that it came from the shorthand.
            aipl::ConstructFields::SpreadFirst(base, rest) => {
                // `..` itself carries no span through the grammar, so the list is
                // spanned from the base expression — one token to its right.
                let span = base.span.clone();
                let value = Expr::new(ExprKind::Spread(Box::new(base)), span.clone());
                (
                    span,
                    FieldInit {
                        name: String::new(),
                        value,
                    },
                    rest,
                )
            }
        };
        let mut fields = vec![first];
        fields.extend(rest);
        let span = fields
            .last()
            .map_or(start.clone(), |f| join_spans(&start, &f.value.span));
        Ok((fields, span))
    }
}

/// A parsed function attribute (`.test({ .. })` or `.doc("...")`), carrying the
/// attribute name's span for duplicate-attribute diagnostics. Folded into the
/// `Function`'s `test_body` / `doc` by the `Function` build action. `pub` only
/// because it surfaces as a gazelle `Action` associated type; not part of the
/// crate's intended API.
pub enum ParsedAttr {
    Test(Expr, Span),
    Doc(String, Span),
}

impl gazelle::Action<aipl::Function<Self>> for Build {
    fn build(&mut self, node: aipl::Function<Self>) -> Result<Function, Self::Error> {
        let aipl::Function::Function(
            is_pub,
            (name, _),
            type_params,
            params,
            effects,
            return_ty,
            body,
            attrs,
        ) = node;
        // Fold the attribute list into the single `test_body` / `doc` slots,
        // rejecting a repeated attribute (which slot would silently win?).
        let mut test_body = None;
        let mut doc = None;
        for attr in attrs {
            match attr {
                ParsedAttr::Test(block, span) => {
                    if test_body.is_some() {
                        return Err(Error::at("duplicate `.test` attribute", span));
                    }
                    test_body = Some(block);
                }
                ParsedAttr::Doc(text, span) => {
                    if doc.is_some() {
                        return Err(Error::at("duplicate `.doc` attribute", span));
                    }
                    doc = Some(text);
                }
            }
        }
        // Resolve the struct-literal shorthand now that the return type and the
        // field list are both in hand. The result is exactly the `Construct` the
        // long form parses to, so nothing downstream of the parser sees the
        // sugar at all.
        let body = match body {
            ParsedBody::Block(e) => e,
            ParsedBody::Construct(fields, span) => {
                // The shorthand names no type, so the return type has to. A
                // generic return (`-> Pair<i64>`) contributes its base name and
                // infers its arguments, exactly like writing `Pair { .. }`.
                let ty_name = match &return_ty {
                    Some(Type::Named(n)) => n.clone(),
                    Some(Type::Generic(base, _)) => base.clone(),
                    Some(other) => {
                        return Err(Error::at(
                            format!(
                                "fn {name:?}: a `{{ field: value, .. }}` body needs a struct return type to name, but this function returns {}; give the literal its type, or return a struct",
                                aipl_syntax::type_name(other)
                            ),
                            span,
                        ))
                    }
                    None => {
                        return Err(Error::at(
                            format!(
                                "fn {name:?}: a `{{ field: value, .. }}` body needs a struct return type to name, but this function declares no return type; add `-> Struct`, or give the literal its type"
                            ),
                            span,
                        ))
                    }
                };
                Expr::new(ExprKind::Construct(ty_name, fields), span)
            }
        };
        Ok(Function {
            name,
            is_pub,
            sig: Signature {
                type_vars: type_params,
                params,
                effects,
                return_ty,
            },
            body,
            test_body,
            doc,
        })
    }
}

impl gazelle::Action<aipl::Vis<Self>> for Build {
    fn build(&mut self, node: aipl::Vis<Self>) -> Result<bool, Self::Error> {
        Ok(matches!(node, aipl::Vis::Public))
    }
}

impl gazelle::Action<aipl::FnAttr<Self>> for Build {
    fn build(&mut self, node: aipl::FnAttr<Self>) -> Result<ParsedAttr, Self::Error> {
        // The argument shape (`{ .. }` block vs string) is fixed by the grammar
        // production; here we only validate the attribute name matches it.
        let unknown = |name: &str, name_span| {
            Error::at(
                format!(
                    "unknown function attribute {name:?}; only `.test({{ .. }})` and \
                     `.doc(\"..\")` are supported"
                ),
                name_span,
            )
        };
        Ok(match node {
            aipl::FnAttr::Test((name, name_span), block) => match name.as_str() {
                "test" => ParsedAttr::Test(block, name_span),
                "doc" => {
                    return Err(Error::at(
                        "`.doc` takes a string argument, not a `{ .. }` block",
                        name_span,
                    ))
                }
                _ => return Err(unknown(&name, name_span)),
            },
            aipl::FnAttr::Doc((name, name_span), (text, _)) => match name.as_str() {
                "doc" => ParsedAttr::Doc(text, name_span),
                "test" => {
                    return Err(Error::at(
                        "`.test` takes a `{ .. }` block, not a string argument",
                        name_span,
                    ))
                }
                _ => return Err(unknown(&name, name_span)),
            },
        })
    }
}

impl gazelle::Action<aipl::FnAttrList<Self>> for Build {
    fn build(&mut self, node: aipl::FnAttrList<Self>) -> Result<Vec<ParsedAttr>, Self::Error> {
        Ok(match node {
            aipl::FnAttrList::First(a) => vec![a],
            aipl::FnAttrList::Rest(mut prev, a) => {
                prev.push(a);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::FnAttrs<Self>> for Build {
    fn build(&mut self, node: aipl::FnAttrs<Self>) -> Result<Vec<ParsedAttr>, Self::Error> {
        Ok(match node {
            aipl::FnAttrs::Present(list) => list,
            aipl::FnAttrs::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::Effects<Self>> for Build {
    fn build(&mut self, node: aipl::Effects<Self>) -> Result<Vec<String>, Self::Error> {
        Ok(match node {
            aipl::Effects::Present(list) => list,
            aipl::Effects::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::EffectList<Self>> for Build {
    fn build(&mut self, node: aipl::EffectList<Self>) -> Result<Vec<String>, Self::Error> {
        Ok(match node {
            aipl::EffectList::First(e) => vec![e],
            aipl::EffectList::Rest(mut prev, e) => {
                prev.push(e);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::Effect<Self>> for Build {
    fn build(&mut self, node: aipl::Effect<Self>) -> Result<String, Self::Error> {
        let aipl::Effect::Effect((name, _)) = node;
        Ok(name)
    }
}

impl gazelle::Action<aipl::Params<Self>> for Build {
    fn build(&mut self, node: aipl::Params<Self>) -> Result<Vec<Param>, Self::Error> {
        Ok(match node {
            aipl::Params::Present(list) | aipl::Params::PresentTrailing(list) => list,
            aipl::Params::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::ParamList<Self>> for Build {
    fn build(&mut self, node: aipl::ParamList<Self>) -> Result<Vec<Param>, Self::Error> {
        Ok(match node {
            aipl::ParamList::First(p) => vec![p],
            aipl::ParamList::Rest(mut prev, p) => {
                prev.push(p);
                prev
            }
        })
    }
}

/// The *sequence* type a variadic parameter's body sees, from its declared
/// element type: a `char` sequence is an AIPL string (`char*` → `str`), every
/// other element `T` uses `T[]` (`str*` → `str[]`, `i64*` → `i64[]`). The
/// element type stays recoverable from it (see `variadic_elem`). Errors when the
/// trailing operator isn't `*` — the grammar admits any operator token there, so
/// this is where `T+`/`T?` are turned away.
fn variadic_seq_ty(elem: Type, op: char, op_span: Span) -> Result<Type, Error> {
    if op != '*' {
        return Err(Error::at(
            format!("expected \"*\" after a variadic parameter type, found {op:?}"),
            op_span,
        ));
    }
    Ok(if elem == Type::Primitive(Primitive::Char) {
        Type::Primitive(Primitive::Str)
    } else {
        Type::Array(Box::new(elem))
    })
}

impl gazelle::Action<aipl::Param<Self>> for Build {
    fn build(&mut self, node: aipl::Param<Self>) -> Result<Param, Self::Error> {
        Ok(match node {
            aipl::Param::Param((name, _), ty) => Param {
                name,
                ty,
                mutable: false,
                variadic: false,
                default: None,
            },
            aipl::Param::MutParam((name, _), ty) => Param {
                name,
                ty,
                mutable: true,
                variadic: false,
                default: None,
            },
            // `k: T = expr` — a keyword parameter (the default is what makes
            // it one; see `ast::Param::default`).
            aipl::Param::WithDefault((name, _), ty, default) => Param {
                name,
                ty,
                mutable: false,
                variadic: false,
                default: Some(default),
            },
            // `x: T*` — a variadic parameter. The trailing operator must be `*`.
            // The stored type is the *sequence type* the body sees: `str` when
            // the element is `char` (an AIPL string is the char sequence),
            // otherwise `T[]`. The element type stays recoverable from it.
            aipl::Param::VariadicParam((name, _), elem, (op, op_span)) => Param {
                name,
                ty: variadic_seq_ty(elem, op, op_span)?,
                mutable: false,
                variadic: true,
                default: None,
            },
            // `k: T* = expr` — both at once: variadic in what it accepts (a
            // sequence, a bare element, or an optional one) and a keyword
            // parameter in how it is passed, since the default is what makes a
            // parameter one. Omitted, it takes the default; supplied, it must be
            // named (`sep = ", "`).
            aipl::Param::VariadicWithDefault((name, _), elem, (op, op_span), default) => Param {
                name,
                ty: variadic_seq_ty(elem, op, op_span)?,
                mutable: false,
                variadic: true,
                default: Some(default),
            },
        })
    }
}

impl gazelle::Action<aipl::TypeParams<Self>> for Build {
    fn build(&mut self, node: aipl::TypeParams<Self>) -> Result<Vec<TypeParam>, Self::Error> {
        Ok(match node {
            aipl::TypeParams::Present(list) => list,
            aipl::TypeParams::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::TypeParamList<Self>> for Build {
    fn build(&mut self, node: aipl::TypeParamList<Self>) -> Result<Vec<TypeParam>, Self::Error> {
        Ok(match node {
            aipl::TypeParamList::First(p) => vec![p],
            aipl::TypeParamList::Rest(mut prev, p) => {
                prev.push(p);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::TypeParam<Self>> for Build {
    fn build(&mut self, node: aipl::TypeParam<Self>) -> Result<TypeParam, Self::Error> {
        // `name : bound` — the bound names a constraint on `name` (`any`
        // accepts everything; `ord` accepts comparable scalars), enforced
        // when the type variable is later resolved by a call. A bare `name`
        // defaults to the `any` bound.
        Ok(match node {
            aipl::TypeParam::Bare((name, _)) => TypeParam {
                name,
                bound: Bound::Any,
            },
            aipl::TypeParam::VariantBound((name, _)) => TypeParam {
                name,
                bound: Bound::Variant,
            },
            aipl::TypeParam::TypeParam((name, _), (bound_name, bound_span)) => {
                let bound = Bound::from_name(&bound_name).ok_or_else(|| {
                    Error::at(
                        format!(
                            "unknown type parameter bound {bound_name:?}; expected \"any\" or \"ord\""
                        ),
                        bound_span,
                    )
                })?;
                TypeParam { name, bound }
            }
        })
    }
}

impl gazelle::Action<aipl::ReturnTy<Self>> for Build {
    fn build(&mut self, node: aipl::ReturnTy<Self>) -> Result<Option<Type>, Self::Error> {
        Ok(match node {
            aipl::ReturnTy::Present(t) => Some(t),
            aipl::ReturnTy::Absent => None,
        })
    }
}

impl gazelle::Action<aipl::Ty<Self>> for Build {
    fn build(&mut self, node: aipl::Ty<Self>) -> Result<Type, Self::Error> {
        Ok(match node {
            aipl::Ty::Base(t) => t,
            aipl::Ty::Result(ok, err) => Type::Result(Box::new(ok), Box::new(err)),
            aipl::Ty::ResultVoid(err) => Type::Result(Box::new(Type::Unit), Box::new(err)),
            aipl::Ty::FnTy(params, ret) => Type::Fn(params, Box::new(ret)),
            aipl::Ty::TupleTy(args) => {
                if args.len() < 2 {
                    return Err(Error::msg(
                        "a tuple type needs at least 2 elements, e.g. (i64, str)".to_string(),
                    ));
                }
                Type::Tuple(args)
            }
            aipl::Ty::TupleArrayTy(args, _rbracket) => {
                if args.len() < 2 {
                    return Err(Error::msg(
                        "a tuple type needs at least 2 elements, e.g. (i64, str)[]".to_string(),
                    ));
                }
                Type::Array(Box::new(Type::Tuple(args)))
            }
            aipl::Ty::TupleResult(args, err) => {
                if args.len() < 2 {
                    return Err(Error::msg(
                        "a tuple type needs at least 2 elements, e.g. (i64, str)!Error".to_string(),
                    ));
                }
                Type::Result(Box::new(Type::Tuple(args)), Box::new(err))
            }
        })
    }
}

impl gazelle::Action<aipl::BaseTy<Self>> for Build {
    fn build(&mut self, node: aipl::BaseTy<Self>) -> Result<Type, Self::Error> {
        Ok(match node {
            // A base-type identifier is a primitive (`i64`, `bool`, `str`, …),
            // the anonymous generic bound `any`, or a non-primitive name
            // (struct/variant/generic-param/`Error`).
            aipl::BaseTy::Named((name, _)) => match aipl_syntax::ast::Primitive::from_name(&name) {
                Some(p) => Type::Primitive(p),
                None if name == "any" => Type::Any,
                None => Type::Named(name),
            },
            // `Foo<A, B>` — a generic type application. The base name is always
            // a user struct/variant template (never a primitive), resolved to a
            // synthetic monomorphic type by mono's `lower_generics` pre-pass.
            aipl::BaseTy::Generic((name, _), args) => Type::Generic(name, args),
            aipl::BaseTy::Optional(inner) => Type::Optional(Box::new(inner)),
            aipl::BaseTy::Array(inner, _rbracket) => Type::Array(Box::new(inner)),
            aipl::BaseTy::Set(_hash, inner) => Type::Set(Box::new(inner)),
            aipl::BaseTy::Dict(_hash, k, v) => Type::Dict(Box::new(k), Box::new(v)),
        })
    }
}

impl gazelle::Action<aipl::TyArgs<Self>> for Build {
    fn build(&mut self, node: aipl::TyArgs<Self>) -> Result<Vec<Type>, Self::Error> {
        Ok(match node {
            aipl::TyArgs::Present(list) => list,
            aipl::TyArgs::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::TyArgList<Self>> for Build {
    fn build(&mut self, node: aipl::TyArgList<Self>) -> Result<Vec<Type>, Self::Error> {
        Ok(match node {
            aipl::TyArgList::First(t) => vec![t],
            aipl::TyArgList::Rest(mut prev, t) => {
                prev.push(t);
                prev
            }
        })
    }
}

/// Fold a statement list and a tail expression into a single nested
/// expression chain (the tail being the block's value).
///
/// Fold right: each stmt wraps the accumulated tail in the appropriate
/// ExprKind, so the final expression is the tail of the body chain.
///
/// for-stmts have no "tail value" — they're folded as
/// `Let("_", For(...), acc)` so the loop's iconst-0 result is discarded
/// into a phantom binding while the after-loop chain continues unchanged.
fn wrap_stmt(stmt: StmtSpec, acc: Expr) -> Expr {
    match stmt {
        StmtSpec::Let {
            name,
            name_span,
            ty,
            value,
            ..
        } => {
            let span = join_spans(&name_span, &acc.span);
            Expr::new(
                ExprKind::Let(name, ty, Box::new(value), Box::new(acc)),
                span,
            )
        }
        StmtSpec::Mut {
            name,
            name_span,
            ty,
            value,
            ..
        } => {
            let span = join_spans(&name_span, &acc.span);
            Expr::new(
                ExprKind::LetMut(name, ty, Box::new(value), Box::new(acc)),
                span,
            )
        }
        StmtSpec::Assign { lhs, value, span } => {
            // The statement's own span (LHS through value), not the usual join
            // with the rest of the block: errors against this node (an
            // immutable target, an unknown field) should point at the
            // statement, not at whatever follows it.
            Expr::new(
                ExprKind::Assign(Box::new(lhs), Box::new(value), Box::new(acc)),
                span,
            )
        }
        StmtSpec::For {
            var,
            var_span,
            index,
            iterable,
            body,
            span: for_span,
        } => {
            // `for (let i, x : iter) { body }` desugars to a plain loop over a
            // counter declared just outside it:
            //   mut __idx$N: u64 = 0;
            //   for (let x : iter) { let i = __idx$N; body; set __idx$N = __idx$N + 1; }
            // The counter is introduced here rather than in the action because
            // it has to scope over the rest of the block (`acc`) — and being
            // re-declared per entry is what resets it when the loop is nested
            // inside another. `i` is re-bound immutably each iteration, so the
            // body can't move the counter out from under the loop.
            //
            // The increment calls the canonical builtin rather than emitting a
            // `+`: operator *usage* is gated per file against its imports, and
            // a loop should not oblige the file to import an operator it never
            // wrote.
            let Some(index) = index else {
                let for_expr = Expr::new(
                    ExprKind::For(var, Box::new(iterable), Box::new(body)),
                    for_span,
                );
                let span = join_spans(&var_span, &acc.span);
                return Expr::new(ExprKind::Seq(Box::new(for_expr), Box::new(acc)), span);
            };
            let tmp = format!("__idx${}", iterable.span.start);
            let sp = var_span.clone();
            let counter = || Expr::new(ExprKind::Ident(tmp.clone()), sp.clone());
            let bump = Expr::new(
                ExprKind::Assign(
                    Box::new(counter()),
                    Box::new(Expr::new(
                        ExprKind::Call(
                            "__builtin_wrapping_add".to_string(),
                            vec![counter(), Expr::new(ExprKind::Num(1), sp.clone())],
                            false,
                        ),
                        sp.clone(),
                    )),
                    Box::new(Expr::new(ExprKind::Unit, sp.clone())),
                ),
                sp.clone(),
            );
            let body_span = body.span.clone();
            let counted = Expr::new(
                ExprKind::Seq(Box::new(body), Box::new(bump)),
                body_span.clone(),
            );
            let bound = Expr::new(
                ExprKind::Let(index, None, Box::new(counter()), Box::new(counted)),
                body_span,
            );
            let for_expr = Expr::new(
                ExprKind::For(var, Box::new(iterable), Box::new(bound)),
                for_span,
            );
            let span = join_spans(&var_span, &acc.span);
            let rest = Expr::new(
                ExprKind::Seq(Box::new(for_expr), Box::new(acc)),
                span.clone(),
            );
            Expr::new(
                ExprKind::LetMut(
                    tmp,
                    Some(Type::Primitive(Primitive::U64)),
                    Box::new(Expr::new(ExprKind::Num(0), sp)),
                    Box::new(rest),
                ),
                span,
            )
        }
        StmtSpec::While {
            cond,
            body,
            span: while_span,
        } => {
            let span = join_spans(&while_span, &acc.span);
            let while_expr = Expr::new(ExprKind::While(Box::new(cond), Box::new(body)), while_span);
            Expr::new(ExprKind::Seq(Box::new(while_expr), Box::new(acc)), span)
        }
        StmtSpec::LetTuple {
            names,
            value,
            span: tup_span,
        } => {
            let tmp = format!("__tpat${}", tup_span.start);
            // Wrap the rest of the block with field-access bindings (innermost last).
            let mut result = acc;
            for (i, name) in names.iter().enumerate().rev() {
                let tmp_ident = Expr::new(ExprKind::Ident(tmp.clone()), tup_span.clone());
                let field = Expr::new(
                    ExprKind::Field(Box::new(tmp_ident), format!("_{i}")),
                    tup_span.clone(),
                );
                let inner_span = join_spans(&tup_span, &result.span);
                result = Expr::new(
                    ExprKind::Let(name.clone(), None, Box::new(field), Box::new(result)),
                    inner_span,
                );
            }
            let outer_span = join_spans(&tup_span, &result.span);
            Expr::new(
                ExprKind::Let(tmp, None, Box::new(value), Box::new(result)),
                outer_span,
            )
        }
        StmtSpec::LetStruct {
            struct_name: _,
            fields,
            value,
            span: struct_span,
        } => {
            let tmp = format!("__spat${}", struct_span.start);
            let mut result = acc;
            for field_name in fields.iter().rev() {
                let tmp_ident = Expr::new(ExprKind::Ident(tmp.clone()), struct_span.clone());
                let field = Expr::new(
                    ExprKind::Field(Box::new(tmp_ident), field_name.clone()),
                    struct_span.clone(),
                );
                let inner_span = join_spans(&struct_span, &result.span);
                result = Expr::new(
                    ExprKind::Let(field_name.clone(), None, Box::new(field), Box::new(result)),
                    inner_span,
                );
            }
            let outer_span = join_spans(&struct_span, &result.span);
            Expr::new(
                ExprKind::Let(tmp, None, Box::new(value), Box::new(result)),
                outer_span,
            )
        }
        StmtSpec::Return {
            value,
            span: ret_span,
        } => {
            let span = join_spans(&ret_span, &acc.span);
            let ret_expr = Expr::new(ExprKind::Return(Box::new(value)), ret_span);
            // `acc` (the rest of the block) is unreachable after a return, but it's
            // kept in the tree so the checker/codegen see a well-formed block.
            Expr::new(ExprKind::Seq(Box::new(ret_expr), Box::new(acc)), span)
        }
    }
}

impl gazelle::Action<aipl::Block<Self>> for Build {
    fn build(&mut self, node: aipl::Block<Self>) -> Result<Expr, Self::Error> {
        let aipl::Block::Block(body) = node;
        Ok(body)
    }
}

impl gazelle::Action<aipl::ElseBranch<Self>> for Build {
    fn build(&mut self, node: aipl::ElseBranch<Self>) -> Result<Expr, Self::Error> {
        Ok(match node {
            // `else { .. }` — the else branch is just its block.
            aipl::ElseBranch::Plain(block) => block,
            // `else if (..) { .. } else ..` — a nested if in the else position,
            // built identically to `Atom::IfElse`.
            aipl::ElseBranch::Elif(cond, then_b, else_b) => {
                let span = join_spans(&cond.span, &else_b.span);
                Expr::new(
                    ExprKind::If(Box::new(cond), Box::new(then_b), Box::new(else_b)),
                    span,
                )
            }
            // `else if (..) { .. }` with no trailing else — a nested else-less
            // if (synthetic unit else), built identically to `Atom::IfNoElse`.
            aipl::ElseBranch::ElifNoElse(cond, then_b) => {
                let span = join_spans(&cond.span, &then_b.span);
                let else_b = Expr::new(ExprKind::Unit, span.clone());
                Expr::new(
                    ExprKind::If(Box::new(cond), Box::new(then_b), Box::new(else_b)),
                    span,
                )
            }
        })
    }
}

impl gazelle::Action<aipl::BlockBody<Self>> for Build {
    fn build(&mut self, node: aipl::BlockBody<Self>) -> Result<Expr, Self::Error> {
        Ok(match node {
            // Empty / nothing left → the block's value is unit.
            aipl::BlockBody::Empty => Expr::new(ExprKind::Unit, 0..0),
            // A leading expression: either the block's trailing value, or
            // `expr;` (discard via `Seq`) followed by the rest of the block.
            aipl::BlockBody::HeadExpr(expr, tail) => match tail {
                BlockTail::Value => expr,
                BlockTail::Discard(rest) => {
                    let span = join_spans(&expr.span, &rest.span);
                    Expr::new(ExprKind::Seq(Box::new(expr), Box::new(rest)), span)
                }
            },
            aipl::BlockBody::HeadStmt(stmt, rest) => wrap_stmt(stmt, rest),
        })
    }
}

impl gazelle::Action<aipl::BlockTail<Self>> for Build {
    fn build(&mut self, node: aipl::BlockTail<Self>) -> Result<BlockTail, Self::Error> {
        Ok(match node {
            aipl::BlockTail::Value => BlockTail::Value,
            aipl::BlockTail::Discard(rest) => BlockTail::Discard(rest),
        })
    }
}

impl gazelle::Action<aipl::LoopBody<Self>> for Build {
    fn build(&mut self, node: aipl::LoopBody<Self>) -> Result<Expr, Self::Error> {
        let aipl::LoopBody::LoopBody(inner) = node;
        Ok(inner)
    }
}

impl gazelle::Action<aipl::LoopInner<Self>> for Build {
    fn build(&mut self, node: aipl::LoopInner<Self>) -> Result<Expr, Self::Error> {
        Ok(match node {
            // The loop discards its body value, so an at-end body is a
            // synthetic `0` (matching the loop expression's own i64 0 result).
            aipl::LoopInner::Empty => Expr::new(ExprKind::Num(0), 0..0),
            aipl::LoopInner::ExprSeq(expr, rest) => {
                let span = join_spans(&expr.span, &rest.span);
                Expr::new(ExprKind::Seq(Box::new(expr), Box::new(rest)), span)
            }
            aipl::LoopInner::StmtSeq(stmt, rest) => wrap_stmt(stmt, rest),
        })
    }
}

impl gazelle::Action<aipl::KwStmt<Self>> for Build {
    fn build(&mut self, node: aipl::KwStmt<Self>) -> Result<StmtSpec, Self::Error> {
        Ok(match node {
            aipl::KwStmt::LetStmt(s) => s,
            aipl::KwStmt::LetTupleStmt(s) => s,
            aipl::KwStmt::LetStructStmt(s) => s,
            aipl::KwStmt::MutStmt(s) => s,
            aipl::KwStmt::AssignStmt(s) => s,
            aipl::KwStmt::ForStmt(s) => s,
            aipl::KwStmt::ForTupleStmt(s) => s,
            aipl::KwStmt::ForIndexedStmt(s) => s,
            aipl::KwStmt::WhileStmt(s) => s,
            aipl::KwStmt::ReturnStmt(s) => s,
        })
    }
}

impl gazelle::Action<aipl::LetStmt<Self>> for Build {
    fn build(&mut self, node: aipl::LetStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let (name, name_span, ty, value) = match node {
            aipl::LetStmt::LetStmt((name, name_span), value) => (name, name_span, None, value),
            aipl::LetStmt::LetTyStmt((name, name_span), ty, value) => {
                (name, name_span, Some(ty), value)
            }
        };
        let span = join_spans(&name_span, &value.span);
        Ok(StmtSpec::Let {
            name,
            name_span,
            ty,
            value,
            span,
        })
    }
}

impl gazelle::Action<aipl::LetTupleStmt<Self>> for Build {
    fn build(&mut self, node: aipl::LetTupleStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let aipl::LetTupleStmt::LetTupleStmt(names, value) = node;
        if names.len() < 2 {
            return Err(Error::msg(
                "a tuple pattern needs at least 2 names, e.g. let (a, b) = expr;".to_string(),
            ));
        }
        let span = value.span.clone();
        Ok(StmtSpec::LetTuple { names, value, span })
    }
}

impl gazelle::Action<aipl::LetStructStmt<Self>> for Build {
    fn build(&mut self, node: aipl::LetStructStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let aipl::LetStructStmt::LetStructStmt((struct_name, _), fields, value) = node;
        let span = value.span.clone();
        Ok(StmtSpec::LetStruct {
            struct_name,
            fields,
            value,
            span,
        })
    }
}

impl gazelle::Action<aipl::StructFieldBindings<Self>> for Build {
    fn build(&mut self, node: aipl::StructFieldBindings<Self>) -> Result<Vec<String>, Self::Error> {
        Ok(match node {
            aipl::StructFieldBindings::Present(list)
            | aipl::StructFieldBindings::PresentTrailing(list) => list,
        })
    }
}

impl gazelle::Action<aipl::StructFieldBindingList<Self>> for Build {
    fn build(
        &mut self,
        node: aipl::StructFieldBindingList<Self>,
    ) -> Result<Vec<String>, Self::Error> {
        Ok(match node {
            aipl::StructFieldBindingList::First((s, _)) => vec![s],
            aipl::StructFieldBindingList::Rest(mut prev, (s, _)) => {
                prev.push(s);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::MutStmt<Self>> for Build {
    fn build(&mut self, node: aipl::MutStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let (name, name_span, ty, value) = match node {
            aipl::MutStmt::MutStmt((name, name_span), value) => (name, name_span, None, value),
            aipl::MutStmt::MutTyStmt((name, name_span), ty, value) => {
                (name, name_span, Some(ty), value)
            }
        };
        let span = join_spans(&name_span, &value.span);
        Ok(StmtSpec::Mut {
            name,
            name_span,
            ty,
            value,
            span,
        })
    }
}

impl gazelle::Action<aipl::AssignStmt<Self>> for Build {
    fn build(&mut self, node: aipl::AssignStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let (lhs, value, span) = match node {
            aipl::AssignStmt::AssignStmt((name, name_span), value) => {
                let span = join_spans(&name_span, &value.span);
                let lhs = Expr::new(ExprKind::Ident(name), name_span);
                (lhs, value, span)
            }
            // `set n++;` is `set n = n ++ 1;`, where `++` is its own operator
            // (encoded `'P'`, gated on importing `++`). The loader collapses it
            // to a plain `+`/`wrapping_add` after gating, so codegen never sees
            // `'P'`. The `1` and operator carry the `++` span so diagnostics
            // (a missing `++` import, or a non-integer `n`) point at the operator.
            aipl::AssignStmt::IncrStmt((name, name_span), pp_span) => {
                let span = join_spans(&name_span, &pp_span);
                let recv = Expr::new(ExprKind::Ident(name.clone()), name_span.clone());
                let one = Expr::new(ExprKind::Num(1), pp_span);
                let value = Expr::new(
                    ExprKind::Binop(Box::new(recv), 'P', Box::new(one)),
                    span.clone(),
                );
                let lhs = Expr::new(ExprKind::Ident(name), name_span);
                (lhs, value, span)
            }
            // `set recv.method(args);` — the writeback form of a mutating method
            // call, desugared to `set recv = recv.method(args)`. The receiver is
            // folded in as `args[0]` and the call is flagged method-style, exactly
            // as `recv.method(args)` parses in expression position; the enclosing
            // `set` is what makes it write the (mutated) result back into `recv`.
            aipl::AssignStmt::SetCallStmt((recv, recv_span), (method, method_span), args) => {
                reject_spread(&args, "a method call")?;
                let last = args.last().map(|a| a.span.clone()).unwrap_or(method_span);
                let span = join_spans(&recv_span, &last);
                let mut all = Vec::with_capacity(args.len() + 1);
                all.push(Expr::new(ExprKind::Ident(recv.clone()), recv_span.clone()));
                all.extend(args);
                let value = Expr::new(ExprKind::Call(method, all, true), span.clone());
                let lhs = Expr::new(ExprKind::Ident(recv), recv_span);
                (lhs, value, span)
            }
            // `set recv.f.g = expr;` — a field update of a mut struct binding.
            // The LHS becomes a `Field` chain, exactly as `recv.f.g` parses in
            // expression position. Not desugared here: the rewrite to nested
            // constructs needs the structs' field lists, which only mono's
            // `infer` knows.
            aipl::AssignStmt::SetFieldStmt((recv, recv_span), path, value) => {
                let span = join_spans(&recv_span, &value.span);
                let mut lhs = Expr::new(ExprKind::Ident(recv), recv_span.clone());
                for (field, field_span) in path {
                    let fspan = join_spans(&recv_span, &field_span);
                    lhs = Expr::new(ExprKind::Field(Box::new(lhs), field), fspan);
                }
                (lhs, value, span)
            }
        };
        Ok(StmtSpec::Assign { lhs, value, span })
    }
}

impl gazelle::Action<aipl::FieldPath<Self>> for Build {
    fn build(&mut self, node: aipl::FieldPath<Self>) -> Result<Vec<(String, Span)>, Self::Error> {
        Ok(match node {
            aipl::FieldPath::First(id) => vec![id],
            aipl::FieldPath::Rest(mut prev, id) => {
                prev.push(id);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::ForStmt<Self>> for Build {
    fn build(&mut self, node: aipl::ForStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let aipl::ForStmt::ForStmt((var, var_span), iterable, body) = node;
        let span = join_spans(&var_span, &body.span);
        Ok(StmtSpec::For {
            var,
            var_span,
            index: None,
            iterable,
            body,
            span,
        })
    }
}

impl gazelle::Action<aipl::ForTupleStmt<Self>> for Build {
    fn build(&mut self, node: aipl::ForTupleStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let aipl::ForTupleStmt::ForTupleStmt(names, iterable, body) = node;
        if names.len() < 2 {
            return Err(Error::msg(
                "a tuple pattern needs at least 2 names, e.g. for (let (a, b) : expr) { ... }"
                    .to_string(),
            ));
        }
        // Desugar `for (let (a, b) : iter) { body }` into a plain for loop with a
        // synthetic temp var and field-access bindings prepended to the body:
        //   for (let __fpat$N : iter) { let a = __fpat$N._0; let b = __fpat$N._1; body }
        let tmp = format!("__fpat${}", iterable.span.start);
        let tmp_span = iterable.span.clone();
        let mut new_body = body;
        for (i, name) in names.iter().enumerate().rev() {
            let tmp_ident = Expr::new(ExprKind::Ident(tmp.clone()), tmp_span.clone());
            let field = Expr::new(
                ExprKind::Field(Box::new(tmp_ident), format!("_{i}")),
                tmp_span.clone(),
            );
            let inner_span = join_spans(&tmp_span, &new_body.span);
            new_body = Expr::new(
                ExprKind::Let(name.clone(), None, Box::new(field), Box::new(new_body)),
                inner_span,
            );
        }
        let span = join_spans(&tmp_span, &new_body.span);
        Ok(StmtSpec::For {
            var: tmp,
            var_span: tmp_span,
            index: None,
            iterable,
            body: new_body,
            span,
        })
    }
}

impl gazelle::Action<aipl::ForIndexedStmt<Self>> for Build {
    fn build(&mut self, node: aipl::ForIndexedStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let aipl::ForIndexedStmt::ForIndexedStmt((index, index_span), (var, _), iterable, body) =
            node;
        let span = join_spans(&index_span, &body.span);
        Ok(StmtSpec::For {
            var,
            var_span: index_span,
            index: Some(index),
            iterable,
            body,
            span,
        })
    }
}

impl gazelle::Action<aipl::WhileStmt<Self>> for Build {
    fn build(&mut self, node: aipl::WhileStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let aipl::WhileStmt::WhileStmt(cond, body) = node;
        // No `while`/paren tokens carry spans, so span the condition through the
        // body (mirrors `for`, whose span starts at its loop variable).
        let span = join_spans(&cond.span, &body.span);
        Ok(StmtSpec::While { cond, body, span })
    }
}

impl gazelle::Action<aipl::ReturnStmt<Self>> for Build {
    fn build(&mut self, node: aipl::ReturnStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let aipl::ReturnStmt::ReturnStmt(value) = node;
        let span = value.span.clone();
        Ok(StmtSpec::Return { value, span })
    }
}

impl gazelle::Action<aipl::Binop<Self>> for Build {
    fn build(&mut self, node: aipl::Binop<Self>) -> Result<char, Self::Error> {
        Ok(match node {
            aipl::Binop::Op((c, _span)) => c,
            aipl::Binop::Minus => '-',
            aipl::Binop::Lt => '<',
            aipl::Binop::Gt => '>',
            aipl::Binop::Or => 'O',
        })
    }
}

impl gazelle::Action<aipl::Term<Self>> for Build {
    fn build(&mut self, node: aipl::Term<Self>) -> Result<Expr, Self::Error> {
        let aipl::Term::Unary(e) = node;
        Ok(e)
    }
}

impl gazelle::Action<aipl::Unary<Self>> for Build {
    fn build(&mut self, node: aipl::Unary<Self>) -> Result<Expr, Self::Error> {
        Ok(match node {
            aipl::Unary::Neg(e) => {
                let span = e.span.clone();
                Expr::new(ExprKind::Neg(Box::new(e)), span)
            }
            aipl::Unary::Not(e) => {
                let span = e.span.clone();
                Expr::new(ExprKind::Not(Box::new(e)), span)
            }
            aipl::Unary::Postfix(e) => e,
        })
    }
}

impl gazelle::Action<aipl::Postfix<Self>> for Build {
    fn build(&mut self, node: aipl::Postfix<Self>) -> Result<Expr, Self::Error> {
        Ok(match node {
            aipl::Postfix::Atom(e) => e,
            aipl::Postfix::FieldAccess(obj, (name, name_span)) => {
                let span = join_spans(&obj.span, &name_span);
                Expr::new(ExprKind::Field(Box::new(obj), name), span)
            }
            aipl::Postfix::TupleIndex(obj, (n, n_span)) => {
                let span = join_spans(&obj.span, &n_span);
                if n < 0 {
                    return Err(Error::at(
                        "tuple index must be a non-negative integer".to_string(),
                        n_span,
                    ));
                }
                Expr::new(ExprKind::Field(Box::new(obj), format!("_{n}")), span)
            }
            aipl::Postfix::MethodCall(obj, (name, name_span), args) => {
                reject_spread(&args, "a method call")?;
                let last = args.last().map(|a| a.span.clone()).unwrap_or(name_span);
                let span = join_spans(&obj.span, &last);
                // Method call: fold the receiver in as `args[0]` and flag the
                // method form. `recv.f(a, b)` is stored as `f(recv, a, b)`.
                let mut all = Vec::with_capacity(args.len() + 1);
                all.push(obj);
                all.extend(args);
                Expr::new(ExprKind::Call(name, all, true), span)
            }
            aipl::Postfix::Index(obj, _lbracket, index) => {
                let span = join_spans(&obj.span, &index.span);
                // `recv[a..b]` — a range literal in index position is the
                // closed slice form; fold it straight into a `Slice` node
                // (identical to the historic dedicated production) rather
                // than constructing a `Span` only to unpack it at runtime.
                // A range that reaches indexing as a *value* (`recv[s]`, a
                // call result, ...) stays an `Index` and takes the Span-index
                // sugar path instead.
                if let ExprKind::Construct(name, inits) = &index.kind {
                    if name == "__builtin_Span" && inits.len() == 2 {
                        let start = inits[0].value.clone();
                        let end = inits[1].value.clone();
                        return Ok(Expr::new(
                            ExprKind::Slice(Box::new(obj), Box::new(start), Some(Box::new(end))),
                            span,
                        ));
                    }
                }
                Expr::new(ExprKind::Index(Box::new(obj), Box::new(index)), span)
            }
            // `recv[start..]` — open end (runs to the receiver's length).
            aipl::Postfix::SliceOpen(obj, _lbracket, start) => {
                let span = join_spans(&obj.span, &start.span);
                Expr::new(ExprKind::Slice(Box::new(obj), Box::new(start), None), span)
            }
            // `recv[..end]` — open start. Semantically `recv[0..end]`, so we
            // synthesize a `0` start literal; it flows through check/codegen
            // unchanged (the start clamps to `[0, len]` regardless).
            aipl::Postfix::SliceTo(obj, _lbracket, end) => {
                let span = join_spans(&obj.span, &end.span);
                let start = Expr::new(ExprKind::Num(0), 0..0);
                Expr::new(
                    ExprKind::Slice(Box::new(obj), Box::new(start), Some(Box::new(end))),
                    span,
                )
            }
            aipl::Postfix::TryOp(obj) => {
                let span = obj.span.clone();
                Expr::new(ExprKind::Try(Box::new(obj)), span)
            }
        })
    }
}

impl gazelle::Action<aipl::Atom<Self>> for Build {
    fn build(&mut self, node: aipl::Atom<Self>) -> Result<Expr, Self::Error> {
        Ok(match node {
            aipl::Atom::Num((n, span)) => Expr::new(ExprKind::Num(n), span),
            aipl::Atom::TrueLit(span) => Expr::new(ExprKind::Bool(true), span),
            aipl::Atom::FalseLit(span) => Expr::new(ExprKind::Bool(false), span),
            aipl::Atom::StringLit((s, span)) => Expr::new(ExprKind::Str(s), span),
            aipl::Atom::CharLit((b, span)) => Expr::new(ExprKind::Char(b), span),
            aipl::Atom::Ident((s, span)) => Expr::new(ExprKind::Ident(s), span),
            aipl::Atom::Call((name, name_span), args) => {
                reject_spread(&args, "a call argument list")?;
                let span = match args.last() {
                    Some(a) => join_spans(&name_span, &a.span),
                    None => name_span,
                };
                Expr::new(ExprKind::Call(name, args, false), span)
            }
            aipl::Atom::Construct((name, name_span), fields) => {
                let span = match fields.last() {
                    Some(f) => join_spans(&name_span, &f.value.span),
                    None => name_span,
                };
                Expr::new(ExprKind::Construct(name, fields), span)
            }
            aipl::Atom::Paren(e) => e,
            aipl::Atom::TupleLit(first, rest) => {
                let last_span = rest
                    .last()
                    .map(|e| e.span.clone())
                    .unwrap_or_else(|| first.span.clone());
                let span = join_spans(&first.span, &last_span);
                let mut elems = Vec::with_capacity(1 + rest.len());
                elems.push(first);
                elems.extend(rest);
                Expr::new(ExprKind::TupleLit(elems), span)
            }
            aipl::Atom::IfElse(cond, then_b, else_b) => {
                let span = join_spans(&cond.span, &else_b.span);
                Expr::new(
                    ExprKind::If(Box::new(cond), Box::new(then_b), Box::new(else_b)),
                    span,
                )
            }
            // Else-less `if`: a synthetic unit `else`, so it's typed (and lowered)
            // exactly like `if (..) { .. } else {}` — unit-valued, used in
            // statement position.
            aipl::Atom::IfNoElse(cond, then_b) => {
                let span = join_spans(&cond.span, &then_b.span);
                let else_b = Expr::new(ExprKind::Unit, span.clone());
                Expr::new(
                    ExprKind::If(Box::new(cond), Box::new(then_b), Box::new(else_b)),
                    span,
                )
            }
            // A lambda in value position with a brace-delimited body
            // (`let f = |x: i64| { x + 1 }`). Built like an argument-position
            // lambda.
            aipl::Atom::LambdaBlock(pipe_span, params, _pipe2, body) => {
                let span = join_spans(&pipe_span, &body.span);
                Expr::new(ExprKind::Lambda(params, Box::new(body)), span)
            }
            aipl::Atom::NoneLit(span) => Expr::new(ExprKind::None, span),
            aipl::Atom::MatchExpr(scrutinee, arms) => {
                let last_span = arms
                    .last()
                    .map(|a| a.span.clone())
                    .unwrap_or_else(|| scrutinee.span.clone());
                let span = join_spans(&scrutinee.span, &last_span);
                Expr::new(ExprKind::Match(Box::new(scrutinee), arms), span)
            }
            aipl::Atom::ShimExpr(shim_span, (effect, effect_span), bindings, body) => {
                // `shim <effect>` — not the body, whose span is empty when it
                // ends in a `;` statement.
                let span = join_spans(&shim_span, &effect_span);
                Expr::new(ExprKind::Shim(effect, bindings, Box::new(body)), span)
            }
            aipl::Atom::ArrayLit(lbracket_span, elems) => {
                // Span runs from `[` to the last element (or just the
                // `[` for an empty literal).
                let span = match elems.last() {
                    Some(e) => join_spans(&lbracket_span, &e.span),
                    None => lbracket_span,
                };
                Expr::new(ExprKind::ArrayLit(elems), span)
            }
            // `#{ .. }` — a set or dict literal (or an empty of either). Span
            // runs from `#` to the last element/value (or just `#` for an
            // empty), like an array literal.
            aipl::Atom::BraceLit(hash_span, brace) => match brace {
                BraceLit::EmptySet => Expr::new(ExprKind::SetLit(Vec::new()), hash_span),
                BraceLit::EmptyDict => Expr::new(ExprKind::DictLit(Vec::new()), hash_span),
                BraceLit::Entries(entries) => {
                    let has_pair = entries
                        .iter()
                        .any(|e| matches!(e, BraceEntry::KeyValue(..)));
                    let has_bare = entries.iter().any(|e| matches!(e, BraceEntry::KeyOnly(..)));
                    if has_pair && has_bare {
                        return Err(Error::at(
                            "a \"#{ .. }\" literal can't mix set elements and \"key: value\" \
                             pairs \u{2014} use either all bare elements (a set) or all pairs (a dict)"
                                .to_string(),
                            hash_span,
                        ));
                    }
                    if has_pair {
                        let pairs: Vec<(Expr, Expr)> = entries
                            .into_iter()
                            .map(|e| match e {
                                BraceEntry::KeyValue(k, v) => (k, v),
                                BraceEntry::KeyOnly(_) => unreachable!("checked above"),
                            })
                            .collect();
                        let span = match pairs.last() {
                            Some((_, v)) => join_spans(&hash_span, &v.span),
                            None => hash_span,
                        };
                        Expr::new(ExprKind::DictLit(pairs), span)
                    } else {
                        let elems: Vec<Expr> = entries
                            .into_iter()
                            .map(|e| match e {
                                BraceEntry::KeyOnly(k) => k,
                                BraceEntry::KeyValue(..) => unreachable!("checked above"),
                            })
                            .collect();
                        let span = match elems.last() {
                            Some(e) => join_spans(&hash_span, &e.span),
                            None => hash_span,
                        };
                        Expr::new(ExprKind::SetLit(elems), span)
                    }
                }
            },
            // `` `text {e1} text {e2} text` `` desugars to a chain of
            // `__aipl_concat` / `__builtin_to_str` calls (left-folded, but
            // LALR reduces right-to-left so `rest` is already built).
            aipl::Atom::TemplateLit((head_text, head_span), first_expr, rest) => {
                let head_node = Expr::new(ExprKind::Str(head_text), head_span.clone());
                let e1_str = to_str_call(first_expr);
                let left = concat_call(head_node, e1_str);
                let full_span = join_spans(&head_span, &rest.span);
                let result = concat_call(left, rest);
                Expr::new(result.kind, full_span)
            }
        })
    }
}

impl gazelle::Action<aipl::TemplateRest<Self>> for Build {
    fn build(&mut self, node: aipl::TemplateRest<Self>) -> Result<Expr, Self::Error> {
        Ok(match node {
            aipl::TemplateRest::Tail((text, span)) => Expr::new(ExprKind::Str(text), span),
            aipl::TemplateRest::Middle((text, span), e, rest) => {
                let text_node = Expr::new(ExprKind::Str(text), span.clone());
                let e_str = to_str_call(e);
                let left = concat_call(text_node, e_str);
                concat_call(left, rest)
            }
        })
    }
}

/// Wrap `e` in a `__template_interp` call: passes `str` through unchanged,
/// converts any other type via `to_str` (without adding surrounding quotes).
fn to_str_call(e: Expr) -> Expr {
    let span = e.span.clone();
    Expr::new(
        ExprKind::Call("__template_interp".to_string(), vec![e], false),
        span,
    )
}

/// Concatenate two `str` expressions via `__aipl_concat`.
fn concat_call(a: Expr, b: Expr) -> Expr {
    let span = join_spans(&a.span, &b.span);
    Expr::new(
        ExprKind::Call("__aipl_concat".to_string(), vec![a, b], false),
        span,
    )
}

impl gazelle::Action<aipl::TupleMore<Self>> for Build {
    fn build(&mut self, node: aipl::TupleMore<Self>) -> Result<Vec<Expr>, Self::Error> {
        Ok(match node {
            aipl::TupleMore::Single(e) => vec![e],
            aipl::TupleMore::More(mut prev, e) => {
                prev.push(e);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::ShimBindings<Self>> for Build {
    fn build(
        &mut self,
        node: aipl::ShimBindings<Self>,
    ) -> Result<Vec<(String, String)>, Self::Error> {
        Ok(match node {
            aipl::ShimBindings::Present(list) => list,
            aipl::ShimBindings::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::ShimBindingList<Self>> for Build {
    fn build(
        &mut self,
        node: aipl::ShimBindingList<Self>,
    ) -> Result<Vec<(String, String)>, Self::Error> {
        Ok(match node {
            aipl::ShimBindingList::First(b) => vec![b],
            aipl::ShimBindingList::Rest(mut list, b) => {
                list.push(b);
                list
            }
            aipl::ShimBindingList::Trailing(list) => list,
        })
    }
}

impl gazelle::Action<aipl::ShimBinding<Self>> for Build {
    fn build(&mut self, node: aipl::ShimBinding<Self>) -> Result<(String, String), Self::Error> {
        let aipl::ShimBinding::ShimBinding((op, _), (f, _)) = node;
        Ok((op, f))
    }
}

impl gazelle::Action<aipl::AltHead<Self>> for Build {
    /// One binding-free pattern inside an alternation, with its own span.
    fn build(&mut self, node: aipl::AltHead<Self>) -> Result<(Pattern, Span), Self::Error> {
        Ok(match node {
            // `_` lexes as an identifier, so it arrives here like any nullary
            // name — same as the standalone `nullary_arm` production.
            aipl::AltHead::AltNullary((name, span)) => (
                if name == "_" {
                    Pattern::Wildcard
                } else {
                    Pattern::Ctor {
                        name,
                        bindings: Vec::new(),
                    }
                },
                span,
            ),
            aipl::AltHead::AltQualified((v, span), (a, _)) => (
                Pattern::Ctor {
                    name: format!("{v}.{a}"),
                    bindings: Vec::new(),
                },
                span,
            ),
            aipl::AltHead::AltNone(span) => (
                Pattern::Ctor {
                    name: "none".to_string(),
                    bindings: Vec::new(),
                },
                span,
            ),
            aipl::AltHead::AltStr((lit, span)) => (Pattern::Str(lit), span),
            aipl::AltHead::AltChar((c, span)) => (Pattern::Char(c), span),
        })
    }
}

impl gazelle::Action<aipl::AltHeads<Self>> for Build {
    fn build(&mut self, node: aipl::AltHeads<Self>) -> Result<Vec<(Pattern, Span)>, Self::Error> {
        Ok(match node {
            // The middle field is the `|` terminal's own span, unused.
            aipl::AltHeads::AltPair(a, _, b) => vec![a, b],
            aipl::AltHeads::AltMore(mut prev, _, b) => {
                prev.push(b);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::MatchArms<Self>> for Build {
    fn build(&mut self, node: aipl::MatchArms<Self>) -> Result<Vec<MatchArm>, Self::Error> {
        Ok(match node {
            aipl::MatchArms::Present(list) | aipl::MatchArms::PresentTrailing(list) => list,
            aipl::MatchArms::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::MatchArmList<Self>> for Build {
    fn build(&mut self, node: aipl::MatchArmList<Self>) -> Result<Vec<MatchArm>, Self::Error> {
        Ok(match node {
            aipl::MatchArmList::First(a) => a,
            aipl::MatchArmList::Rest(mut prev, a) => {
                prev.extend(a);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::ArmBody<Self>> for Build {
    fn build(&mut self, node: aipl::ArmBody<Self>) -> Result<Expr, Self::Error> {
        // Both alternatives already carry an `Expr` — a block's value is the
        // expression its `block_body` folds to — so the arm body is that value
        // whichever form was written.
        Ok(match node {
            aipl::ArmBody::Expr(e) | aipl::ArmBody::Block(e) => e,
        })
    }
}

impl gazelle::Action<aipl::MatchArm<Self>> for Build {
    /// One written arm becomes one *or more* `MatchArm`s: an alternation
    /// `A | B => body` expands here into one arm per pattern, so no later stage
    /// ever sees an or-pattern. `MatchArmList` flattens the result.
    fn build(&mut self, node: aipl::MatchArm<Self>) -> Result<Vec<MatchArm>, Self::Error> {
        // `A | B | C => body`: one arm per pattern, each carrying its own
        // pattern's span so a later diagnostic points at the alternative that
        // caused it rather than at the whole group. The body is cloned per
        // pattern — for the arms this exists to collapse (`Fn | If => Keyword`)
        // that is exactly the code already written out longhand, so it costs
        // nothing; a large shared body is duplicated, which is the tradeoff.
        if let aipl::MatchArm::AltArm(heads, body) = node {
            return Ok(heads
                .into_iter()
                .map(|(pattern, span)| MatchArm {
                    pattern,
                    body: body.clone(),
                    span,
                })
                .collect());
        }
        Ok(vec![match node {
            aipl::MatchArm::CtorArm((name, span), bindings, body) => MatchArm {
                pattern: Pattern::Ctor { name, bindings },
                body,
                span,
            },
            // `V.A(b0, ..) => body` — a variant-qualified constructor pattern. The
            // dotted name is carried through as `V.A`; the loader resolves it to
            // the qualified constructor (like `V.A(..)` in expression position).
            aipl::MatchArm::QualifiedCtorArm((v, span), (a, _), bindings, body) => MatchArm {
                pattern: Pattern::Ctor {
                    name: format!("{v}.{a}"),
                    bindings,
                },
                body,
                span,
            },
            aipl::MatchArm::QualifiedNullaryArm((v, span), (a, _), body) => MatchArm {
                pattern: Pattern::Ctor {
                    name: format!("{v}.{a}"),
                    bindings: Vec::new(),
                },
                body,
                span,
            },
            // A bare identifier is a nullary constructor — except `_`, which is
            // the wildcard (it lexes as an identifier, so it arrives here).
            aipl::MatchArm::NullaryArm((name, span), body) => MatchArm {
                pattern: if name == "_" {
                    Pattern::Wildcard
                } else {
                    Pattern::Ctor {
                        name,
                        bindings: Vec::new(),
                    }
                },
                body,
                span,
            },
            aipl::MatchArm::NoneArm(span, body) => MatchArm {
                pattern: Pattern::Ctor {
                    name: "none".to_string(),
                    bindings: Vec::new(),
                },
                body,
                span,
            },
            // `"foo" => body`: a string-literal pattern (for a `str` scrutinee).
            aipl::MatchArm::CharArm((c, span), body) => MatchArm {
                pattern: Pattern::Char(c),
                span: join_spans(&span, &body.span),
                body,
            },
            aipl::MatchArm::StrArm((lit, span), body) => MatchArm {
                pattern: Pattern::Str(lit),
                body,
                span,
            },
            // `[e0, e1, ...] => body`: an array-literal pattern (for an array
            // scrutinee). The elements are validated as literals by the checker.
            aipl::MatchArm::ArrayArm(span, elems, body) => {
                reject_spread(&elems, "an array pattern")?;
                MatchArm {
                    pattern: Pattern::Array(elems),
                    body,
                    span,
                }
            }
            // Handled above, before this match.
            aipl::MatchArm::AltArm(..) => unreachable!("AltArm returns early"),
        }])
    }
}

impl gazelle::Action<aipl::MatchBindings<Self>> for Build {
    fn build(&mut self, node: aipl::MatchBindings<Self>) -> Result<Vec<String>, Self::Error> {
        let aipl::MatchBindings::Present(list) = node;
        Ok(list)
    }
}

impl gazelle::Action<aipl::BindingList<Self>> for Build {
    fn build(&mut self, node: aipl::BindingList<Self>) -> Result<Vec<String>, Self::Error> {
        Ok(match node {
            aipl::BindingList::First((s, _)) => vec![s],
            aipl::BindingList::Rest(mut prev, (s, _)) => {
                prev.push(s);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::Args<Self>> for Build {
    fn build(&mut self, node: aipl::Args<Self>) -> Result<Vec<Expr>, Self::Error> {
        Ok(match node {
            aipl::Args::Present(list) | aipl::Args::PresentTrailing(list) => list,
            aipl::Args::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::BraceBody<Self>> for Build {
    fn build(&mut self, node: aipl::BraceBody<Self>) -> Result<BraceLit, Self::Error> {
        Ok(match node {
            aipl::BraceBody::Entries(list) | aipl::BraceBody::EntriesTrailing(list) => {
                BraceLit::Entries(list)
            }
            aipl::BraceBody::EmptyDict => BraceLit::EmptyDict,
            aipl::BraceBody::EmptySet => BraceLit::EmptySet,
        })
    }
}

impl gazelle::Action<aipl::EntryList<Self>> for Build {
    fn build(&mut self, node: aipl::EntryList<Self>) -> Result<Vec<BraceEntry>, Self::Error> {
        Ok(match node {
            aipl::EntryList::First(e) => vec![e],
            aipl::EntryList::Rest(mut prev, e) => {
                prev.push(e);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::Entry<Self>> for Build {
    fn build(&mut self, node: aipl::Entry<Self>) -> Result<BraceEntry, Self::Error> {
        Ok(match node {
            aipl::Entry::KeyOnly(k) => BraceEntry::KeyOnly(k),
            aipl::Entry::KeyValue(k, v) => BraceEntry::KeyValue(k, v),
        })
    }
}

impl gazelle::Action<aipl::ArgList<Self>> for Build {
    fn build(&mut self, node: aipl::ArgList<Self>) -> Result<Vec<Expr>, Self::Error> {
        Ok(match node {
            aipl::ArgList::First(e) => vec![e],
            aipl::ArgList::Rest(mut prev, e) => {
                prev.push(e);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::Arg<Self>> for Build {
    fn build(&mut self, node: aipl::Arg<Self>) -> Result<Expr, Self::Error> {
        Ok(match node {
            aipl::Arg::Expr(e) | aipl::Arg::Lambda(e) => e,
            aipl::Arg::OpValue((c, span)) => op_value_lambda(c, span),
            // `k = expr` — a keyword argument, spanning the name through the
            // value. Resolved (and removed) by the loader's expansion.
            aipl::Arg::KwArg((name, name_span), value) => {
                let span = join_spans(&name_span, &value.span);
                Expr::new(ExprKind::KwArg(name, Box::new(value)), span)
            }
            // `..xs`. `DOTDOT` carries no span, so the node takes the operand's
            // — errors point at what is being spread. Only an array literal
            // accepts one; every other `args` consumer calls `reject_spread`.
            aipl::Arg::Spread(e) => {
                let span = e.span.clone();
                Expr::new(ExprKind::Spread(Box::new(e)), span)
            }
        })
    }
}

/// Reject a spread element that isn't a direct element of an array literal.
/// `args` is shared by calls, the mutating-writeback statement, and array
/// patterns, so the grammar admits `..x` in all of them; only the array-literal
/// builder leaves it in place.
fn reject_spread(args: &[Expr], context: &str) -> Result<(), Error> {
    match args.iter().find(|a| matches!(a.kind, ExprKind::Spread(_))) {
        Some(a) => Err(Error::at(
            format!("`..` spread is only allowed in an array literal, not in {context}"),
            a.span.clone(),
        )),
        None => Ok(()),
    }
}

/// An `OP`-token operator passed as a value (`apply(2, 3, +)`) desugars to a
/// binary lambda `|lhs, rhs| lhs <op> rhs`, reusing every lambda mechanism
/// (capture analysis — there are none — lifting, and codegen). The operator is
/// still gated like any operator use: the body's `Binop` makes the loader
/// require it to be imported, and a function-aliased operator (`my_add as +`)
/// is dispatched to a call there just as in infix position. The synthesized
/// nodes carry the operator's own span, so a "not imported" error points at it.
fn op_value_lambda(op: char, sp: Span) -> Expr {
    let lhs_param = LambdaParam {
        name: "lhs".to_string(),
        ty: None,
        span: sp.clone(),
    };
    let rhs_param = LambdaParam {
        name: "rhs".to_string(),
        ty: None,
        span: sp.clone(),
    };
    let lhs = Expr::new(ExprKind::Ident("lhs".to_string()), sp.clone());
    let rhs = Expr::new(ExprKind::Ident("rhs".to_string()), sp.clone());
    let body = Expr::new(
        ExprKind::Binop(Box::new(lhs), op, Box::new(rhs)),
        sp.clone(),
    );
    Expr::new(
        ExprKind::Lambda(vec![lhs_param, rhs_param], Box::new(body)),
        sp,
    )
}

impl gazelle::Action<aipl::Lambda<Self>> for Build {
    fn build(&mut self, node: aipl::Lambda<Self>) -> Result<Expr, Self::Error> {
        let (span, params, body) = match node {
            aipl::Lambda::LambdaExpr(pipe_span, params, _pipe2, body) => {
                (join_spans(&pipe_span, &body.span), params, body)
            }
            // `|| body` — no parameters; the `||` token carries no span, so the
            // body's span stands in for the lambda's.
            aipl::Lambda::LambdaNoargs(body) | aipl::Lambda::LambdaNoargsBlock(body) => {
                (body.span.clone(), Vec::new(), body)
            }
        };
        Ok(Expr::new(ExprKind::Lambda(params, Box::new(body)), span))
    }
}

impl gazelle::Action<aipl::LambdaParams<Self>> for Build {
    fn build(&mut self, node: aipl::LambdaParams<Self>) -> Result<Vec<LambdaParam>, Self::Error> {
        Ok(match node {
            aipl::LambdaParams::Present(list) => list,
            aipl::LambdaParams::Empty => Vec::new(),
        })
    }
}

impl gazelle::Action<aipl::LambdaParamList<Self>> for Build {
    fn build(
        &mut self,
        node: aipl::LambdaParamList<Self>,
    ) -> Result<Vec<LambdaParam>, Self::Error> {
        Ok(match node {
            aipl::LambdaParamList::First(p) => vec![p],
            aipl::LambdaParamList::Rest(mut prev, p) => {
                prev.push(p);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::LambdaParam<Self>> for Build {
    fn build(&mut self, node: aipl::LambdaParam<Self>) -> Result<LambdaParam, Self::Error> {
        Ok(match node {
            aipl::LambdaParam::Untyped((name, span)) => LambdaParam {
                name,
                ty: None,
                span,
            },
            aipl::LambdaParam::Typed((name, span), ty) => LambdaParam {
                name,
                ty: Some(ty),
                span,
            },
        })
    }
}

impl gazelle::Action<aipl::Expr<Self>> for Build {
    fn build(&mut self, node: aipl::Expr<Self>) -> Result<Expr, Self::Error> {
        Ok(match node {
            aipl::Expr::Term(t) => t,
            aipl::Expr::Binop(l, op, r) => {
                let span = join_spans(&l.span, &r.span);
                Expr::new(ExprKind::Binop(Box::new(l), op, Box::new(r)), span)
            }
            // `start..end` — a range expression is sugar for constructing the
            // builtin `Span` struct. Desugared right here (no name is written,
            // so no `Span` import is needed — matching slicing, which has
            // always used `..` unimported). A range that is *syntactically*
            // the index of `recv[...]` is folded back into a `Slice` node by
            // the `Index` builder below.
            aipl::Expr::Range(l, r) => {
                let span = join_spans(&l.span, &r.span);
                Expr::new(
                    ExprKind::Construct(
                        "__builtin_Span".to_string(),
                        vec![
                            FieldInit {
                                name: "start".to_string(),
                                value: l,
                            },
                            FieldInit {
                                name: "end".to_string(),
                                value: r,
                            },
                        ],
                    ),
                    span,
                )
            }
        })
    }
}

// Op encoding: arithmetic + logical ops use distinct codes so the binop
// reducer can pass them through as `char`.
//   '||' / '&&'   => 'O' / 'A'
//   '==' / '!='   => 'E' / 'N'
//   '<=' / '>='   => 'L' / 'G'
//   '<' / '>' / '+' / '*' / '/' keep their literal chars
// Levels, loosest to tightest:
//   1 `||`  2 `&&`  3 `==` `!=`  4 `..`  5 `<` `>` `<=` `>=`
//   6 `+` `+++` (and unary/binary `-`)  7 `*` `/` `%`
// `..` (level 4, see the `DotDot` token below) sits *above* equality so a range
// on the right of a comparison groups as one operand — `span == 1..2` is
// `span == (1..2)`, not `(span == 1)..2` — and *below* the arithmetic so
// `a + 1..b * 2` is still `(a + 1)..(b * 2)`.
fn op_precedence(c: char) -> Precedence {
    match c {
        'O' => Precedence::Left(1),
        'A' => Precedence::Left(2),
        'E' | 'N' => Precedence::Left(3),
        '<' | '>' | 'L' | 'G' => Precedence::Left(5),
        // `+` (integer add) and `'C'` (`+++`, string concat) share additive
        // precedence.
        '+' | 'C' => Precedence::Left(6),
        '*' | '/' | '%' => Precedence::Left(7),
        _ => unreachable!("unknown op code {c:?}"),
    }
}

thread_local! {
    /// When armed (by [`parse_with_allows`]), every `#[allow]` lint-squelch
    /// marker's span is recorded here. Same shape as [`COMMENT_SINK`]: plain
    /// [`parse`] leaves it disarmed (the markers only matter to the loader's
    /// lint pass, which uses `parse_with_allows`).
    static ALLOW_SINK: std::cell::RefCell<Option<Vec<Span>>> =
        const { std::cell::RefCell::new(None) };
}

/// Record an `#[allow]` marker's span into the armed sink, if any.
fn record_allow(span: Span) {
    ALLOW_SINK.with(|sink| {
        if let Some(v) = sink.borrow_mut().as_mut() {
            v.push(span);
        }
    });
}

/// The delimiter a [`LexedTokenKind::StrLit`] was written with — the mirror of
/// `lex_aipl.aipl`'s `StrStyle`. Lets a consumer (the autoformatter) recover the
/// original spelling from the decoded value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexedStrStyle {
    /// `"..."`
    Quoted,
    /// `"""..."""` (de-dented)
    TripleQuoted,
    /// `` `...` `` (interpolation-free template)
    Backtick,
    /// ```` ```...``` ```` (interpolation-free raw template, de-dented)
    TripleBacktick,
}

/// A token kind produced by the dogfooded AIPL lexer (`lex_aipl.aipl`),
/// mirrored arm-for-arm from its `AiplTok` variant so the FFI marshaling is a
/// direct name match. Value-carrying arms hold the decoded value: a `StrLit`'s
/// escape-decoded (and, for a `Triple`/`TripleBacktick` style, de-dented)
/// contents plus its delimiter style, an int literal's value, a char literal's
/// byte. The `RawTemplate*` interpolated-segment arms hold their de-dented
/// value too (their rule's `finalize` is `dedent_segments`).
/// `Space`/comments/`AllowMarker` only ever appear in [`LexedOutput::trivia`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexedTokenKind {
    Space,
    LineComment,
    BlockComment,
    AllowMarker,
    Name(String),
    IntLit(i64),
    StrLit(String, LexedStrStyle),
    CharTok(u8),
    TemplateHead(String),
    TemplateMid(String),
    TemplateTail(String),
    RawTemplateHead(String),
    RawTemplateMid(String),
    RawTemplateTail(String),
    True,
    False,
    None,
    Fn,
    Let,
    Mut,
    Set,
    Pub,
    Import,
    From,
    As,
    For,
    While,
    Match,
    Return,
    Shim,
    Struct,
    Variant,
    If,
    Else,
    Builtins,
    EqEq,
    Ne,
    Arrow,
    FatArrow,
    AndAnd,
    OrOr,
    Pipe,
    DotDot,
    PlusPlusPlus,
    PlusPlus,
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
    Bang,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Period,
    Comma,
    Colon,
    Semi,
    Question,
    Hash,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
}

/// One token from the dogfooded AIPL lexer: its [`LexedTokenKind`] and source
/// byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexedToken {
    pub kind: LexedTokenKind,
    pub span: Span,
}

/// What the dogfooded AIPL lexer returns for a whole source: the emitted
/// token stream, and the trivia side-channel (comments and `#[allow]`
/// markers, in source order — whitespace is skipped outright and appears in
/// neither).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexedOutput {
    pub tokens: Vec<LexedToken>,
    pub trivia: Vec<LexedToken>,
}

/// A hard lex error from the dogfooded AIPL lexer, with the source byte span
/// it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexedError {
    pub message: String,
    pub span: Span,
}

/// The dogfooded lexer, installed by the compiler (via [`set_lex_hook`]).
static LEX_HOOK: std::sync::OnceLock<fn(&str) -> Result<LexedOutput, LexedError>> =
    std::sync::OnceLock::new();

/// The dogfooded strip-then-lex lexer, installed via [`set_lex_stripped_hook`].
static LEX_STRIPPED_HOOK: std::sync::OnceLock<fn(&str) -> Result<LexedOutput, LexedError>> =
    std::sync::OnceLock::new();

/// Install the raw lexer hook. The compiler points this at the dogfooded AIPL
/// `lex_aipl`, run through the embedding FFI. First install wins (the hook is
/// process-global).
pub fn set_lex_hook(f: fn(&str) -> Result<LexedOutput, LexedError>) {
    let _ = LEX_HOOK.set(f);
}

/// Install the strip-then-lex hook. The compiler points this at the dogfooded
/// AIPL `lex_aipl_stripped` (which strips trailing `--- section ---` blocks
/// then lexes, both dogfooded steps in one FFI crossing). First install wins.
pub fn set_lex_stripped_hook(f: fn(&str) -> Result<LexedOutput, LexedError>) {
    let _ = LEX_STRIPPED_HOOK.set(f);
}

/// Lex `src` as-is through the installed dogfooded AIPL lexer. There is no
/// native fallback: this panics if the hook isn't installed (call
/// `install_parser_hooks` first).
pub fn lex_aipl(src: &str) -> Result<LexedOutput, LexedError> {
    let hook = LEX_HOOK
        .get()
        .expect("lex hook not installed before lexing (call install_parser_hooks)");
    hook(src)
}

/// Strip trailing `--- section ---` test blocks from `src`, then lex — through
/// the installed dogfooded AIPL `lex_aipl_stripped`. No native fallback: panics
/// if the hook isn't installed (call `install_parser_hooks` first).
pub fn lex_aipl_stripped(src: &str) -> Result<LexedOutput, LexedError> {
    let hook = LEX_STRIPPED_HOOK
        .get()
        .expect("strip-lex hook not installed before lexing (call install_parser_hooks)");
    hook(src)
}

/// If `line` is a `--- name ---` test-section marker, return the trimmed
/// inner name. Used by the cases test harness to delimit sections; the
/// compiler treats any such marker as a hard cutoff (see
/// [`strip_test_sections`]).
///
/// A line is a marker iff it starts with `---` at column 0 (no leading
/// whitespace) and, once trailing whitespace is trimmed, ends with `---`
/// with a non-empty inner segment.
///
/// The marker logic is dogfooded — the AIPL `parse_test_section_header`, run
/// through the embedding FFI via the installed hook. There is **no native
/// fallback**: it panics if the hook isn't installed, so install it (via
/// `install_parser_hooks`) before parsing. (`strip_test_sections` runs this on
/// every line of every parse, so any in-process parse needs the hook.)
pub fn parse_test_section_header(line: &str) -> Option<String> {
    let hook = TEST_SECTION_HEADER_HOOK.get().expect(
        "test-section-header hook not installed before parsing (call install_parser_hooks)",
    );
    hook(line)
}

/// The test-section-header parser, installed by the compiler (via
/// [`set_test_section_header_hook`]) to dogfood the AIPL
/// `parse_test_section_header`. Required — see [`parse_test_section_header`].
static TEST_SECTION_HEADER_HOOK: std::sync::OnceLock<fn(&str) -> Option<String>> =
    std::sync::OnceLock::new();

/// Install the test-section-header parser. The compiler points this at the
/// dogfooded AIPL `parse_test_section_header`, run through the embedding FFI.
/// First install wins (the hook is process-global).
pub fn set_test_section_header_hook(f: fn(&str) -> Option<String>) {
    let _ = TEST_SECTION_HEADER_HOOK.set(f);
}

/// Return the portion of `src` before the first `--- section ---` test
/// marker. The cases test harness uses these markers to bundle expected
/// stdout/stderr/exit/errors after the AIPL code in a single file; the
/// compiler ignores them so `aipl run/ir/build` can be pointed at a test
/// fixture directly without any prep step.
///
/// The marker scan is dogfooded — the AIPL `strip_test_sections` (`str -> str`,
/// like this function), run through the embedding FFI via the installed hook,
/// returns the kept prefix; since that's a byte-prefix of `src` we re-borrow it
/// as `&src[..kept.len()]`. There is **no native fallback**: it panics if the
/// hook isn't installed, so install it (via `install_parser_hooks`) before
/// parsing. (`parse` and `lex_tokens` call this on every parse — see
/// [`set_strip_test_sections_hook`].)
pub fn strip_test_sections(src: &str) -> &str {
    let hook = STRIP_TEST_SECTIONS_HOOK.get().expect(
        "strip-test-sections hook not installed before parsing (call install_parser_hooks)",
    );
    // The returned prefix ends on a line boundary (after a `\n`, or all of `src`),
    // so its byte length is a valid char boundary to re-borrow from `src`.
    &src[..hook(src).len().min(src.len())]
}

/// The `--- file: <path> ---` companion sources declared in `src`, as
/// `(relative path, contents)` — the sibling files a case needs on disk beside
/// it (imported modules, fixtures its tests read). Empty when there are none.
///
/// Section bodies keep their inner blank lines but drop trailing newlines, which
/// is how the test harness has always written them; a companion whose path is
/// empty or contains a backslash is skipped, since both would resolve
/// unpredictably (the harness asserts on those instead, where a fixture author
/// is there to fix it).
///
/// Shared so the corpus harness and `aipl check` stage companions the same way
/// rather than each parsing the markers itself.
///
/// Dogfooded — the AIPL `companion_files`, run through the embedding FFI via the
/// installed hook. There is **no native fallback**: it panics if the hook isn't
/// installed, so install it (via `install_parser_hooks`) first, exactly like
/// [`parse_test_section_header`].
/// `Err(message)` when a `file:` marker names a path that can't be staged (empty,
/// or containing a backslash) — refusing is better than staging a case somewhere
/// unintended, and it matches what the corpus harness already asserts.
pub fn companion_files(src: &str) -> Result<Vec<(String, String)>, String> {
    let hook = COMPANION_FILES_HOOK
        .get()
        .expect("companion-files hook not installed (call install_parser_hooks)");
    hook(src)
}

/// The companion-file extractor, installed by the compiler (via
/// [`set_companion_files_hook`]) to dogfood the AIPL `companion_files`.
/// Required — see [`companion_files`].
#[allow(clippy::type_complexity)]
static COMPANION_FILES_HOOK: std::sync::OnceLock<
    fn(&str) -> Result<Vec<(String, String)>, String>,
> = std::sync::OnceLock::new();

/// Install the companion-file extractor. The compiler points this at the
/// dogfooded AIPL `companion_files`, run through the embedding FFI. First
/// install wins (the hook is process-global).
pub fn set_companion_files_hook(f: fn(&str) -> Result<Vec<(String, String)>, String>) {
    let _ = COMPANION_FILES_HOOK.set(f);
}

/// Write `companions` (from [`companion_files`]) under `dir`, creating parent
/// directories as needed. Used to give a case's tests the sibling files they
/// expect to find in the working directory.
pub fn stage_companions(dir: &Path, companions: &[(String, String)]) -> std::io::Result<()> {
    for (rel, contents) in companions {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
    }
    Ok(())
}

/// The section stripper, installed by the compiler (via
/// [`set_strip_test_sections_hook`]) to dogfood the AIPL `strip_test_sections`.
/// Required — see [`strip_test_sections`]. Returns the kept prefix (a byte-prefix
/// of its input).
static STRIP_TEST_SECTIONS_HOOK: std::sync::OnceLock<fn(&str) -> String> =
    std::sync::OnceLock::new();

/// Install the section stripper. The compiler points this at the dogfooded AIPL
/// `strip_test_sections`, run through the embedding FFI. First install wins (the
/// hook is process-global).
pub fn set_strip_test_sections_hook(f: fn(&str) -> String) {
    let _ = STRIP_TEST_SECTIONS_HOOK.set(f);
}

/// Split `src` into `(main, sections)` at the first `--- name ---` test-section
/// marker line — the counterpart of [`strip_test_sections`] that also returns the
/// stripped-off sections (empty when there are none). Dogfooded via the AIPL
/// `split_test_sections` through the embedding FFI; both halves are byte-
/// substrings of `src`, so they're re-borrowed from it. No native fallback —
/// panics if the hook isn't installed (call `install_parser_hooks` first).
pub fn split_test_sections(src: &str) -> (&str, &str) {
    let hook = SPLIT_TEST_SECTIONS_HOOK.get().expect(
        "split-test-sections hook not installed before parsing (call install_parser_hooks)",
    );
    // The main half ends on a line boundary, so its byte length is a valid char
    // boundary at which to re-borrow both halves from `src`.
    let cut = hook(src).0.len().min(src.len());
    (&src[..cut], &src[cut..])
}

/// The section splitter, installed by the compiler (via
/// [`set_split_test_sections_hook`]) to dogfood the AIPL `split_test_sections`.
/// Required — see [`split_test_sections`]. Returns `(main, sections)`, both
/// byte-substrings of its input.
static SPLIT_TEST_SECTIONS_HOOK: std::sync::OnceLock<fn(&str) -> (String, String)> =
    std::sync::OnceLock::new();

/// Install the section splitter. The compiler points this at the dogfooded AIPL
/// `split_test_sections`, run through the embedding FFI. First install wins (the
/// hook is process-global).
pub fn set_split_test_sections_hook(f: fn(&str) -> (String, String)) {
    let _ = SPLIT_TEST_SECTIONS_HOOK.set(f);
}

/// The [`Span`] of the first line's trailing space/tab run in `src`, or `None`
/// if no line has any — the locator for [`reject_trailing_whitespace`].
/// Dogfooded: the AIPL `find_trailing_whitespace`, run through the embedding FFI
/// via the installed hook. There is **no native fallback**: it panics if the hook
/// isn't installed, so install it (via `install_parser_hooks`) before parsing.
fn find_trailing_whitespace(src: &str) -> Option<Span> {
    let hook = FIND_TRAILING_WHITESPACE_HOOK.get().expect(
        "trailing-whitespace hook not installed before parsing (call install_parser_hooks)",
    );
    hook(src)
}

/// The trailing-whitespace locator, installed by the compiler (via
/// [`set_find_trailing_whitespace_hook`]) to dogfood the AIPL
/// `find_trailing_whitespace`. Required — see [`find_trailing_whitespace`].
static FIND_TRAILING_WHITESPACE_HOOK: std::sync::OnceLock<fn(&str) -> Option<Span>> =
    std::sync::OnceLock::new();

/// Install the trailing-whitespace locator. The compiler points this at the
/// dogfooded AIPL `find_trailing_whitespace`, run through the embedding FFI. First
/// install wins (the hook is process-global).
pub fn set_find_trailing_whitespace_hook(f: fn(&str) -> Option<Span>) {
    let _ = FIND_TRAILING_WHITESPACE_HOOK.set(f);
}

/// Coarse classification of a lexed token, used by the syntax-highlighting
/// test to verify the TextMate grammar at `assets/aipl.tmLanguage.json`
/// assigns sensible scopes. Comments and whitespace are not represented —
/// the lexer skips them — and are verified separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Reserved word: `fn`, `if`, `else`, `struct`, `import`, `from`,
    /// `let`, `for`, `mut`, `set`, `match`, `builtins`.
    Keyword,
    /// `true`, `false`, `none`.
    Constant,
    /// Built-in type names — lexically identifiers (`i64`, `bool`, `char`,
    /// `str`, `any`) but the highlighter scopes them as types.
    BuiltinType,
    /// User-defined identifier (function/struct/var/etc.).
    Identifier,
    /// Integer literal.
    Number,
    /// `"..."` literal.
    Str,
    /// `'.'` literal.
    Char,
    /// Operators: `+ - * / % == != < > <= >= && || ! -> =>`.
    Operator,
    /// Brackets, separators, sigils: `( ) { } [ ] , ; : . ? =`.
    Punctuation,
}

/// Tokenize `input` and classify each token for syntax-highlighter
/// verification. Strips test-section markers first (the lexer doesn't
/// understand them), so the caller only sees AIPL source tokens.
///
/// Lexing is dogfooded: the section stripping *and* the lexing both happen in
/// the AIPL [`lex_aipl_stripped`] via one hook crossing (no native fallback).
/// A [`LexedError`] becomes an [`Error`] at its span.
pub fn lex_tokens(input: &str) -> Result<Vec<(TokenKind, Span)>, Error> {
    let out = lex_aipl_stripped(input).map_err(|e| Error::at(e.message, e.span))?;
    Ok(out
        .tokens
        .into_iter()
        .map(|t| (classify_lexed(&t.kind), t.span))
        .collect())
}

/// A [`TokenKind`] refined for the formatter: template-literal pieces are kept
/// distinct instead of folded into `Str`. The formatter copies a template
/// verbatim from its head to its matching tail, so it must see the piece
/// boundaries — and it can't recover them from token text (an empty segment's
/// `TemplateMiddle` is the single character `{`, identical to a brace).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmtTokenKind {
    Plain(TokenKind),
    /// `` `text{ `` — opens a template literal, through the first `{`.
    TemplateHead,
    /// `}text{` between two interpolations (span starts just after the
    /// previous interpolation's `}`, which the lexer does not emit).
    TemplateMiddle,
    /// `` }text` `` — closes the template literal.
    TemplateTail,
}

/// Map a dogfooded-lexer token kind to the formatter's [`FmtTokenKind`]: an
/// interpolated-template piece (regular *or* raw) keeps its head/middle/tail
/// position, and every other kind — including all four merged `StrLit` string
/// forms — folds to `Plain(classify_lexed(..))` (a `StrLit` classifies to
/// `Str`). Mirrors the native lexer, whose `classify` folds an interpolation-
/// free template to `Str` and collapses raw template pieces to the same
/// head/middle/tail as regular ones.
fn fmt_kind(k: &LexedTokenKind) -> FmtTokenKind {
    use LexedTokenKind as K;
    match k {
        K::TemplateHead(_) | K::RawTemplateHead(_) => FmtTokenKind::TemplateHead,
        K::TemplateMid(_) | K::RawTemplateMid(_) => FmtTokenKind::TemplateMiddle,
        K::TemplateTail(_) | K::RawTemplateTail(_) => FmtTokenKind::TemplateTail,
        other => FmtTokenKind::Plain(classify_lexed(other)),
    }
}

/// The comment spans in a dogfooded-lexer run's trivia, in source order — the
/// line and block comments *and* the `#[allow]` markers. The native tokenizer
/// records a `#[allow]` in its comment sink too ("so the formatter carries it
/// through, glued to its line like a trailing comment"), so every trivia record
/// is a comment for the formatter's purposes. (Whitespace is skipped outright
/// and never reaches the trivia channel.)
fn comment_spans(out: &LexedOutput) -> Vec<Span> {
    out.trivia.iter().map(|t| t.span.clone()).collect()
}

/// Tokenize `input` for the formatter: every token plus the span of every
/// comment, both in source order (token text is recovered from the span, so
/// literals stay verbatim). Lexing is dogfooded ([`lex_aipl`], via the installed
/// hook). Unlike [`lex_tokens`] the input is taken as-is — no test-section
/// stripping — because the formatter splits trailing `--- section ---` blocks
/// off itself and must account for every byte it is given.
#[allow(clippy::type_complexity)]
pub fn lex_tokens_and_comments(
    input: &str,
) -> Result<(Vec<(FmtTokenKind, Span)>, Vec<Span>), Error> {
    let out = lex_aipl(input).map_err(|e| Error::at(e.message, e.span))?;
    let toks = out
        .tokens
        .iter()
        .map(|t| (fmt_kind(&t.kind), t.span.clone()))
        .collect();
    Ok((toks, comment_spans(&out)))
}

/// Tokenize `input` for the formatter's *preservation check*: each token as
/// `(kind, signature)` plus every comment span. A token's signature is its
/// **semantic value** — for a string literal (any of the four delimiter forms,
/// all now one `StrLit`) or a template piece, the lexer's decoded value (escapes
/// applied, and a `"""`/```` ``` ```` literal de-dented — done in the lexer's
/// emit); for everything else, the raw source text (recovered from the span).
/// Two spellings that lex to the same value therefore share a signature, so the
/// formatter's value-preserving whitespace edits (re-indenting a raw block's
/// content or its closing delimiter) don't register as changes, while any real
/// change to a literal's value does. Input is taken as-is (no section
/// stripping), like [`lex_tokens_and_comments`]. Every string/template value —
/// including an interpolated raw template's segments — arrives already decoded
/// and de-dented from the lexer, so re-indenting a raw block is value-preserving.
#[allow(clippy::type_complexity)]
pub fn lex_signatures_and_comments(
    input: &str,
) -> Result<(Vec<(FmtTokenKind, String)>, Vec<Span>), Error> {
    use LexedTokenKind as K;
    let out = lex_aipl(input).map_err(|e| Error::at(e.message, e.span))?;
    let toks = out
        .tokens
        .iter()
        .map(|t| {
            let sig = match &t.kind {
                K::StrLit(v, _)
                | K::TemplateHead(v)
                | K::TemplateMid(v)
                | K::TemplateTail(v)
                | K::RawTemplateHead(v)
                | K::RawTemplateMid(v)
                | K::RawTemplateTail(v) => v.clone(),
                _ => input[t.span.clone()].to_string(),
            };
            (fmt_kind(&t.kind), sig)
        })
        .collect();
    Ok((toks, comment_spans(&out)))
}

/// Coarse-classify a dogfooded-lexer token kind, exactly as [`classify`] does
/// for the native `Terminal` — including the identifier-text refinement that
/// scopes the built-in type names (`i64`/`bool`/`char`/…) as `BuiltinType`.
/// The trivia kinds (`Space`/comments/`AllowMarker`) never appear in the token
/// stream (they ride the trivia side-channel), so reaching one is a bug.
fn classify_lexed(k: &LexedTokenKind) -> TokenKind {
    use LexedTokenKind as K;
    match k {
        K::Fn
        | K::If
        | K::Else
        | K::Struct
        | K::Variant
        | K::Import
        | K::From
        | K::As
        | K::Pub
        | K::Let
        | K::For
        | K::While
        | K::Mut
        | K::Set
        | K::Match
        | K::Return
        | K::Shim
        | K::Builtins => TokenKind::Keyword,
        K::True | K::False | K::None => TokenKind::Constant,
        K::Name(s) => match s.as_str() {
            "bool" | "char" | "str" | "any" => TokenKind::BuiltinType,
            _ if aipl_syntax::int_bits(s).is_some() => TokenKind::BuiltinType,
            _ => TokenKind::Identifier,
        },
        K::IntLit(_) => TokenKind::Number,
        K::StrLit(_, _)
        | K::TemplateHead(_)
        | K::TemplateMid(_)
        | K::TemplateTail(_)
        | K::RawTemplateHead(_)
        | K::RawTemplateMid(_)
        | K::RawTemplateTail(_) => TokenKind::Str,
        K::CharTok(_) => TokenKind::Char,
        K::EqEq
        | K::Ne
        | K::Arrow
        | K::FatArrow
        | K::AndAnd
        | K::OrOr
        | K::Pipe
        | K::DotDot
        | K::PlusPlusPlus
        | K::PlusPlus
        | K::Eq
        | K::Lt
        | K::Le
        | K::Gt
        | K::Ge
        | K::Bang
        | K::Plus
        | K::Minus
        | K::Star
        | K::Slash
        | K::Percent => TokenKind::Operator,
        K::Period
        | K::Comma
        | K::Colon
        | K::Semi
        | K::Question
        | K::Hash
        | K::LParen
        | K::RParen
        | K::LBrace
        | K::RBrace
        | K::LBracket
        | K::RBracket => TokenKind::Punctuation,
        K::Space | K::LineComment | K::BlockComment | K::AllowMarker => {
            unreachable!("trivia kind {k:?} in the token stream")
        }
    }
}

/// Reject trailing whitespace — a space or tab at the end of any line, including
/// inside a (multi-line) string literal (string contents aren't exempt). Reports
/// the first offending run, caret under the whitespace. A `\r` before the newline
/// is treated as part of the line ending (`\r\n`), so the whitespace it follows
/// is still flagged.
///
/// The locating is dogfooded: the AIPL [`find_trailing_whitespace`] returns the
/// byte [`Span`] of the first offending run (or `None`) via the FFI — AIPL's
/// `for (let c : src)` iterates `src` byte-by-byte, so its offsets are byte
/// offsets, matching the error rendering. There is **no native fallback**.
fn reject_trailing_whitespace(src: &str) -> Result<(), Error> {
    match find_trailing_whitespace(src) {
        None => Ok(()),
        Some(span) => Err(Error::at(
            "trailing whitespace is not allowed".to_string(),
            span,
        )),
    }
}

/// [`parse`], additionally returning the spans of every `#[allow]`
/// lint-squelch marker the lexer skipped — the loader hands them to
/// [`aipl_syntax::lint`]'s pass, which drops lint errors squelched by a
/// same-line marker.
pub fn parse_with_allows(input: &str) -> Result<(Program, Vec<Span>), Error> {
    ALLOW_SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
    let result = parse(input);
    let allows = ALLOW_SINK
        .with(|sink| sink.borrow_mut().take())
        .expect("allow sink armed above");
    Ok((result?, allows))
}

/// Convert the dogfooded lexer's token stream into the gazelle `Terminal`
/// stream the parser consumes — an arm-for-arm mapping. The AIPL lexer already
/// decoded every string/char/int value and (in its emit) de-dented the `"""` /
/// ```` ``` ```` literals — interpolated raw-template segments included (that
/// rule's `finalize` is `dedent_segments`) — so every value maps straight
/// across, `RawTemplate*` exactly like the regular `Template*`.
fn lexed_to_terminals(out: LexedOutput) -> Vec<(aipl::Terminal<Build>, Span)> {
    use self::aipl::Terminal as T;
    use LexedTokenKind as K;

    let mut pairs = Vec::with_capacity(out.tokens.len());
    for t in out.tokens.into_iter() {
        let span = t.span;
        let term = match t.kind {
            K::Fn => T::Fn,
            K::If => T::If,
            K::Else => T::Else,
            K::Struct => T::Struct,
            K::Variant => T::Variant,
            K::Import => T::Import,
            K::From => T::From,
            K::As => T::As,
            K::Pub => T::Pub,
            K::Let => T::Let,
            K::For => T::For,
            K::While => T::While,
            K::Mut => T::Mut,
            K::Set => T::Set,
            K::Match => T::Match,
            K::Return => T::Return,
            K::Shim => T::Shim(span.clone()),
            K::Builtins => T::Builtins(span.clone()),
            K::None => T::None(span.clone()),
            K::True => T::True(span.clone()),
            K::False => T::False(span.clone()),
            K::Name(s) => T::Ident((s, span.clone())),
            K::IntLit(n) => T::Num((n, span.clone())),
            K::StrLit(s, _) => T::Str((s, span.clone())),
            K::CharTok(b) => T::Char((b, span.clone())),
            K::TemplateHead(s) => T::TemplateHead((s, span.clone())),
            K::TemplateMid(s) => T::TemplateMiddle((s, span.clone())),
            K::TemplateTail(s) => T::TemplateTail((s, span.clone())),
            K::RawTemplateHead(s) => T::TemplateHead((s, span.clone())),
            K::RawTemplateMid(s) => T::TemplateMiddle((s, span.clone())),
            K::RawTemplateTail(s) => T::TemplateTail((s, span.clone())),
            // Operators carrying a `(char-tag, span)` value + infix precedence,
            // so they can also be used as first-class function values
            // (`apply(2, 3, +)`) and still point diagnostics at themselves.
            K::PlusPlusPlus => T::Op(('C', span.clone()), op_precedence('C')),
            K::EqEq => T::Op(('E', span.clone()), op_precedence('E')),
            K::Ne => T::Op(('N', span.clone()), op_precedence('N')),
            K::Le => T::Op(('L', span.clone()), op_precedence('L')),
            K::Ge => T::Op(('G', span.clone()), op_precedence('G')),
            K::AndAnd => T::Op(('A', span.clone()), op_precedence('A')),
            K::Plus => T::Op(('+', span.clone()), op_precedence('+')),
            K::Star => T::Op(('*', span.clone()), op_precedence('*')),
            K::Slash => T::Op(('/', span.clone()), op_precedence('/')),
            K::Percent => T::Op(('%', span.clone()), op_precedence('%')),
            K::OrOr => T::Oror(op_precedence('O')),
            K::Lt => T::Langle(op_precedence('<')),
            K::Gt => T::Rangle(op_precedence('>')),
            K::Minus => T::Minus(Precedence::Left(6)),
            // Level 4 — tighter than `==`, looser than `<` and the arithmetic;
            // see `op_precedence` for the whole table.
            K::DotDot => T::Dotdot(Precedence::Left(4)),
            K::PlusPlus => T::Plusplus(span.clone()),
            K::Arrow => T::Arrow,
            K::FatArrow => T::Fatarrow,
            K::Pipe => T::Pipe(span.clone()),
            K::Eq => T::Eq,
            K::Bang => T::Bang,
            K::Question => T::Question,
            K::Period => T::Dot,
            K::Comma => T::Comma,
            K::Colon => T::Colon,
            K::Semi => T::Semi,
            K::Hash => T::Hash(span.clone()),
            K::LParen => T::Lparen,
            K::RParen => T::Rparen,
            K::LBrace => T::Lbrace,
            K::RBrace => T::Rbrace,
            K::LBracket => T::Lbracket(span.clone()),
            K::RBracket => T::Rbracket,
            k @ (K::Space | K::LineComment | K::BlockComment | K::AllowMarker) => {
                unreachable!("trivia kind {k:?} in the token stream")
            }
        };
        pairs.push((term, span));
    }
    pairs
}

pub fn parse(input: &str) -> Result<Program, Error> {
    let input = strip_test_sections(input);
    reject_trailing_whitespace(input)?;
    let mut parser = aipl::Parser::<Build>::new();
    let mut actions = Build;

    // Lex through the dogfooded AIPL lexer (input already section-stripped), then
    // map its tokens to the gazelle terminal stream. No native fallback.
    let out = lex_aipl(input).map_err(|e| Error::at(e.message, e.span))?;
    // The `#[allow]` lint-squelch markers ride the lexer's trivia side-channel;
    // feed their spans into the armed `ALLOW_SINK` so `parse_with_allows` (the
    // loader's lint pass) can drop lint errors squelched on the same line.
    for t in &out.trivia {
        if t.kind == LexedTokenKind::AllowMarker {
            record_allow(t.span.clone());
        }
    }
    let pairs = lexed_to_terminals(out);
    // The exact source text of each token, so the error can name the actual
    // token (`+`, `foo`, `0`) rather than its kind. Index matches push order,
    // which is what `format_error` expects for the offending token.
    let texts: Vec<&str> = pairs
        .iter()
        .map(|(_, sp)| input.get(sp.start..sp.end).unwrap_or(""))
        .collect();

    for (tok, span) in pairs {
        match parser.push(tok, &mut actions) {
            Ok(()) => {}
            Err(gazelle::ParseError::Syntax { terminal }) => {
                return Err(Error::at(
                    friendly_syntax_error(&parser, terminal, &texts),
                    span,
                ));
            }
            // A build action rejected the shape (e.g. a mixed `#{ .. }`); its
            // error already carries the right span and message.
            Err(gazelle::ParseError::Action(e)) => return Err(e),
        }
    }

    let mut program = parser.finish(&mut actions).map_err(|(p, err)| match err {
        gazelle::ParseError::Syntax { terminal } => {
            // Unexpected end of input: point the caret just past the source.
            let eof = input.len()..input.len();
            Error::at(friendly_syntax_error(&p, terminal, &texts), eof)
        }
        gazelle::ParseError::Action(e) => e,
    })?;

    // Bake `assert(cond)` calls inside `.test({ .. })` bodies into
    // `__assert(cond, "input:LINE: TEXT")`, capturing each assertion's source
    // location now (while the source is in hand) for the `check` failure report.
    // Only test bodies are rewritten, so a bare `assert(..)` elsewhere stays an
    // unknown call — `assert` is effectively test-only.
    for item in &mut program.items {
        if let Item::Fn(f) = item {
            if let Some(test_body) = &mut f.test_body {
                bake_asserts(test_body, input);
            }
        }
    }
    Ok(program)
}

/// Rewrite each `assert(cond)` within `e` into `__assert(cond, "input:LINE:
/// TEXT")`, where the location string is computed from `src` and the condition's
/// span. Recurses through the whole expression so nested asserts are caught.
fn bake_asserts(e: &mut Expr, src: &str) {
    // Rewrite an `assert(cond)` in place, then recurse into the condition.
    if let ExprKind::Call(name, args, _) = &e.kind {
        if name == "assert" && args.len() == 1 {
            let ExprKind::Call(_, mut args, _) = std::mem::replace(&mut e.kind, ExprKind::Unit)
            else {
                unreachable!()
            };
            let mut cond = args.pop().expect("one arg");
            bake_asserts(&mut cond, src);
            let loc = Expr::new(
                ExprKind::Str(assert_loc(src, cond.span.clone())),
                cond.span.clone(),
            );
            e.kind = ExprKind::Call("__assert".to_string(), vec![cond, loc], false);
            return;
        }
    }
    match &mut e.kind {
        // A shim's bindings are names; asserts can only be in its body.
        ExprKind::Shim(_, _, body) => bake_asserts(body, src),
        ExprKind::Call(_, args, _)
        | ExprKind::ArrayLit(args)
        | ExprKind::SetLit(args)
        | ExprKind::TupleLit(args) => {
            for a in args {
                bake_asserts(a, src);
            }
        }
        ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                bake_asserts(k, src);
                bake_asserts(v, src);
            }
        }
        ExprKind::Binop(a, _, b)
        | ExprKind::Seq(a, b)
        | ExprKind::Let(_, _, a, b)
        | ExprKind::LetMut(_, _, a, b)
        | ExprKind::Assign(_, a, b)
        | ExprKind::Index(a, b)
        | ExprKind::For(_, a, b)
        | ExprKind::While(a, b) => {
            bake_asserts(a, src);
            bake_asserts(b, src);
        }
        ExprKind::If(a, b, c) => {
            bake_asserts(a, src);
            bake_asserts(b, src);
            bake_asserts(c, src);
        }
        ExprKind::Slice(a, b, c) => {
            bake_asserts(a, src);
            bake_asserts(b, src);
            if let Some(c) = c {
                bake_asserts(c, src);
            }
        }
        ExprKind::Neg(x)
        | ExprKind::Not(x)
        | ExprKind::Field(x, _)
        | ExprKind::Try(x)
        | ExprKind::Return(x)
        | ExprKind::KwArg(_, x)
        | ExprKind::Spread(x) => bake_asserts(x, src),
        ExprKind::Construct(_, inits) => {
            for fi in inits {
                bake_asserts(&mut fi.value, src);
            }
        }
        ExprKind::Match(scrut, arms) => {
            bake_asserts(scrut, src);
            for arm in arms {
                bake_asserts(&mut arm.body, src);
            }
        }
        ExprKind::Lambda(_, body) => bake_asserts(body, src),
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Ident(_)
        | ExprKind::None
        | ExprKind::Unit => {}
    }
}

/// Format an assertion's source location as `input:LINE: TEXT` (1-based line,
/// the condition's trimmed source text), matching the `input:` filename the rest
/// of the compiler's diagnostics use. Dogfooded: the AIPL `assert_loc`, run
/// through the embedding FFI via the installed hook. There is **no native
/// fallback**: it panics if the hook isn't installed, so install it (via
/// `install_parser_hooks`) before parsing.
fn assert_loc(src: &str, span: Span) -> String {
    let hook = ASSERT_LOC_HOOK
        .get()
        .expect("assert-loc hook not installed before parsing (call install_parser_hooks)");
    hook(src, span)
}

/// The assertion-location formatter, installed by the compiler (via
/// [`set_assert_loc_hook`]) to dogfood the AIPL `assert_loc`. Required — see
/// [`assert_loc`].
static ASSERT_LOC_HOOK: std::sync::OnceLock<fn(&str, Span) -> String> = std::sync::OnceLock::new();

/// Install the assertion-location formatter. The compiler points this at the
/// dogfooded AIPL `assert_loc`, run through the embedding FFI. First install
/// wins (the hook is process-global).
pub fn set_assert_loc_hook(f: fn(&str, Span) -> String) {
    let _ = ASSERT_LOC_HOOK.set(f);
}

/// Friendly names for grammar symbols, used to turn the parser's internal
/// token/rule names into something a user recognizes (e.g. `RBRACE` → `}`,
/// `IDENT` → `identifier`). Nonterminals that can appear in an "expected" set
/// map to a short noun (e.g. `expr` → `expression`). Anything not listed falls
/// back to its raw grammar name.
const SYMBOL_DISPLAY_NAMES: &[(&str, &str)] = &[
    // Literals / identifiers.
    ("IDENT", "identifier"),
    ("NUM", "number"),
    ("STR", "string"),
    ("CHAR", "character"),
    ("TRUE", "true"),
    ("FALSE", "false"),
    ("NONE", "none"),
    ("BUILTINS", "builtins"),
    // Keywords.
    ("FN", "fn"),
    ("IF", "if"),
    ("ELSE", "else"),
    ("STRUCT", "struct"),
    ("VARIANT", "variant"),
    ("IMPORT", "import"),
    ("FROM", "from"),
    ("AS", "as"),
    ("PUB", "pub"),
    ("LET", "let"),
    ("FOR", "for"),
    ("WHILE", "while"),
    ("MUT", "mut"),
    ("SET", "set"),
    ("MATCH", "match"),
    ("RETURN", "return"),
    ("SHIM", "shim"),
    // Punctuation / operators.
    ("LPAREN", "("),
    ("RPAREN", ")"),
    ("LBRACE", "{"),
    ("RBRACE", "}"),
    ("LBRACKET", "["),
    ("RBRACKET", "]"),
    ("HASH", "#"),
    ("COMMA", ","),
    ("COLON", ":"),
    ("ARROW", "->"),
    ("DOT", "."),
    ("DOTDOT", ".."),
    ("SEMI", ";"),
    ("EQ", "="),
    ("QUESTION", "?"),
    ("FATARROW", "=>"),
    ("BANG", "!"),
    ("PLUSPLUS", "++"),
    ("MINUS", "-"),
    ("OP", "operator"),
    ("LANGLE", "<"),
    ("RANGLE", ">"),
    // Nonterminals that surface in "expected" sets.
    ("expr", "expression"),
    ("term", "expression"),
    ("unary", "expression"),
    ("postfix", "expression"),
    ("atom", "expression"),
    ("binop", "operator"),
    ("kw_stmt", "statement"),
    ("block_body", "statement"),
    ("loop_inner", "statement"),
    ("block", "{"),
    ("fn_body", "{"),
    ("loop_body", "{"),
    ("ty", "type"),
    ("return_ty", "->"),
    ("param", "parameter"),
    ("params", "parameter"),
    ("param_list", "parameter"),
    ("type_param", "type parameter"),
    ("type_params", "<"),
    ("type_param_list", "type parameter"),
    ("field_decl", "field"),
    ("field_init", "field"),
    ("construct_fields", "field"),
    ("arg_list", "expression"),
    ("effect", "effect"),
    ("effects", "effect"),
    ("effect_list", "effect"),
    ("import_name_list", "name"),
    ("item", "definition"),
    // End-of-input is spelled `$` internally.
    ("$", "end of input"),
];

/// Build a clear one-line syntax error from gazelle's diagnostic: keep only the
/// `unexpected X, expected …` summary (dropping the internal parse-stack /
/// item dump), with symbol names humanized and the expected set de-duplicated.
fn friendly_syntax_error(
    parser: &aipl::Parser<Build>,
    terminal: gazelle::SymbolId,
    texts: &[&str],
) -> String {
    let raw = parser.format_error(terminal, Some(SYMBOL_DISPLAY_NAMES), Some(texts));
    let first = raw.lines().next().unwrap_or("syntax error");
    // `first` is `unexpected 'X'` or `unexpected 'X', expected: a, b, c`.
    let (found_part, expected_part) = match first.split_once(", expected: ") {
        Some((f, l)) => (f, Some(l)),
        None => (first, None),
    };
    let found = found_part
        .strip_prefix("unexpected '")
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(found_part);
    let found_msg = if found == "end of input" {
        // Multi-word token reads badly inside quotes.
        "unexpected end of input".to_string()
    } else if found.starts_with('\'') || found.starts_with('"') {
        // Char/string literals are already delimited — don't double-quote
        // (`'a'` not `''a''`).
        format!("unexpected {found}")
    } else {
        format!("unexpected '{found}'")
    };
    match expected_part {
        // Humanized names can collide (e.g. `block` and `LBRACE` both → `{`);
        // de-duplicate (BTreeSet also sorts) for a stable, readable list.
        // Quote literal tokens (`'}'`, `'else'`) so punctuation doesn't blur
        // into the list separators; leave category words (`expression`) bare.
        Some(list) => {
            let items: std::collections::BTreeSet<String> =
                list.split(", ").map(quote_expected).collect();
            let items: Vec<&str> = items.iter().map(String::as_str).collect();
            format!("{found_msg}; expected {}", human_join(&items))
        }
        None => found_msg,
    }
}

/// Category words name a *kind* of thing the user can't type literally and
/// read fine bare; everything else is a concrete token and is quoted.
const CATEGORY_WORDS: &[&str] = &[
    "expression",
    "statement",
    "identifier",
    "number",
    "string",
    "character",
    "type",
    "type parameter",
    "operator",
    "parameter",
    "field",
    "effect",
    "name",
    "definition",
    "end of input",
];

fn quote_expected(item: &str) -> String {
    if CATEGORY_WORDS.contains(&item) {
        item.to_string()
    } else {
        format!("'{item}'")
    }
}

/// Join items as `a`, `a or b`, or `a, b, or c`.
fn human_join(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [a] => a.to_string(),
        [a, b] => format!("{a} or {b}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    }
}
