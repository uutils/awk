// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use bumpalo::Bump;

use crate::{Ast, Lexer, Parser, diagnostics::ParsingError};

fn parse<'a>(source: &'a str, arena: &'a Bump) -> super::Result<&'a Ast<'a>> {
    let parser = arena.alloc(Parser::new(arena));
    parser.parse_top(&mut Lexer::new(source.as_bytes(), arena), true)
}

fn parse_error_span(source: &str) -> std::ops::Range<usize> {
    let arena = Bump::new();
    let parser = arena.alloc(Parser::new(&arena));
    match parser.parse_top(&mut Lexer::new(source.as_bytes(), &arena), true) {
        Err(err) => err.span().expect("expected a span-bearing parse error"),
        Ok(_) => panic!("expected parse error for {source:?}"),
    }
}

fn spanned_snippet(source: &str, span: std::ops::Range<usize>) -> &str {
    std::str::from_utf8(&source.as_bytes()[span]).expect("span out of bounds")
}

// Behold! The Holy Macro to rule them all.
macro_rules! test_parser {
    (
        $code:expr => {
            $(loads:      $loads:expr,)?
            $(begin:      $begin:expr,)?
            $(end:        $end:expr,)?
            $(begin_file: $begin_file:expr,)?
            $(end_file:   $end_file:expr,)?
            $(rules:      $rules:expr,)?
            $(concurrent: $concurrent:expr,)?
            $(functions:  $functions:expr,)?
        }
    ) => {
        let arena = Bump::new();
        let code = parse($code, &arena).unwrap();

        #[allow(unused_mut, unused_assignments)]
        let _ = {
            use ::std::{option::Option, primitive::str, assert_eq, format};

            let mut loads:      &[&str]                         = &[];
            let mut end:        &[&str]                         = &[];
            let mut begin:      &[&str]                         = &[];
            let mut begin_file: &[&str]                         = &[];
            let mut end_file:   &[&str]                         = &[];
            let mut rules:      &[(Option<&str>, Option<&str>)] = &[];
            let mut concurrent: &[(Option<&str>, Option<&str>)] = &[];
            let mut functions:  &[(&str, &[&str], &str)]        = &[];

            $(loads      = &$loads;)?
            $(end        = &$end;)?
            $(begin      = &$begin;)?
            $(begin_file = &$begin_file;)?
            $(end_file   = &$end_file;)?
            $(rules      = &$rules;)?
            $(concurrent = &$concurrent;)?
            $(functions  = &$functions;)?

            test_parser!(
                @internal check |(a, b)| assert_eq!(a.as_bytes(), b.as_ref());
                loads => code.loads
            );
            test_parser!(
                @internal munch check_for_each code;
                |(&a, b)| assert_eq!(a, &format!("{b:?}"));
                begin, end, begin_file, end_file
            );
            test_parser!(
                @internal munch check_for_each code;
                |((e_pattern, e_actions), b)| {
                    assert_eq!(
                        *e_pattern,
                        b.pattern.as_ref().map(|x| format!("{x:?}")).as_deref()
                    );
                    assert_eq!(
                        *e_actions,
                        b.actions.as_ref().map(|x| format!("{x:?}")).as_deref()
                    );
                };
                rules, concurrent
            );
            test_parser!(
                @internal check |((e_name, e_args, e_body), (name, fun))| {
                    assert_eq!(e_name, &format!("{name:?}"));
                    test_parser!(@internal check
                        |(&a, b)| assert_eq!(a, &format!("{b:?}"));
                        e_args => fun.args
                    );
                    assert_eq!(*e_body, format!("{:?}", fun.body));
                };
                functions => code.functions
            );
        };
    };
    (is_err!($($code:expr),*)) => {
        let arena = Bump::new();
        assert!([$($code),*].into_iter().all(|e| parse(e, &arena).is_err()));
    };
    (@internal check $lambda:expr; $a:expr => $b:expr) => {
        assert_eq!($a.len(), $b.len());
        $a.into_iter().zip(&$b).for_each($lambda);
    };
    (@internal check_for_each $code:ident; $lambda:expr; $a:ident) => {
        test_parser!(@internal check $lambda; $a => $code.$a);
    };
    (@internal munch $method:ident $code:ident; $lambda:expr; $arg:ident, $($rest:tt)*) => {
        test_parser!(@internal $method $code; $lambda; $arg);
        test_parser!(@internal munch $method $code; $lambda; $($rest)*);
    };
    (@internal munch $method:ident $code:ident; $lambda:expr; $arg:ident) => {
        test_parser!(@internal $method $code; $lambda; $arg);
    };
    (@internal munch $method:ident $code:ident; $lambda:expr;) => {};
}

