//! AIPL lexer and parser: the gazelle grammar, the tokenizer, and the
//! human-friendly rendering of syntax errors. Produces an [`aipl_syntax::ast`]
//! tree from source text.

use gazelle::lexer::Scanner;
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
            COMMA, COLON, ARROW, DOT, DOTDOT, SEMI, EQ, QUESTION, FATARROW,
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
            prec OROR
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
        function = vis FN IDENT type_params LPAREN params RPAREN effects return_ty block fn_attrs => function;
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
        // Optional `<T: any, U: any>` generic parameter list. The bound is
        // required syntactically (only `any` is meaningful).
        type_params = LANGLE type_param_list RANGLE => present | _ => empty;
        type_param_list = type_param => first | type_param_list COMMA type_param => rest;
        type_param = IDENT COLON IDENT => type_param;

        effects = effect_list => present | _ => empty;
        effect_list = effect => first | effect_list effect => rest;
        effect = BANG IDENT => effect;

        struct_decl = STRUCT IDENT LBRACE fields RBRACE => struct_decl;

        // `variant Shape = Circle(i64) | Rect(i64, i64) | Empty` — a sum type.
        // Cases are `|`-separated; each is a bare name (nullary) or a name with
        // a parenthesized positional payload. No terminator: the next item's
        // leading keyword ends the case list (only `|` continues it).
        variant_decl = VARIANT IDENT EQ variant_cases => variant_decl;
        variant_cases = variant_case => first
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
              | IDENT COLON ty EQ expr => with_default;

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
           | LPAREN ty_args RPAREN LBRACKET RBRACKET => tuple_array_ty;
        base_ty = IDENT => named
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
                | while_stmt => while_stmt
                | return_stmt => return_stmt;
        let_stmt = LET IDENT EQ expr SEMI => let_stmt;
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
        mut_stmt = MUT IDENT EQ expr SEMI => mut_stmt;
        // `set n = expr;` stores to a mut binding; `set n++;` is sugar for
        // `set n = n + 1;` (so it desugars to a `+`/`wrapping_add` use and is
        // gated on importing `+` like any other operator).
        assign_stmt = SET IDENT EQ expr SEMI => assign_stmt
                    | SET IDENT PLUSPLUS SEMI => incr_stmt;
        for_stmt = FOR LPAREN LET IDENT COLON expr RPAREN loop_body => for_stmt;
        // `for (let (a, b) : expr) { ... }` — destructuring for loop. Desugars to
        // a plain for loop with a synthetic temp var and field-access let bindings
        // prepended to the body; after `FOR LPAREN LET`, the next token (`LPAREN`
        // vs `IDENT`) unambiguously distinguishes this from `for_stmt`.
        for_tuple_stmt = FOR LPAREN LET LPAREN match_bindings RPAREN COLON expr RPAREN loop_body => for_tuple_stmt;
        while_stmt = WHILE LPAREN expr RPAREN loop_body => while_stmt;

        expr = term => term | expr binop expr => binop;
        binop = OP => op | MINUS => minus | LANGLE => lt | RANGLE => gt | OROR => or;

        term = unary => unary;

        unary = MINUS unary => neg | BANG unary => not | postfix => postfix;

        postfix = atom => atom
                | postfix DOT IDENT => field_access
                | postfix DOT NUM => tuple_index
                | postfix DOT IDENT LPAREN args RPAREN => method_call
                | postfix LBRACKET expr RBRACKET => index
                // `recv[start..end]` — string slice. The `..` (DOTDOT) after the
                // first `expr` distinguishes it from a plain index on one token.
                | postfix LBRACKET expr DOTDOT expr RBRACKET => slice
                // `recv[start..]` — open-ended slice (to the receiver's length).
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
             | IF LPAREN expr RPAREN block ELSE block => if_else
             // Else-less `if` (statement position): yields unit, so its `then`
             // block must be unit-typed. Desugars to `if .. {} else {}`.
             | IF LPAREN expr RPAREN block => if_no_else
             | NONE => none_lit
             | MATCH LPAREN expr RPAREN LBRACE match_arms RBRACE => match_expr
             | LBRACKET args RBRACKET => array_lit
             | HASH LBRACE brace_body RBRACE => brace_lit
             // Template literal: `` `text {expr} text` `` with 1+ interpolations.
             // The no-interpolation case is emitted as a plain STR by the lexer.
             | TEMPLATE_HEAD expr template_rest => template_lit;

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
        match_arm = IDENT LPAREN match_bindings RPAREN FATARROW expr => ctor_arm
                  | IDENT FATARROW expr => nullary_arm
                  | NONE FATARROW expr => none_arm
                  | STR FATARROW expr => str_arm
                  | LBRACKET args RBRACKET FATARROW expr => array_arm;
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
        arg = expr => expr | lambda => lambda | OP => op_value
            | IDENT EQ expr => kw_arg;
        lambda = PIPE lambda_params PIPE expr => lambda_expr
               | PIPE lambda_params PIPE block => lambda_block
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
        field_init = IDENT COLON expr => field_init;
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
    type MatchArm = MatchArm;
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
    type ForStmt = StmtSpec;
    type ForTupleStmt = StmtSpec;
    type WhileStmt = StmtSpec;
    type ReturnStmt = StmtSpec;
}

/// A block-body statement, in the form the block-builder needs to fold
/// it into the enclosing expression chain.
pub enum StmtSpec {
    Let {
        name: String,
        name_span: Span,
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
        value: Expr,
        span: Span,
    },
    Assign {
        name: String,
        name_span: Span,
        value: Expr,
        span: Span,
    },
    For {
        var: String,
        var_span: Span,
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
            aipl::ImportName::AliasedBang((name, span)) => name_as_op(name, span, "!"),
            // Operator imports (`import { ==, < } from builtins`). The `OP`
            // span isn't needed here — operator imports rarely conflict, and the
            // "not imported" error points at the use site — so a dummy keeps all
            // operator-import variants (the spanless `-`/`<`/… below) uniform.
            aipl::ImportName::Op((c, _)) => op_import(op_spelling(c)),
            aipl::ImportName::OpMinus => op_import("-"),
            aipl::ImportName::OpLt => op_import("<"),
            aipl::ImportName::OpGt => op_import(">"),
            aipl::ImportName::OpOr => op_import("||"),
            aipl::ImportName::OpBang => op_import("!"),
            // `import { ++ } from builtins;` — the increment operator. Imported
            // on its own spelling (not via `+`); the span is unused like the
            // other bare operators.
            aipl::ImportName::OpPlusplus(_) => op_import("++"),
        })
    }
}

