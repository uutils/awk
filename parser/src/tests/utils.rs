// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::fmt::Write;

use bumpalo::Bump;

use crate::{Ast, Lexer, Parser, Result};

pub fn parse<'a>(source: &'a str, arena: &'a Bump) -> Result<&'a Ast<'a>> {
    let parser = arena.alloc(Parser::new(arena, true));
    parser.parse_top(&mut Lexer::new(source.as_bytes(), arena), true)
}

pub fn parse_error_span(source: &str) -> std::ops::Range<usize> {
    let arena = Bump::new();
    let parser = arena.alloc(Parser::new(&arena, true));
    match parser.parse_top(&mut Lexer::new(source.as_bytes(), &arena), true) {
        Err(err) => err.span().expect("expected a span-bearing parse error"),
        Ok(_) => panic!("expected parse error for {source:?}"),
    }
}

pub fn spanned_snippet(source: &str, span: std::ops::Range<usize>) -> &str {
    std::str::from_utf8(&source.as_bytes()[span]).expect("span out of bounds")
}

// Behold! The Holy Macro to rule them all.
#[macro_export]
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
        let code = $crate::tests::utils::parse($code, &arena).unwrap();

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
        assert!([$($code),*].into_iter().all(|e| $crate::tests::utils::parse(e, &arena).is_err()));
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

/// Canonical `Debug` fingerprint used to compare ASTs across round-trips.
pub fn ast_signature(ast: &Ast<'_>) -> String {
    let mut out = String::new();
    for load in &ast.loads {
        let _ = writeln!(out, "load:{load:?}");
    }
    for body in &ast.begin {
        let _ = writeln!(out, "begin:{body:?}");
    }
    for body in &ast.begin_file {
        let _ = writeln!(out, "begin_file:{body:?}");
    }
    for rule in &ast.rules {
        let pattern = rule.pattern.as_ref().map(|p| format!("{p:?}"));
        let actions = rule.actions.as_ref().map(|a| format!("{a:?}"));
        let _ = writeln!(out, "rule:({pattern:?},{actions:?})");
    }
    for rule in &ast.concurrent {
        let pattern = rule.pattern.as_ref().map(|p| format!("{p:?}"));
        let actions = rule.actions.as_ref().map(|a| format!("{a:?}"));
        let _ = writeln!(out, "concurrent:({pattern:?},{actions:?})");
    }
    for body in &ast.end_file {
        let _ = writeln!(out, "end_file:{body:?}");
    }
    for body in &ast.end {
        let _ = writeln!(out, "end:{body:?}");
    }
    for (name, fun) in &ast.functions {
        let _ = writeln!(out, "function:{name:?}:{fun:?}");
    }
    out
}

pub fn parse_top<'a>(source: &'a str, arena: &'a Bump) -> Result<&'a Ast<'a>> {
    let parser = arena.alloc(Parser::new(arena, true));
    parser.parse_top(&mut Lexer::new(source.as_bytes(), arena), true)
}

/// Pretty-print `ast`, parse the result, and require both signatures to match.
pub fn roundtrip_ast(ast: &Ast<'_>) -> Result<(), String> {
    let mut printed = String::new();
    write!(printed, "{ast}").map_err(|e| format!("display failed: {e}"))?;

    let arena = Bump::new();
    let reparsed = parse_top(&printed, &arena).map_err(|e| format!("re-parse failed: {e}"))?;

    let before = ast_signature(ast);
    let after = ast_signature(reparsed);
    if before != after {
        return Err(format!(
            "AST mismatch after round-trip\nprinted:\n{printed}\nbefore:\n{before}\nafter:\n{after}"
        ));
    }
    Ok(())
}

pub fn roundtrip_source(source: &str) -> Result<(), String> {
    let arena = Bump::new();
    let ast = parse_top(source, &arena).map_err(|e| format!("initial parse failed: {e}"))?;
    roundtrip_ast(ast)
}