#[test]
fn test_parser_meta_holy_macro() {
    let source = "
        @load \"lib_foo.1\";
        @load \"lib_bar.so\";

        BEGIN { print 1 + 1 }
        BEGIN { 2 + 2 == 4\nprint \"foo\" }
        { if (a) print 2; }
        $0 == \"lisp would be proud\";
        function foo(a, b) { print a ? b : c }
    ";
    test_parser!(source => {
        loads: ["lib_foo.1", "lib_bar.so"],
        begin: [
            "(body (Print (Add 1 1)))",
            "(body (Eq (Add 2 2) 4) (Print \"foo\"))"
        ],
        rules: [
            (None, Some("(body (if awk::a (body (Print 2))))")),
            (Some("(Eq (Record 0) \"lisp would be proud\")"), None),
        ],
        functions: [
            (
                "awk::foo",
                &["awk::a", "awk::b"],
                "(body (Print (?: awk::a awk::b awk::c)))"
            )
        ],
    });
}

#[test]
fn test_parser_valid_patterns() {
    let source = "
        BEGIN { print }
        END { print }
        BEGINFILE { print }
        ENDFILE { print }
        $0 == 1 && /x/ { print }
        /abc/ { print }
        !$0, x::a ? b : c { print }
        awk;
        1 + 1 \n { print }
        { print }
        a in arr { print }
    ";
    const BODY: &str = "(body (Print))";
    test_parser!(source => {
        begin: [BODY],
        end: [BODY],
        begin_file: [BODY],
        end_file: [BODY],
        rules: [
            (Some("(And (Eq (Record 0) 1) /x/)"), Some(BODY)),
            (Some("/abc/"), Some(BODY)),
            (
                Some("(Range (Negation (Record 0)) (?: x::a awk::b awk::c))"),
                Some(BODY)
            ),
            (Some("awk::awk"), None),
            (Some("(Add 1 1)"), None),
            (None, Some(BODY)),
            (None, Some(BODY)),
            (Some("(In awk::arr awk::a)"), Some("(body (Print))")),
        ],
    });
}

#[test]
fn test_parser_invalid_patterns() {
    test_parser!(is_err!("BEGIN", "END", "BEGINFILE", "ENDFILE", "print 1;"));
}

#[test]
fn test_parser_statement_end_after_command() {
    test_parser!("{ print 1; { print 2 } } {;} { print; } { { print }; }" => {
        rules: [
            (None, Some("(body (Print 1) (Print 2))")),
            (None, Some("(body)")),
            (None, Some("(body (Print))")),
            (None, Some("(body (Print))"))
        ],
    });
    test_parser!(is_err!(
        "{ print 1 { 1 + 1 } }",
        "{ print 1 { } }",
        "{ printf \"%d\" 1 { } }"
    ));
}

#[test]
fn test_parser_statement_bodies() {
    let source = "
        { if (1); }
        { if (1); else; }
        { if (1) print; }
        { if (1) print; else; }
        { if (1); else print; }
        { for (;;); }
        { while (1); }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (if 1 (body)))")),
            (None, Some("(body (if 1 (body) (else (body))))")),
            (None, Some("(body (if 1 (body (Print))))")),
            (None, Some("(body (if 1 (body (Print)) (else (body))))")),
            (None, Some("(body (if 1 (body) (else (body (Print)))))")),
            (None, Some("(body (for (pass) (pass) (pass) (body)))")),
            (None, Some("(body (while 1 (body)))")),
        ],
    });
    test_parser!(is_err!(
        "{ if (1) }",
        "{ if (1); else }",
        "{ if (1) }",
        "{ if (1) 1 + 1 else }",
        "{ if (1) print 1 else }",
        "{ if (1) 1 + 1; else }",
        "{ if (1) print 1; else }",
        "{ while (1) }",
        "{ for (;;) }"
    ));
}