/// An `ImportName` for an operator (no source span — operator tokens carry none).
fn op_import(spelling: &str) -> ImportName {
    ImportName {
        name: spelling.to_string(),
        alias: None,
        span: 0..0,
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
        let aipl::StructDecl::StructDecl((name, _), fields) = node;
        Ok(StructDecl { name, fields })
    }
}

impl gazelle::Action<aipl::VariantDecl<Self>> for Build {
    fn build(&mut self, node: aipl::VariantDecl<Self>) -> Result<VariantDecl, Self::Error> {
        let aipl::VariantDecl::VariantDecl((name, _), cases) = node;
        Ok(VariantDecl { name, cases })
    }
}

impl gazelle::Action<aipl::VariantCases<Self>> for Build {
    fn build(&mut self, node: aipl::VariantCases<Self>) -> Result<Vec<VariantCase>, Self::Error> {
        Ok(match node {
            aipl::VariantCases::First(c) => vec![c],
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
        let aipl::FieldInit::FieldInit((name, _), value) = node;
        Ok(FieldInit { name, value })
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
            aipl::Param::VariadicParam((name, _), elem, (op, op_span)) => {
                if op != '*' {
                    return Err(Error::at(
                        format!("expected \"*\" after a variadic parameter type, found {op:?}"),
                        op_span,
                    ));
                }
                // The element's sequence type: a `char` sequence is an AIPL
                // string (`char*` → `str`); every other element `T` uses `T[]`
                // (so `str*` is `str[]`, `i64*` is `i64[]`, etc.).
                let ty = if elem == Type::Primitive(Primitive::Char) {
                    Type::Primitive(Primitive::Str)
                } else {
                    Type::Array(Box::new(elem))
                };
                Param {
                    name,
                    ty,
                    mutable: false,
                    variadic: true,
                    default: None,
                }
            }
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
        // when the type variable is later resolved by a call.
        let aipl::TypeParam::TypeParam((name, _), (bound_name, bound_span)) = node;
        let bound = Bound::from_name(&bound_name).ok_or_else(|| {
            Error::at(
                format!("unknown type parameter bound {bound_name:?}; expected \"any\" or \"ord\""),
                bound_span,
            )
        })?;
        Ok(TypeParam { name, bound })
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
            value,
            ..
        } => {
            let span = join_spans(&name_span, &acc.span);
            Expr::new(ExprKind::Let(name, Box::new(value), Box::new(acc)), span)
        }
        StmtSpec::Mut {
            name,
            name_span,
            value,
            ..
        } => {
            let span = join_spans(&name_span, &acc.span);
            Expr::new(ExprKind::LetMut(name, Box::new(value), Box::new(acc)), span)
        }
        StmtSpec::Assign {
            name,
            name_span,
            value,
            ..
        } => {
            let span = join_spans(&name_span, &acc.span);
            Expr::new(ExprKind::Assign(name, Box::new(value), Box::new(acc)), span)
        }
        StmtSpec::For {
            var,
            var_span,
            iterable,
            body,
            span: for_span,
        } => {
            let for_expr = Expr::new(
                ExprKind::For(var, Box::new(iterable), Box::new(body)),
                for_span,
            );
            let span = join_spans(&var_span, &acc.span);
            Expr::new(ExprKind::Seq(Box::new(for_expr), Box::new(acc)), span)
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
                    ExprKind::Let(name.clone(), Box::new(field), Box::new(result)),
                    inner_span,
                );
            }
            let outer_span = join_spans(&tup_span, &result.span);
            Expr::new(
                ExprKind::Let(tmp, Box::new(value), Box::new(result)),
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
                    ExprKind::Let(field_name.clone(), Box::new(field), Box::new(result)),
                    inner_span,
                );
            }
            let outer_span = join_spans(&struct_span, &result.span);
            Expr::new(
                ExprKind::Let(tmp, Box::new(value), Box::new(result)),
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
            aipl::KwStmt::WhileStmt(s) => s,
            aipl::KwStmt::ReturnStmt(s) => s,
        })
    }
}