#[test]
fn test_parser_reserved_qualified_identifiers() {
    test_parser!(is_err!(
        "{ if::while }",
        "{ foo::while }",
        "{ while::foo }",
        "@namespace \"if\"; BEGIN {}",
        "function foo::while() {}",
        "function while::foo() {}"
    ));
}

#[test]
fn test_parser_non_assoc() {
    test_parser!(is_err!(
        "a == b == c",
        "a != b != c",
        "a > b > c",
        "a < b < c",
        "a >= b >= c",
        "a <= b <= c"
    ));
}

#[test]
fn test_parser_exponentiation() {
    let source = "
        { 2 ^ 1 }
        { 2 ** 1 }
        { 2 ^ 3 ^ 4 }
        { 2 * 3 ^ 4 }
        { 2 ^ 3 * 4 }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (Raise 2 1))")),
            (None, Some("(body (Raise 2 1))")),
            (None, Some("(body (Raise 2 (Raise 3 4)))")),
            (None, Some("(body (Multiply 2 (Raise 3 4)))")),
            (None, Some("(body (Multiply (Raise 2 3) 4))")),
        ],
    });
}

#[test]
fn test_parser_multiplicative_precedence() {
    let source = "
        { 2 * 3 % 4 }
        { 2 % 3 * 4 }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (Modulo (Multiply 2 3) 4))")),
            (None, Some("(body (Multiply (Modulo 2 3) 4))")),
        ],
    });
}

#[test]
fn test_parser_relaxed_assignments() {
    let source = "
        { 1 + 0 && x = 1 }
        { 1 + 0 || x = 1 }
        { 1 + 0 >= x = 1 }
        { 1 + 0 < x = 1 }
        { 1 + 0 ~ x = 1 }
        { 1 + 0 !~ x = 1 }
        { y = @/a/ ? b : c }
        { 1 + 0 || z = @/a/ ? b : c }
        { 1 + 0 || z = /a/ && b || c }
    ";
    test_parser!(
        source => {
            rules: [
                (None, Some("(body (And (Add 1 0) (Assignment awk::x 1)))")),
                (None, Some("(body (Or (Add 1 0) (Assignment awk::x 1)))")),
                (None, Some("(body (GtE (Add 1 0) (Assignment awk::x 1)))")),
                (None, Some("(body (Lt (Add 1 0) (Assignment awk::x 1)))")),
                (None, Some("(body (Matches (Add 1 0) (Assignment awk::x 1)))")),
                (None, Some("(body (MatchesNot (Add 1 0) (Assignment awk::x 1)))")),
                (None, Some("(body (?: (Assignment awk::y @/a/) awk::b awk::c))")),
                (None, Some("(body (?: (Or (Add 1 0) (Assignment awk::z @/a/)) awk::b awk::c))")),
                (
                    None,
                    Some("(body (Or (Add 1 0) (Assignment awk::z (Or (And /a/ awk::b) awk::c))))"),
                ),
            ],
        }
    );
    test_parser!(is_err!("{ 1 + x = 1 }", "{ x y = 1 }", "{ 1 + 2 * x = 1 }"));
}

#[test]
fn test_parser_inc_dec() {
    let source = r#"
        { ++a $0-- }
        { --a[2] ++$(1 + 1) }
        { a++ a["x"]-- }
        { --a $"a"++ }
        { --a/a++ a++/--a }
    "#;
    test_parser!(source => {
        rules: [
            (None, Some("(body (Concat (IncrementL awk::a) (DecrementR (Record 0))))")),
            (
                None,
                Some(
                    "(body (Concat (DecrementL (Index awk::a 2)) (IncrementL (Record (Add 1 1)))))"
                )
            ),
            (None, Some("(body (Concat (IncrementR awk::a) (DecrementR (Index awk::a \"x\"))))")),
            (None, Some("(body (Concat (DecrementL awk::a) (IncrementR (Record \"a\"))))")),
            (None, Some("(body (Concat (Divide (DecrementL awk::a) (IncrementR awk::a)) \
                                       (Divide (IncrementR awk::a) (DecrementL awk::a))))"))
        ],
    });
    // these should parse as (Cat (--L (++R $0)) a), or otherwise error out.
    // FIXME: not treated as errors yet.
    // test_parser!(is_err!("{ $0++ --a }", "{ ++$0 ++a }"));
}

#[test]
fn test_parser_arrays() {
    let source = "
        { a[1]; a[1] = x = b[2] = 2 + 2 }
        { ++a[1]; print b[a]-- }
        { a[1, 2, 3, \"a\"] += 1 }
        { print a in arr, (1, 2, \"a\") in arr }
        { print $((1, 2) in a) }
    ";
    test_parser!(source => {
        rules: [
            (
                None,
                Some("(body (Index awk::a 1) \
                    (Assignment (Index awk::a 1) \
                    (Assignment awk::x (Assignment (Index awk::b 2) (Add 2 2)))))"
                )
            ),
            (
                None,
                Some("(body (IncrementL (Index awk::a 1)) \
                    (Print (DecrementR (Index awk::b awk::a))))"
                )
            ),
            (None, Some("(body (AddAssign (Index awk::a 1 2 3 \"a\") 1))")),
            (None, Some("(body (Print (In awk::arr awk::a) (In awk::arr 1 2 \"a\")))")),
            (None, Some("(body (Print (Record (In awk::a 1 2))))")),
        ],
    });

    test_parser!(is_err!(
        "$(1, 2) in arr",
        "x in 2",
        "2[1]",
        "\"a\"[2]",
        "2 in a = 1"
    ));
}

#[test]
fn test_parser_nested_arrays() {
    let source = "
        { a[1][2] }
        { a[1][2][3] }
        { a[1][2] = 2 }
        { a[1][2][3] = 2 }
        { a[1][2][3][4][5] = 2 }
        { ++a[1][2] }
        { a[1][2]-- }
        { a[1][2] += 1 }
        { b = a[1][2] }
        { b = a[1][2][3] }
        { a[1, 2][3] }
        { a[1][2, 3] }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (Index (Index awk::a 1) 2))")),
            (None, Some("(body (Index (Index (Index awk::a 1) 2) 3))")),
            (None, Some("(body (Assignment (Index (Index awk::a 1) 2) 2))")),
            (None, Some("(body (Assignment (Index (Index (Index awk::a 1) 2) 3) 2))")),
            (None, Some("(body (Assignment (Index (Index (Index (Index (Index awk::a 1) 2) 3) 4) 5) 2))")),
            (None, Some("(body (IncrementL (Index (Index awk::a 1) 2)))")),
            (None, Some("(body (DecrementR (Index (Index awk::a 1) 2)))")),
            (None, Some("(body (AddAssign (Index (Index awk::a 1) 2) 1))")),
            (None, Some("(body (Assignment awk::b (Index (Index awk::a 1) 2)))")),
            (None, Some("(body (Assignment awk::b (Index (Index (Index awk::a 1) 2) 3)))")),
            (None, Some("(body (Index (Index awk::a 1 2) 3))")),
            (None, Some("(body (Index (Index awk::a 1) 2 3))")),
        ],
    });

    test_parser!(is_err!(
        "{ 2[1][2] }",
        "{ \"a\"[1][2] }",
        "{ (a + b)[1][2] }"
    ));
}

#[test]
fn test_parser_for_loop() {
    let source = "
        { for (i = 0; i < n; i++) print }
        { for (; i < n; i++) print }
        { for (i = 0; ; i++) print }
        { for (i = 0; i < n;) print }
        { for (;; i++) print }
        { for (; i < n;) print }
        { for (i = 0; ;) print }
        { for (;;) print }
        { for ((i in arr); a; b) print }
        { for (((i, 2) in arr); ;) print }
        { for (k in array) print }
    ";
    test_parser!(
        source => {
            rules: [
                (
                    None,
                    Some("(body (for (Assignment awk::i 0) (Lt awk::i awk::n) (IncrementR awk::i) \
                        (body (Print))))")
                ),
                (
                    None,
                    Some("(body (for (pass) (Lt awk::i awk::n) (IncrementR awk::i) \
                        (body (Print))))")
                ),
                (
                    None,
                    Some("(body (for (Assignment awk::i 0) (pass) (IncrementR awk::i) \
                        (body (Print))))")
                ),
                (
                    None,
                    Some("(body (for (Assignment awk::i 0) (Lt awk::i awk::n) (pass) \
                        (body (Print))))")
                ),
                (None, Some("(body (for (pass) (pass) (IncrementR awk::i) (body (Print))))")),
                (None, Some("(body (for (pass) (Lt awk::i awk::n) (pass) (body (Print))))")),
                (None, Some("(body (for (Assignment awk::i 0) (pass) (pass) (body (Print))))")),
                (None, Some("(body (for (pass) (pass) (pass) (body (Print))))")),
                (None, Some("(body (for (In awk::arr awk::i) awk::a awk::b (body (Print))))")),
                (None, Some("(body (for (In awk::arr awk::i 2) (pass) (pass) (body (Print))))")),
                (None, Some("(body (for-each awk::k awk::array (body (Print))))")),
            ],
        }
    );

    test_parser!(is_err!("{ for(x in array; a; b) {} }"));
}