impl gazelle::Action<aipl::LetStmt<Self>> for Build {
    fn build(&mut self, node: aipl::LetStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let aipl::LetStmt::LetStmt((name, name_span), value) = node;
        let span = join_spans(&name_span, &value.span);
        Ok(StmtSpec::Let {
            name,
            name_span,
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
        let aipl::MutStmt::MutStmt((name, name_span), value) = node;
        let span = join_spans(&name_span, &value.span);
        Ok(StmtSpec::Mut {
            name,
            name_span,
            value,
            span,
        })
    }
}

impl gazelle::Action<aipl::AssignStmt<Self>> for Build {
    fn build(&mut self, node: aipl::AssignStmt<Self>) -> Result<StmtSpec, Self::Error> {
        let (name, name_span, value, span) = match node {
            aipl::AssignStmt::AssignStmt((name, name_span), value) => {
                let span = join_spans(&name_span, &value.span);
                (name, name_span, value, span)
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
                (name, name_span, value, span)
            }
        };
        Ok(StmtSpec::Assign {
            name,
            name_span,
            value,
            span,
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
                ExprKind::Let(name.clone(), Box::new(field), Box::new(new_body)),
                inner_span,
            );
        }
        let span = join_spans(&tmp_span, &new_body.span);
        Ok(StmtSpec::For {
            var: tmp,
            var_span: tmp_span,
            iterable,
            body: new_body,
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
                Expr::new(ExprKind::Index(Box::new(obj), Box::new(index)), span)
            }
            aipl::Postfix::Slice(obj, _lbracket, start, end) => {
                let span = join_spans(&obj.span, &end.span);
                Expr::new(
                    ExprKind::Slice(Box::new(obj), Box::new(start), Some(Box::new(end))),
                    span,
                )
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
            aipl::Atom::NoneLit(span) => Expr::new(ExprKind::None, span),
            aipl::Atom::MatchExpr(scrutinee, arms) => {
                let last_span = arms
                    .last()
                    .map(|a| a.span.clone())
                    .unwrap_or_else(|| scrutinee.span.clone());
                let span = join_spans(&scrutinee.span, &last_span);
                Expr::new(ExprKind::Match(Box::new(scrutinee), arms), span)
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
            aipl::MatchArmList::First(a) => vec![a],
            aipl::MatchArmList::Rest(mut prev, a) => {
                prev.push(a);
                prev
            }
        })
    }
}

impl gazelle::Action<aipl::MatchArm<Self>> for Build {
    fn build(&mut self, node: aipl::MatchArm<Self>) -> Result<MatchArm, Self::Error> {
        Ok(match node {
            aipl::MatchArm::CtorArm((name, span), bindings, body) => MatchArm {
                pattern: Pattern::Ctor { name, bindings },
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
            aipl::MatchArm::StrArm((lit, span), body) => MatchArm {
                pattern: Pattern::Str(lit),
                body,
                span,
            },
            // `[e0, e1, ...] => body`: an array-literal pattern (for an array
            // scrutinee). The elements are validated as literals by the checker.
            aipl::MatchArm::ArrayArm(span, elems, body) => MatchArm {
                pattern: Pattern::Array(elems),
                body,
                span,
            },
        })
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
        })
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
            aipl::Lambda::LambdaExpr(pipe_span, params, _pipe2, body)
            | aipl::Lambda::LambdaBlock(pipe_span, params, _pipe2, body) => {
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
        })
    }
}

// Op encoding: arithmetic + logical ops use distinct codes so the binop
// reducer can pass them through as `char`.
//   '||' / '&&'   => 'O' / 'A'
//   '==' / '!='   => 'E' / 'N'
//   '<=' / '>='   => 'L' / 'G'
//   '<' / '>' / '+' / '*' / '/' keep their literal chars
fn op_precedence(c: char) -> Precedence {
    match c {
        'O' => Precedence::Left(2),
        'A' => Precedence::Left(3),
        'E' | 'N' => Precedence::Left(4),
        '<' | '>' | 'L' | 'G' => Precedence::Left(5),
        '+' => Precedence::Left(6),
        '*' | '/' | '%' => Precedence::Left(7),
        _ => unreachable!("unknown op code {c:?}"),
    }
}

/// Skip whitespace plus `// line comments` and `/* block comments */`.
/// Block comments nest, matching Rust's behavior. Returns an error on an
/// unterminated block comment so the EOF doesn't silently swallow code.
fn skip_whitespace_and_comments<I: Iterator<Item = char>>(
    src: &mut Scanner<I>,
) -> Result<(), Error> {
    loop {
        src.skip_whitespace();
        match (src.peek(), src.peek_n(1)) {
            (Some('/'), Some('/')) => {
                let start = src.offset();
                src.advance();
                src.advance();
                while let Some(c) = src.peek() {
                    if c == '\n' {
                        break;
                    }
                    src.advance();
                }
                // The span excludes the terminating newline (a comment's text,
                // not its line ending); the newline is consumed by the next
                // `skip_whitespace`.
                record_comment(start..src.offset());
            }
            (Some('/'), Some('*')) => {
                let start = src.offset();
                src.advance();
                src.advance();
                let mut depth = 1usize;
                while depth > 0 {
                    match (src.peek(), src.peek_n(1)) {
                        (Some('/'), Some('*')) => {
                            src.advance();
                            src.advance();
                            depth += 1;
                        }
                        (Some('*'), Some('/')) => {
                            src.advance();
                            src.advance();
                            depth -= 1;
                        }
                        (Some(_), _) => {
                            src.advance();
                        }
                        (None, _) => {
                            return Err(Error::at(
                                "unterminated block comment",
                                start..src.offset(),
                            ));
                        }
                    }
                }
                record_comment(start..src.offset());
            }
            _ => return Ok(()),
        }
    }
}

thread_local! {
    /// When armed (by [`lex_tokens_and_comments`]), every comment the lexer
    /// skips is recorded here. A thread-local rather than a parameter so the
    /// recursive tokenizer plumbing (templates re-enter `tokenize_one`) stays
    /// untouched; ordinary parses leave it disarmed and pay one cell read per
    /// comment.
    static COMMENT_SINK: std::cell::RefCell<Option<Vec<Span>>> =
        const { std::cell::RefCell::new(None) };
}

/// Record a skipped comment's span into the armed sink, if any.
fn record_comment(span: Span) {
    COMMENT_SINK.with(|sink| {
        if let Some(v) = sink.borrow_mut().as_mut() {
            v.push(span);
        }
    });
}

/// Tokenize, pairing each terminal with its source span so the parser can
/// point a caret at the offending token on a syntax error.
/// Process the verbatim contents of a `"""..."""` raw string: trim the
/// surrounding line breaks (the one right after the opening `"""` and the one
/// right before the closing `"""`), then de-dent by stripping the common
/// leading-*space* prefix shared by every non-blank line. Raw strings do no
/// escape processing — their contents are otherwise taken literally.
///
/// The whole transform runs through the installed hook (the dogfooded AIPL
/// `process_raw_string`, via the embedding FFI). There is no native fallback —
/// the hook must be installed before any `"""` raw string is parsed.
fn process_raw_string(content: &str) -> String {
    let hook = RAW_STRING_HOOK
        .get()
        .expect("process_raw_string hook not installed before parsing a raw string");
    assert!(
        !IN_RAW_STRING_HOOK.with(std::cell::Cell::get),
        "process_raw_string hook recursed — its compilation must not contain a raw string",
    );
    IN_RAW_STRING_HOOK.with(|f| f.set(true));
    let _reset = RawStringHookGuard;
    hook(content)
}

/// The raw-string processor, installed by the compiler (via
/// [`set_process_raw_string_hook`]).
static RAW_STRING_HOOK: std::sync::OnceLock<fn(&str) -> String> = std::sync::OnceLock::new();

thread_local! {
    /// Set while the hook runs, so a re-entrant call — which would mean the
    /// hook's *own* compilation contained a raw string — aborts loudly instead
    /// of recursing forever.
    static IN_RAW_STRING_HOOK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install the raw-string processor. The compiler points this at the dogfooded
/// AIPL `process_raw_string`, run through the embedding FFI. First install wins
/// (the hook is process-global).
pub fn set_process_raw_string_hook(f: fn(&str) -> String) {
    let _ = RAW_STRING_HOOK.set(f);
}

/// Resets the re-entrancy flag even if the hook panics.
struct RawStringHookGuard;
impl Drop for RawStringHookGuard {
    fn drop(&mut self) {
        IN_RAW_STRING_HOOK.with(|f| f.set(false));
    }
}

fn tokenize(input: &str) -> Result<Vec<(aipl::Terminal<Build>, Span)>, Error> {
    let mut src = Scanner::new(input);
    let mut tokens: Vec<(aipl::Terminal<Build>, Span)> = Vec::new();
    loop {
        skip_whitespace_and_comments(&mut src)?;
        if src.at_end() {
            break;
        }
        tokenize_one(&mut src, &mut tokens)?;
    }
    Ok(tokens)
}

/// Tokenize one token from `src` (whitespace already skipped) and push it
/// onto `tokens`. Handles all token types including template literals.
fn tokenize_one<I: Iterator<Item = char>>(
    src: &mut Scanner<I>,
    tokens: &mut Vec<(aipl::Terminal<Build>, Span)>,
) -> Result<(), Error> {
    let start = src.offset();
    let c = src.peek().unwrap();

    // Raw string: `"""..."""`. Contents are taken verbatim (no escapes);
    // a `"` or `""` may appear inside, only `"""` closes. One surrounding
    // line break is trimmed and the contents de-dented (see
    // `process_raw_string`). A normal `"..."` literal is handled below.
    if c == '"' && src.peek_n(1) == Some('"') && src.peek_n(2) == Some('"') {
        src.advance();
        src.advance();
        src.advance(); // opening """
        let mut raw = String::new();
        loop {
            match src.peek() {
                Some('"') if src.peek_n(1) == Some('"') && src.peek_n(2) == Some('"') => {
                    src.advance();
                    src.advance();
                    src.advance(); // closing """
                    break;
                }
                Some(ch) => {
                    raw.push(ch);
                    src.advance();
                }
                None => {
                    return Err(Error::at(
                        "unterminated raw string literal",
                        start..src.offset(),
                    ));
                }
            }
        }
        let span = start..src.offset();
        tokens.push((
            aipl::Terminal::Str((process_raw_string(&raw), span.clone())),
            span,
        ));
        return Ok(());
    }

    // String literal: `"..."`. Supports the escapes:
    //   \n  newline    \t  tab      \r  carriage return
    //   \\  backslash  \"  quote
    // (no \0: strings are null-terminated at the runtime level)
    if c == '"' {
        src.advance(); // opening "
        let mut s = String::new();
        loop {
            match src.peek() {
                Some('"') => break,
                Some('\\') => {
                    let esc_start = src.offset();
                    src.advance();
                    match src.peek() {
                        Some('n') => {
                            s.push('\n');
                            src.advance();
                        }
                        Some('t') => {
                            s.push('\t');
                            src.advance();
                        }
                        Some('r') => {
                            s.push('\r');
                            src.advance();
                        }
                        Some('\\') => {
                            s.push('\\');
                            src.advance();
                        }
                        Some('"') => {
                            s.push('"');
                            src.advance();
                        }
                        Some(other) => {
                            return Err(Error::at(
                                format!("unknown escape sequence \\{other}"),
                                esc_start..src.offset() + other.len_utf8(),
                            ));
                        }
                        None => {
                            return Err(Error::at(
                                "unterminated string literal",
                                start..src.offset(),
                            ));
                        }
                    }
                }
                Some(ch) => {
                    s.push(ch);
                    src.advance();
                }
                None => {
                    return Err(Error::at(
                        "unterminated string literal",
                        start..src.offset(),
                    ));
                }
            }
        }
        src.advance(); // closing "
        let span = start..src.offset();
        tokens.push((aipl::Terminal::Str((s, span.clone())), span));
        return Ok(());
    }

    // Char literal: `'x'` or `'\n'`. One byte (ASCII); the same escape
    // set as strings. Non-ASCII characters (UTF-8 multi-byte) are
    // rejected so `char` stays a byte-deterministic primitive.
    if c == '\'' {
        src.advance(); // opening '
        let byte = match src.peek() {
            Some('\\') => {
                src.advance();
                let esc_at = src.offset() - 1;
                let b = match src.peek() {
                    Some('n') => b'\n',
                    Some('t') => b'\t',
                    Some('r') => b'\r',
                    Some('\\') => b'\\',
                    Some('\'') => b'\'',
                    Some('"') => b'"',
                    Some(other) => {
                        return Err(Error::at(
                            format!("unknown escape sequence \\{other}"),
                            esc_at..src.offset() + other.len_utf8(),
                        ));
                    }
                    None => {
                        return Err(Error::at("unterminated char literal", start..src.offset()));
                    }
                };
                src.advance();
                b
            }
            Some('\'') => {
                return Err(Error::at("empty char literal", start..src.offset() + 1));
            }
            Some(ch) => {
                if !ch.is_ascii() {
                    return Err(Error::at(
                        format!("non-ASCII character in char literal: {ch:?}"),
                        start..src.offset() + ch.len_utf8(),
                    ));
                }
                src.advance();
                ch as u8
            }
            None => {
                return Err(Error::at("unterminated char literal", start..src.offset()));
            }
        };
        match src.peek() {
            Some('\'') => {
                src.advance();
            }
            _ => {
                return Err(Error::at(
                    "char literal must contain exactly one character",
                    start..src.offset(),
                ));
            }
        }
        let span = start..src.offset();
        tokens.push((aipl::Terminal::Char((byte, span.clone())), span));
        return Ok(());
    }

    // Triple-backtick template literal: ```text {expr} text```.
    // Like single-backtick but verbatim (no escape sequences) and with
    // process_raw_string applied to the combined text segments (de-indent +
    // surrounding-blank-line strip). Must be checked before single-backtick.
    if c == '`' && src.peek_n(1) == Some('`') && src.peek_n(2) == Some('`') {
        return tokenize_triple_template(src, start, tokens);
    }

    // Template literal: `` `text {expr} text` ``.
    // A no-interpolation template (no `{`) is emitted as a plain STR.
    // Otherwise: TEMPLATE_HEAD, then expression tokens (brace-depth tracked),
    // then TEMPLATE_MIDDLE / TEMPLATE_TAIL until the closing `` ` ``.
    if c == '`' {
        return tokenize_template(src, start, tokens);
    }

    // Identifier or keyword
    if c.is_alphabetic() || c == '_' {
        let mut s = String::new();
        while let Some(ch) = src.peek() {
            if ch.is_alphanumeric() || ch == '_' {
                s.push(ch);
                src.advance();
            } else {
                break;
            }
        }
        let span = start..src.offset();
        let kw = match s.as_str() {
            "fn" => aipl::Terminal::Fn,
            "if" => aipl::Terminal::If,
            "else" => aipl::Terminal::Else,
            "struct" => aipl::Terminal::Struct,
            "variant" => aipl::Terminal::Variant,
            "import" => aipl::Terminal::Import,
            "from" => aipl::Terminal::From,
            "as" => aipl::Terminal::As,
            "pub" => aipl::Terminal::Pub,
            "let" => aipl::Terminal::Let,
            "for" => aipl::Terminal::For,
            "while" => aipl::Terminal::While,
            "mut" => aipl::Terminal::Mut,
            "set" => aipl::Terminal::Set,
            "match" => aipl::Terminal::Match,
            "return" => aipl::Terminal::Return,
            "builtins" => aipl::Terminal::Builtins(span.clone()),
            "none" => aipl::Terminal::None(span.clone()),
            "true" => aipl::Terminal::True(span.clone()),
            "false" => aipl::Terminal::False(span.clone()),
            _ => aipl::Terminal::Ident((s, span.clone())),
        };
        tokens.push((kw, span));
        return Ok(());
    }

    // Number
    if c.is_ascii_digit() {
        let mut s = String::new();
        while let Some(ch) = src.peek() {
            if ch.is_ascii_digit() {
                s.push(ch);
                src.advance();
            } else {
                break;
            }
        }
        let span = start..src.offset();
        let n: i64 = s
            .parse()
            .map_err(|e| Error::at(format!("bad number {s:?}: {e}"), span.clone()))?;
        tokens.push((aipl::Terminal::Num((n, span.clone())), span));
        return Ok(());
    }

    // Two-char operators (must beat their single-char counterparts). An
    // `OP` carries its own span (`(char, Span)`) so it can be used as a
    // value (`apply(2, 3, +)`) and still point diagnostics at the operator.
    if let Some(next) = src.peek_n(1) {
        let op_span = start..start + 2;
        let pair_tok = match (c, next) {
            ('-', '>') => Some(aipl::Terminal::Arrow),
            // `..` — the range separator in a slice `s[a..b]`. Must beat the
            // single-char `.` (field/method access).
            ('.', '.') => Some(aipl::Terminal::Dotdot),
            ('=', '>') => Some(aipl::Terminal::Fatarrow),
            // `++` — increment statement; must beat the single-char `+`.
            ('+', '+') => Some(aipl::Terminal::Plusplus(op_span)),
            ('=', '=') => Some(aipl::Terminal::Op(('E', op_span), op_precedence('E'))),
            ('!', '=') => Some(aipl::Terminal::Op(('N', op_span), op_precedence('N'))),
            ('<', '=') => Some(aipl::Terminal::Op(('L', op_span), op_precedence('L'))),
            ('>', '=') => Some(aipl::Terminal::Op(('G', op_span), op_precedence('G'))),
            ('&', '&') => Some(aipl::Terminal::Op(('A', op_span), op_precedence('A'))),
            ('|', '|') => Some(aipl::Terminal::Oror(op_precedence('O'))),
            _ => None,
        };
        if let Some(t) = pair_tok {
            src.advance();
            src.advance();
            tokens.push((t, start..src.offset()));
            return Ok(());
        }
    }

    // Single-char punctuation and operators
    let tok = match c {
        '(' => aipl::Terminal::Lparen,
        ')' => aipl::Terminal::Rparen,
        '{' => aipl::Terminal::Lbrace,
        '}' => aipl::Terminal::Rbrace,
        // `[` carries a span so array-literal expressions (which may
        // be empty, `[]`, and thus have no element span) can still
        // point somewhere sensible in errors.
        '[' => aipl::Terminal::Lbracket(start..start + 1),
        ']' => aipl::Terminal::Rbracket,
        '#' => aipl::Terminal::Hash(start..start + 1),
        ',' => aipl::Terminal::Comma,
        ':' => aipl::Terminal::Colon,
        '.' => aipl::Terminal::Dot,
        ';' => aipl::Terminal::Semi,
        '=' => aipl::Terminal::Eq,
        '!' => aipl::Terminal::Bang,
        '?' => aipl::Terminal::Question,
        // Single `|` opens/closes a lambda parameter list (`||` for
        // logical-or is handled by the two-char pass above).
        '|' => aipl::Terminal::Pipe(start..start + 1),
        '-' => aipl::Terminal::Minus(Precedence::Left(6)),
        '+' | '*' | '/' | '%' => {
            aipl::Terminal::Op((c, start..start + c.len_utf8()), op_precedence(c))
        }
        // `<` / `>` are both comparison operators and generic-param
        // brackets; they carry comparison precedence either way.
        '<' => aipl::Terminal::Langle(op_precedence('<')),
        '>' => aipl::Terminal::Rangle(op_precedence('>')),
        other => {
            return Err(Error::at(
                format!("unexpected character {other:?}"),
                start..start + other.len_utf8(),
            ));
        }
    };
    src.advance();
    tokens.push((tok, start..src.offset()));
    Ok(())
}

/// Scan a template literal starting at `template_start` (the position of the
/// opening `` ` ``).  Emits either a plain STR (no interpolations) or a
/// sequence of TEMPLATE_HEAD, expression tokens, TEMPLATE_MIDDLE/TAIL pairs.
///
/// Escapes inside text segments: `\\` `\`` `\{` `\n` `\t` `\r`.
fn tokenize_template<I: Iterator<Item = char>>(
    src: &mut Scanner<I>,
    template_start: usize,
    tokens: &mut Vec<(aipl::Terminal<Build>, Span)>,
) -> Result<(), Error> {
    src.advance(); // consume opening `

    // Scan the first text segment (before the first `{` or closing `` ` ``).
    let mut text = String::new();
    let mut open_brace: Span;
    loop {
        match src.peek() {
            None => {
                return Err(Error::at(
                    "unterminated template literal",
                    template_start..src.offset(),
                ));
            }
            Some('`') => {
                // No interpolations: emit as a plain STR.
                src.advance();
                let span = template_start..src.offset();
                tokens.push((aipl::Terminal::Str((text, span.clone())), span));
                return Ok(());
            }
            Some('{') => {
                // First interpolation: emit TEMPLATE_HEAD.
                let brace_start = src.offset();
                let head_end = brace_start + 1;
                src.advance(); // consume {
                open_brace = brace_start..head_end;
                let head_span = template_start..head_end;
                tokens.push((
                    aipl::Terminal::TemplateHead((text, head_span.clone())),
                    head_span,
                ));
                break;
            }
            Some('\\') => {
                src.advance();
                match src.peek() {
                    Some('`') => {
                        text.push('`');
                        src.advance();
                    }
                    Some('{') => {
                        text.push('{');
                        src.advance();
                    }
                    Some('n') => {
                        text.push('\n');
                        src.advance();
                    }
                    Some('t') => {
                        text.push('\t');
                        src.advance();
                    }
                    Some('r') => {
                        text.push('\r');
                        src.advance();
                    }
                    Some('\\') => {
                        text.push('\\');
                        src.advance();
                    }
                    Some(other) => {
                        let esc = src.offset() - 1;
                        return Err(Error::at(
                            format!("unknown escape sequence \\{other}"),
                            esc..esc + 1 + other.len_utf8(),
                        ));
                    }
                    None => {
                        return Err(Error::at(
                            "unterminated template literal",
                            template_start..src.offset(),
                        ));
                    }
                }
            }
            Some(ch) => {
                text.push(ch);
                src.advance();
            }
        }
    }

    // HEAD was emitted. Alternate: scan expression tokens, then a text
    // segment ending in MIDDLE or TAIL.
    loop {
        tokenize_template_expr(src, open_brace.clone(), tokens)?;

        let seg_start = src.offset();
        let mut seg = String::new();
        loop {
            match src.peek() {
                None => {
                    return Err(Error::at(
                        "unterminated template literal",
                        template_start..src.offset(),
                    ));
                }
                Some('`') => {
                    src.advance(); // consume closing `
                    let tail_span = seg_start..src.offset();
                    tokens.push((
                        aipl::Terminal::TemplateTail((seg, tail_span.clone())),
                        tail_span,
                    ));
                    return Ok(());
                }
                Some('{') => {
                    let brace_start = src.offset();
                    let mid_end = brace_start + 1;
                    src.advance(); // consume {
                    open_brace = brace_start..mid_end;
                    let mid_span = seg_start..mid_end;
                    tokens.push((
                        aipl::Terminal::TemplateMiddle((seg, mid_span.clone())),
                        mid_span,
                    ));
                    break; // continue outer loop: scan next expression
                }
                Some('\\') => {
                    src.advance();
                    match src.peek() {
                        Some('`') => {
                            seg.push('`');
                            src.advance();
                        }
                        Some('{') => {
                            seg.push('{');
                            src.advance();
                        }
                        Some('n') => {
                            seg.push('\n');
                            src.advance();
                        }
                        Some('t') => {
                            seg.push('\t');
                            src.advance();
                        }
                        Some('r') => {
                            seg.push('\r');
                            src.advance();
                        }
                        Some('\\') => {
                            seg.push('\\');
                            src.advance();
                        }
                        Some(other) => {
                            let esc = src.offset() - 1;
                            return Err(Error::at(
                                format!("unknown escape sequence \\{other}"),
                                esc..esc + 1 + other.len_utf8(),
                            ));
                        }
                        None => {
                            return Err(Error::at(
                                "unterminated template literal",
                                template_start..src.offset(),
                            ));
                        }
                    }
                }
                Some(ch) => {
                    seg.push(ch);
                    src.advance();
                }
            }
        }
    }
}

/// Scan the expression tokens inside a `{ .. }` interpolation. The opening
/// `{` has already been consumed (`open_brace` is its span, for error
/// reporting). Emits tokens until the matching `}` is reached; that closing
/// `}` is consumed but NOT emitted (it is the template delimiter, not an
/// `RBRACE`). Nested `{` / `}` pairs (e.g. from set/dict literals, blocks, or
/// nested template literals) are depth-tracked so their `LBRACE`/`RBRACE`
/// tokens are emitted normally.
///
/// A missing `}` doesn't necessarily surface as an error *here*: scanning
/// just keeps consuming subsequent source looking for the closing brace, so a
/// stray token deep inside (e.g. a `` ` `` that reads as opening some other
/// template literal) can fail far from the real mistake — often past the
/// enclosing template literal's own closing delimiter, which then itself
/// misreports as "unterminated". So every error that can propagate out of
/// this scan (whether from here directly or from a nested tokenize call) gets
/// `open_brace` attached as a note, pointing back at the interpolation that
/// was never actually closed.
fn tokenize_template_expr<I: Iterator<Item = char>>(
    src: &mut Scanner<I>,
    open_brace: Span,
    tokens: &mut Vec<(aipl::Terminal<Build>, Span)>,
) -> Result<(), Error> {
    let mut depth = 1usize;
    while depth > 0 {
        skip_whitespace_and_comments(src)
            .map_err(|e| e.with_note("in this unclosed interpolation", open_brace.clone()))?;
        if src.at_end() {
            return Err(Error::at(
                "unterminated template literal interpolation",
                open_brace.clone(),
            ));
        }
        let pos = src.offset();
        match src.peek().unwrap() {
            '{' => {
                depth += 1;
                src.advance();
                tokens.push((aipl::Terminal::Lbrace, pos..src.offset()));
            }
            '}' => {
                depth -= 1;
                src.advance();
                if depth > 0 {
                    tokens.push((aipl::Terminal::Rbrace, pos..src.offset()));
                }
                // depth == 0: closing } of the interpolation; don't emit it.
            }
            _ => {
                tokenize_one(src, tokens).map_err(|e| {
                    e.with_note("in this unclosed interpolation", open_brace.clone())
                })?;
            }
        }
    }
    Ok(())
}

/// Triple-backtick template literal: ` ```text {expr} text``` `.
/// Like single-backtick but verbatim content (no escape sequences) and with
/// `process_raw_string` applied to the combined text segments so that the
/// leading/trailing blank line is stripped and the common indent is removed —
/// the same treatment as a `"""..."""` raw string.
///
/// Implementation: two phases.
/// Phase 1 — scan all text segments verbatim, tokenizing each `{...}`
///   expression into a local sub-buffer.
/// Phase 2 — join the text segments with `\x00` (a character that cannot
///   appear in AIPL source), run `process_raw_string` on the joined string,
///   split back on `\x00` to recover the processed segments, then emit all
///   tokens (same TEMPLATE_HEAD / TEMPLATE_MIDDLE / TEMPLATE_TAIL terminals
///   as the single-backtick variant).
fn tokenize_triple_template<I: Iterator<Item = char>>(
    src: &mut Scanner<I>,
    template_start: usize,
    tokens: &mut Vec<(aipl::Terminal<Build>, Span)>,
) -> Result<(), Error> {
    src.advance();
    src.advance();
    src.advance(); // opening ```

    // Phase 1: collect text segments and per-expression token buffers.
    // text_segs[i] = (raw_text, span_start, span_end)
    // expr_bufs[i] = tokens for the i-th interpolation expression
    let mut text_segs: Vec<(String, usize, usize)> = Vec::new();
    let mut expr_bufs: Vec<Vec<(aipl::Terminal<Build>, Span)>> = Vec::new();

    let mut cur_text = String::new();
    let mut seg_start = src.offset();

    loop {
        match src.peek() {
            None => {
                return Err(Error::at(
                    "unterminated triple-backtick template literal",
                    template_start..src.offset(),
                ));
            }
            Some('`') if src.peek_n(1) == Some('`') && src.peek_n(2) == Some('`') => {
                let seg_end = src.offset();
                src.advance();
                src.advance();
                src.advance(); // consume closing ```
                text_segs.push((cur_text, seg_start, src.offset()));
                let _ = seg_end; // span_end is after the closing ```, computed above
                break;
            }
            Some('{') => {
                let brace_start = src.offset();
                let seg_end = brace_start + 1; // include the {
                src.advance(); // consume {
                text_segs.push((cur_text.clone(), seg_start, seg_end));
                cur_text = String::new();

                let mut expr_tokens: Vec<(aipl::Terminal<Build>, Span)> = Vec::new();
                tokenize_template_expr(src, brace_start..seg_end, &mut expr_tokens)?;
                expr_bufs.push(expr_tokens);
                seg_start = src.offset();
            }
            Some(ch) => {
                cur_text.push(ch);
                src.advance();
            }
        }
    }

    // Phase 2: apply process_raw_string to the combined text.
    // Join with \x00 as a separator that can't appear in source.
    let combined: String = text_segs
        .iter()
        .map(|(t, _, _)| t.as_str())
        .collect::<Vec<_>>()
        .join("\x00");
    let processed = process_raw_string(&combined);
    let processed_segs: Vec<String> = processed.split('\x00').map(|s| s.to_string()).collect();

    // Defensive: split count must match text_segs count.
    debug_assert_eq!(
        processed_segs.len(),
        text_segs.len(),
        "process_raw_string must not remove the \\x00 separators"
    );

    // Phase 3: emit tokens.
    if expr_bufs.is_empty() {
        // No interpolations: emit as a plain STR (same as a raw string).
        let (_, _, span_end) = text_segs[0];
        let span = template_start..span_end;
        let s = processed_segs.into_iter().next().unwrap_or_default();
        tokens.push((aipl::Terminal::Str((s, span.clone())), span));
        return Ok(());
    }

    // Emit TEMPLATE_HEAD (first text segment).
    let (_, _, head_end) = text_segs[0];
    let head_span = template_start..head_end;
    tokens.push((
        aipl::Terminal::TemplateHead((processed_segs[0].clone(), head_span.clone())),
        head_span,
    ));

    // Emit alternating expression tokens + MIDDLE / TAIL.
    let n_exprs = expr_bufs.len();
    for (i, expr_buf) in expr_bufs.into_iter().enumerate() {
        tokens.extend(expr_buf);

        let (_, seg_s, seg_e) = text_segs[i + 1];
        let seg_span = seg_s..seg_e;
        if i + 1 < n_exprs {
            tokens.push((
                aipl::Terminal::TemplateMiddle((processed_segs[i + 1].clone(), seg_span.clone())),
                seg_span,
            ));
        } else {
            tokens.push((
                aipl::Terminal::TemplateTail((processed_segs[i + 1].clone(), seg_span.clone())),
                seg_span,
            ));
        }
    }

    Ok(())
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
pub fn lex_tokens(input: &str) -> Result<Vec<(TokenKind, Span)>, Error> {
    let input = strip_test_sections(input);
    let raw = tokenize(input)?;
    Ok(raw.into_iter().map(|(t, sp)| (classify(&t), sp)).collect())
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

/// Tokenize `input` for the formatter: every token plus the span of every
/// comment, both in source order (token text is recovered from the span, so
/// literals stay verbatim). Unlike [`lex_tokens`] the input is taken as-is —
/// no test-section stripping — because the formatter splits trailing
/// `--- section ---` blocks off itself and must account for every byte it is
/// given.
#[allow(clippy::type_complexity)]
pub fn lex_tokens_and_comments(
    input: &str,
) -> Result<(Vec<(FmtTokenKind, Span)>, Vec<Span>), Error> {
    COMMENT_SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
    // Disarm the sink on every exit path (including a tokenize error), so a
    // later plain parse on this thread doesn't keep recording.
    let result = tokenize(input);
    let comments = COMMENT_SINK
        .with(|sink| sink.borrow_mut().take())
        .expect("comment sink armed above");
    let raw = result?;
    let toks = raw
        .into_iter()
        .map(|(t, sp)| {
            use self::aipl::Terminal as T;
            let kind = match &t {
                T::TemplateHead(_) => FmtTokenKind::TemplateHead,
                T::TemplateMiddle(_) => FmtTokenKind::TemplateMiddle,
                T::TemplateTail(_) => FmtTokenKind::TemplateTail,
                other => FmtTokenKind::Plain(classify(other)),
            };
            (kind, sp)
        })
        .collect();
    Ok((toks, comments))
}

/// Tokenize `input` for the formatter's *preservation check*: each token as
/// `(kind, signature)` plus every comment span. A token's signature is its
/// **semantic value** — the lexer's processed string for a `"..."`/`"""`
/// literal or a template `` ` `` piece (i.e. after escape handling and
/// raw-string de-denting), and the raw source text for everything else. Two
/// spellings that lex to the same value therefore share a signature, so the
/// formatter's value-preserving whitespace edits (re-indenting a raw block's
/// content or its closing delimiter) don't register as changes, while any real
/// change to a literal's value does. Input is taken as-is (no section
/// stripping), like [`lex_tokens_and_comments`].
#[allow(clippy::type_complexity)]
pub fn lex_signatures_and_comments(
    input: &str,
) -> Result<(Vec<(FmtTokenKind, String)>, Vec<Span>), Error> {
    COMMENT_SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
    let result = tokenize(input);
    let comments = COMMENT_SINK
        .with(|sink| sink.borrow_mut().take())
        .expect("comment sink armed above");
    let raw = result?;
    let toks = raw
        .into_iter()
        .map(|(t, sp)| {
            use self::aipl::Terminal as T;
            let (kind, sig) = match &t {
                T::TemplateHead((v, _)) => (FmtTokenKind::TemplateHead, v.clone()),
                T::TemplateMiddle((v, _)) => (FmtTokenKind::TemplateMiddle, v.clone()),
                T::TemplateTail((v, _)) => (FmtTokenKind::TemplateTail, v.clone()),
                T::Str((v, _)) => (FmtTokenKind::Plain(TokenKind::Str), v.clone()),
                other => (
                    FmtTokenKind::Plain(classify(other)),
                    input[sp.clone()].to_string(),
                ),
            };
            (kind, sig)
        })
        .collect();
    Ok((toks, comments))
}

fn classify(t: &aipl::Terminal<Build>) -> TokenKind {
    // `aipl` as a bare path is ambiguous here: rustc has to choose between
    // the crate (this is the aipl crate) and the gazelle-generated module
    // of the same name. `self::aipl` pins it to the local module.
    use self::aipl::Terminal as T;
    match t {
        T::Fn | T::If | T::Else | T::Struct | T::Variant | T::Import | T::From | T::As | T::Pub
        | T::Let | T::For | T::While | T::Mut | T::Set | T::Match | T::Return | T::Builtins(_) => {
            TokenKind::Keyword
        }
        T::True(_) | T::False(_) | T::None(_) => TokenKind::Constant,
        T::Ident((s, _)) => match s.as_str() {
            "bool" | "char" | "str" | "any" => TokenKind::BuiltinType,
            _ if aipl_syntax::int_bits(s).is_some() => TokenKind::BuiltinType,
            _ => TokenKind::Identifier,
        },
        T::Num(_) => TokenKind::Number,
        T::Str(_) | T::TemplateHead(_) | T::TemplateMiddle(_) | T::TemplateTail(_) => {
            TokenKind::Str
        }
        T::Char(_) => TokenKind::Char,
        T::Op(_, _)
        | T::Minus(_)
        | T::Langle(_)
        | T::Rangle(_)
        | T::Oror(_)
        | T::Pipe(_)
        | T::Bang
        | T::Plusplus(_)
        | T::Arrow
        | T::Fatarrow
        // `..` — the slice range separator.
        | T::Dotdot
        // `=` (assignment in `let`/`mut`/`set`) is conventionally
        // `keyword.operator.assignment` in TextMate scopes — group it
        // with the other operators rather than with bracket punctuation.
        | T::Eq => TokenKind::Operator,
        T::Lparen | T::Rparen | T::Lbrace | T::Rbrace | T::Lbracket(_) | T::Rbracket
        | T::Hash(_) | T::Comma | T::Colon | T::Dot | T::Semi | T::Question => {
            TokenKind::Punctuation
        }
        // gazelle generates a private `__Phantom` variant for its internal
        // type-parameter use; it's unreachable from real input.
        _ => unreachable!("gazelle phantom terminal"),
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

pub fn parse(input: &str) -> Result<Program, Error> {
    let input = strip_test_sections(input);
    reject_trailing_whitespace(input)?;
    let mut parser = aipl::Parser::<Build>::new();
    let mut actions = Build;

    let pairs = tokenize(input)?;
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
        | ExprKind::Let(_, a, b)
        | ExprKind::LetMut(_, a, b)
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
        | ExprKind::KwArg(_, x) => bake_asserts(x, src),
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