#[test]
fn test_parser_logical_operators() {
    let source = r"
        { a && b && c == 3 }
        { a || b > 2 || c }
        { 1 ~ /a/ || b && c }
        { !a }
        { !(a && b) }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (And (And awk::a awk::b) (Eq awk::c 3)))")),
            (None, Some("(body (Or (Or awk::a (Gt awk::b 2)) awk::c))")),
            (None, Some("(body (Or (Matches 1 @/a/) (And awk::b awk::c)))")),
            (None, Some("(body (Negation awk::a))")),
            (None, Some("(body (Negation (And awk::a awk::b)))")),
        ],
    });
}

#[test]
fn test_parser_delete() {
    let source = r"
        { delete arr[k] }
        { delete arr[i, j] }
        { delete arr }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (delete (Index awk::arr awk::k)))")),
            (None, Some("(body (delete (Index awk::arr awk::i awk::j)))")),
            (None, Some("(body (delete awk::arr))")),
        ],
    });
}

#[test]
fn test_parser_if() {
    let source = r"
        { if (a) print }
        { if (k in arr) print; else if (x) print; else print; }
        { if (x == 1 && 2) print 1; else print 0 }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (if awk::a (body (Print))))")),
            (
                None,
                Some("(body (if (In awk::arr awk::k) (body (Print)) (else (body (if awk::x \
                (body (Print)) (else (body (Print))))))))")
            ),
            (
                None,
                Some("(body (if (And (Eq awk::x 1) 2) (body (Print 1)) (else (body (Print 0)))))")
            ),
        ],
    });
}

#[test]
fn test_parser_dangling_else() {
    let source = "{ if (a) if (b) print 1; else print 2 }";
    test_parser!(source => {
        rules: [
            (None, Some(
                "(body (if awk::a (body (if awk::b (body (Print 1)) (else (body (Print 2)))))))"
            )),
        ],
    });
}

#[test]
fn test_parser_while() {
    let source = r"
        { while (a < 10) a++ }
        { while (1) { print a; if (a > 5) break } }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (while (Lt awk::a 10) (body (IncrementR awk::a))))")),
            (None, Some(concat!(
                "(body (while 1 ",
                "(body (Print awk::a) (if (Gt awk::a 5) (body (break))))))"
            ))),
        ],
    });
}

#[test]
fn test_parser_do_while() {
    let source = r"
        { do { a++ } while (a < 10) }
        { do { print; break } while (a--) }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (do-while (body (IncrementR awk::a)) (Lt awk::a 10)))")),
            (None, Some("(body (do-while (body (Print) (break)) (DecrementR awk::a)))")),
        ],
    });
}

#[test]
fn test_parser_pratt_error_spans() {
    let source = "{ 1 = x }";
    let span = parse_error_span(source);
    assert!(matches!(
        parse(source, &Bump::new()),
        Err(ParsingError::OperatorExpectsVariable(_))
    ));
    assert_eq!(spanned_snippet(source, span), "1 =");

    let source = "{ ++1 }";
    let span = parse_error_span(source);
    assert_eq!(spanned_snippet(source, span), "++1");

    let source = "{ 1[2] }";
    let span = parse_error_span(source);
    assert_eq!(spanned_snippet(source, span), "1[");

    let source = "{ 1 + 2 = x }";
    let span = parse_error_span(source);
    assert_eq!(spanned_snippet(source, span), "1 + 2 =");

    let source = "{ x in 2 }";
    let span = parse_error_span(source);
    assert_eq!(spanned_snippet(source, span), "2");

    // Non-associative operators keep the offending token's span.
    let source = "{ a == b == c }";
    let span = parse_error_span(source);
    assert!(matches!(
        parse(source, &Bump::new()),
        Err(ParsingError::NonAssociativeOperator(_))
    ));
    assert_eq!(spanned_snippet(source, span), "==");

    let source = "{ (1 + 2 }";
    let span = parse_error_span(source);
    assert!(matches!(
        parse(source, &Bump::new()),
        Err(ParsingError::UnclosedParenthesisExpression(_))
    ));
    assert_eq!(spanned_snippet(source, span), "(1 + 2 }");

    let source = "{ a[1 }";
    let span = parse_error_span(source);
    assert!(matches!(
        parse(source, &Bump::new()),
        Err(ParsingError::UnclosedArrayAccess(_))
    ));
    assert_eq!(spanned_snippet(source, span), "a[1 }");
}

#[test]
fn test_parser_switch() {
    let source = r#"
        { switch (x) { case 1: print; case "a": print 2; default: print 3 } }
        { switch (1 + 1) { case /pat/: print } }
    "#;
    test_parser!(source => {
        rules: [
            (
                None,
                Some(concat!(
                    "(body (switch awk::x",
                    " (case 1 (body (Print)))",
                    " (case \"a\" (body (Print 2)))",
                    " (default 2 (body (Print 3)))))"
                ))
            ),
            (
                None,
                Some("(body (switch (Add 1 1) (case /pat/ (body (Print)))))")
            ),
        ],
    });

    test_parser!(is_err!(
        "{ switch (x) { default: print; default: print } }",
        "{ switch (x) { case a: print } }"
    ));
}

#[test]
fn test_parser_getline() {
    let source = r#"
        { getline }
        { getline x }
        { getline < "f" }
        { getline x < "f" }
        { "cmd" | getline }
        { "cmd" | getline x }
        { "cmd" |& getline }
        { "cmd" |& getline x }
    "#;
    test_parser!(source => {
        rules: [
            (None, Some("(body (getline))")),
            (None, Some("(body (getline awk::x))")),
            (None, Some("(body (getline< \"f\"))")),
            (None, Some("(body (getline< \"f\" awk::x))")),
            (None, Some("(body (getline| \"cmd\"))")),
            (None, Some("(body (getline| \"cmd\" awk::x))")),
            (None, Some("(body (getline|& \"cmd\"))")),
            (None, Some("(body (getline|& \"cmd\" awk::x))")),
        ],
    });
}

#[test]
fn test_parser_printf() {
    let source = r#"
        { printf "%d", 1, 2 }
        { printf("%d", 1) }
    "#;
    test_parser!(source => {
        rules: [
            (None, Some("(body (Printf \"%d\" 1 2))")),
            (None, Some("(body (Printf \"%d\" 1))")),
        ],
    });
}

#[test]
fn test_parser_control_flow() {
    let source = r"
        { next }
        { nextfile }
        { continue }
        { return }
        { return 1 }
        { exit }
        { exit 1 }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (next))")),
            (None, Some("(body (nextfile))")),
            (None, Some("(body (continue))")),
            (None, Some("(body (return))")),
            (None, Some("(body (return 1))")),
            (None, Some("(body (exit))")),
            (None, Some("(body (exit 1))")),
        ],
    });
}

#[test]
fn test_parser_redirection() {
    let source = r#"
        { print > "out" }
        { print >> "out" }
        { print | "cmd" }
        { print |& "cmd" }
    "#;
    test_parser!(source => {
        rules: [
            (None, Some("(body (Print (> \"out\")))")),
            (None, Some("(body (Print (>> \"out\")))")),
            (None, Some("(body (Print (| \"cmd\")))")),
            (None, Some("(body (Print (|& \"cmd\")))")),
        ],
    });
}

#[test]
fn test_parser_regex_matching() {
    let source = r"
        { a ~ /b/ }
        { a !~ /b/ }
        { /x/ ~ /y/ }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (Matches awk::a @/b/))")),
            (None, Some("(body (MatchesNot awk::a @/b/))")),
            (None, Some("(body (Matches /x/ @/y/))")),
        ],
    });
}

#[test]
fn test_parser_function_calls() {
    let source = r"
        { foo(1, 2) }
        { @bar(3) }
        { foo::baz(a, b + 1) }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (awk::foo 1 2))")),
            (None, Some("(body (@awk::bar 3))")),
            (None, Some("(body (foo::baz awk::a (Add awk::b 1)))")),
        ],
    });
}

#[test]
fn test_parser_compound_assignments() {
    let source = r"
        { a -= 1 }
        { a *= 2 }
        { a /= 3 }
        { a ^= 4 }
        { a %= 5 }
        { a /= /pat/ }
    ";
    test_parser!(source => {
        rules: [
            (None, Some("(body (SubAssign awk::a 1))")),
            (None, Some("(body (MulAssign awk::a 2))")),
            (None, Some("(body (DivAssign awk::a 3))")),
            (None, Some("(body (PowAssign awk::a 4))")),
            (None, Some("(body (ModAssign awk::a 5))")),
            (None, Some("(body (DivAssign awk::a /pat/))")),
        ],
    });
}

#[test]
fn test_parser_builtin_variables() {
    let source = r"
        { NR; NF; FS; RS; OFS; ORS; FILENAME; ARGC; ARGV; SUBSEP; FNR; OFMT; RSTART; RLENGTH; ENVIRON }
    ";
    test_parser!(source => {
        rules: [(
            None,
            Some(concat!(
                "(body NR NF FS RS OFS ORS FILENAME ARGC ARGV SUBSEP FNR OFMT ",
                "RSTART RLENGTH ENVIRON)"
            ))
        )],
    });
}

#[test]
fn test_parser_concurrent() {
    let source = r"
        @concurrent { print 1 }
        @concurrent $1 { print 2 }
    ";
    test_parser!(source => {
        concurrent: [
            (None, Some("(body (Print 1))")),
            (Some("(Record 1)"), Some("(body (Print 2))")),
        ],
    });
}

#[test]
fn test_parser_namespace_directive() {
    let source = r#"
        @namespace "myns";
        function bar() { print }
    "#;
    test_parser!(source => {
        functions: [("myns::bar", &[], "(body (Print))")],
    });
}

#[test]
fn test_parser_unary_and_divide() {
    let source = r"
        { -a; +a; !a; a / b; a + b - c }
        { int(a) }
    ";
    test_parser!(source => {
        rules: [
            (
                None,
                Some(concat!(
                    "(body (Negative awk::a) (ToInt awk::a) (Negation awk::a) ",
                    "(Divide awk::a awk::b) (Subtract (Add awk::a awk::b) awk::c))"
                ))
            ),
            (None, Some("(body (Int awk::a))")),
        ],
    });
}

#[test]
fn test_pretty_print_omits_default_namespace() {
    use std::fmt::Write;

    let arena = Bump::new();
    let parser = arena.alloc(Parser::new(&arena));
    let source = "{ print a + b }";
    let ast = parser.parse("test", source.as_bytes()).unwrap();
    let mut printed = String::new();
    write!(printed, "{ast}").unwrap();
    assert!(
        !printed.contains("awk::"),
        "pretty-print should omit the default awk namespace: {printed}"
    );
    assert!(printed.contains("print a"));
}

#[test]
fn test_pretty_print_namespace_directive() {
    use std::fmt::Write;

    let arena = Bump::new();
    let parser = arena.alloc(Parser::new(&arena));
    let source = r#"
        @namespace "myns";
        function bar(x) { print x }
    "#;
    let ast = parser.parse("test", source.as_bytes()).unwrap();
    let mut printed = String::new();
    write!(printed, "{ast}").unwrap();
    assert!(
        printed.contains("@namespace \"myns\""),
        "pretty-print should restore @namespace directive: {printed}"
    );
    assert!(
        !printed.contains("myns::"),
        "pretty-print should use unqualified names under @namespace: {printed}"
    );
}

#[test]
fn test_pretty_print_namespace_roundtrip() {
    use std::fmt::Write;

    let arena = Bump::new();
    let parser = arena.alloc(Parser::new(&arena));
    let source = r#"
        @namespace "myns";
        function bar() { x = 1 }
        { print y }
    "#;
    let ast = parser.parse("test", source.as_bytes()).unwrap();
    let mut printed = String::new();
    write!(printed, "{ast}").unwrap();

    let arena2 = Bump::new();
    let parser2 = arena2.alloc(Parser::new(&arena2));
    let reparsed = parser2.parse("test", printed.as_bytes()).unwrap();
    assert_eq!(ast.functions.len(), reparsed.functions.len());
    assert_eq!(ast.rules.len(), reparsed.rules.len());
}
