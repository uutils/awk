// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::io::Write;

use bumpalo::{
    Bump,
    collections::{CollectIn, Vec},
};

use crate::{Identifier, Token};

fn lex<'a>(
    src: &'a [u8],
    arena: &'a Bump,
    posix_strict: bool,
    gnu_strict: bool,
) -> Vec<'a, Token<'a>> {
    Token::lex(src, arena, posix_strict, gnu_strict)
        .collect_in::<Result<Vec<_>, _>>(arena)
        .unwrap()
}

#[test]
fn lexer_test_newlines_non_posix() {
    let mixed = " \t \n \t\n\n \\\n \t";
    let arena = Bump::new();
    let mut str = Vec::new_in(&arena);
    for tok in ["BEGIN", "{", "else", "do", "&&", "||", "?", ":", ","] {
        write!(str, "{tok}{mixed}").unwrap();
    }
    str.push(b'}');
    assert_eq!(
        &lex(&str, &arena, false, false),
        &[
            Token::BeginPattern,
            Token::Newline,
            Token::Newline,
            Token::OpenBrace,
            Token::Else,
            Token::Do,
            Token::BooleanAnd,
            Token::BooleanOr,
            Token::QuestionMark,
            Token::Colon,
            Token::Comma,
            Token::ClosedBrace
        ]
    );
}

#[test]
#[should_panic]
fn lexer_test_newlines_posix() {
    let mixed = " \t \n \t\n\n \\\n \t";
    let arena = Bump::new();
    let mut str = Vec::new_in(&arena);
    for tok in ["BEGIN", "{", "else", "do", "&&", "||", "?", ":", ","] {
        write!(str, "{tok}{mixed}").unwrap();
    }
    str.push(b'}');
    lex(&str, &arena, true, false);
}

#[test]
fn lexer_test_collapsible_delimiters() {
    let arena = Bump::new();
    let str = b";\\\n;\n\n\n\n;;\n\\\n\n";
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::Semicolon,
            Token::Semicolon,
            Token::Newline,
            Token::Semicolon,
            Token::Semicolon,
            Token::Newline,
            Token::Newline,
        ]
    );
}

#[test]
fn lexer_test_multiline() {
    let arena = Bump::new();
    let str = b"\"aaaa\\\nbbbb\", /ccc\\\nd/";
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::String(b"aaaabbbb".into()),
            Token::Comma,
            Token::Regex(b"cccd".into())
        ]
    );
}

#[test]
fn lexer_test_uu_extensions() {
    let arena = Bump::new();
    assert_eq!(
        lex(b"@concurrent", &arena, false, true),
        &[Token::IndirectCall(Identifier {
            namespace: None,
            literal: "concurrent"
        })]
    );
}

#[test]
fn lexer_test_gnu_pattern() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"BEGINFILE ENDFILE", &arena, true, false),
        &[
            Token::Identifier(Identifier { namespace: None, literal: "BEGINFILE" }),
            Token::Identifier(Identifier { namespace: None, literal: "ENDFILE" })
        ]
    );
}

#[test]
fn lexer_test_nums() {
    let arena = Bump::new();
    let str = b"1 20. 0. .3 2e4 -3.e2 5e+1 2.1e-3 -2147483649 -128 -0 127 2147483648";
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::Integer(1),
            Token::Number(20.),
            Token::Number(0.),
            Token::Number(0.3),
            Token::Number(2e4),
            Token::Number(-3e2),
            Token::Number(5e1),
            Token::Number(2.1e-3),
            Token::Number(-2_147_483_649.),
            Token::Integer(-128),
            Token::Integer(0),
            Token::Integer(127),
            Token::Number(2_147_483_648.)
        ]
    );
}

#[test]
fn lexer_test_directive_escaping() {
    let arena = Bump::new();
    let str = br#" @include "aa\"a\ta" @nsinclude "b\"\nb" "#;
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::IncludeDirective,
            Token::String(b"aa\"a\ta".into()),
            Token::NsIncludeDirective,
            Token::String(b"b\"\nb".into())
        ]
    );
}

#[test]
fn lexer_test_ident_rules_non_posix() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"1a::a a::1a _a", &arena, false, false),
        &[
            Token::Integer(1),
            Token::Identifier(Identifier { namespace: Some("a"), literal: "a" }),
            Token::Identifier(Identifier { namespace: None, literal: "a" }),
            Token::Colon,
            Token::Colon,
            Token::Integer(1),
            Token::Identifier(Identifier { namespace: None, literal: "a" }),
            Token::Identifier(Identifier { namespace: None, literal: "_a" })
        ]
    );
}

#[test]
#[should_panic]
fn lexer_test_ident_rules_posix() {
    let arena = Bump::new();
    lex(b"@namespace \"foo\"; foo::a", &arena, true, false);
}

#[test]
fn lexer_test_general_tokens() {
    let arena = Bump::new();
    let str = br#"
        @load "lib1.so.1"
        BEGIN { print a + 1 }
        /2\..*/;
        END { $1 == foo::bar }
    "#;
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::Newline,
            Token::LoadDirective,
            Token::String(b"lib1.so.1".into()),
            Token::Newline,
            Token::BeginPattern,
            Token::OpenBrace,
            Token::Print,
            Token::Identifier(Identifier { namespace: None, literal: "a" }),
            Token::Plus,
            Token::Integer(1),
            Token::ClosedBrace,
            Token::Newline,
            Token::Regex(b"2\\..*".into()),
            Token::Semicolon,
            Token::Newline,
            Token::EndPattern,
            Token::OpenBrace,
            Token::Record,
            Token::Integer(1),
            Token::EqualTo,
            Token::Identifier(Identifier { namespace: Some("foo"), literal: "bar" }),
            Token::ClosedBrace,
            Token::Newline
        ]
    );
}

#[test]
fn lexer_test_regex_ambiguity() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"1/=1. a/=1", &arena, false, false),
        &[
            Token::Integer(1),
            Token::SlashAssign,
            Token::Number(1.),
            Token::Identifier(Identifier { namespace: None, literal: "a" }),
            Token::SlashAssign,
            Token::Integer(1)
        ]
    );
}

#[test]
fn lexer_test_hex_escape() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\x41\"", &arena, false, false),
        &[Token::String(b"A".into())]
    );
}

#[test]
fn lexer_test_hex_escape_uppercase() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\x4F\"", &arena, false, false),
        &[Token::String(b"O".into())]
    );
}

#[test]
fn lexer_test_hex_escape_single_digit() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\x9\"", &arena, false, false),
        &[Token::String(b"\x09".into())]
    );
}

#[test]
fn lexer_test_hex_escape_posix_strict() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\x41\"", &arena, true, false),
        &[Token::String(b"x41".into())]
    );
}

#[test]
fn lexer_test_unicode_escape_posix_strict() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\u0041\"", &arena, true, false),
        &[Token::String(b"u0041".into())]
    );
}

#[test]
fn lexer_test_unicode_escape_ascii() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\u0041\"", &arena, false, false),
        &[Token::String(b"A".into())]
    );
}

#[test]
fn lexer_test_unicode_escape_two_byte() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\u00e9\"", &arena, false, false),
        &[Token::String("\u{00e9}".as_bytes().into())]
    );
}

#[test]
fn lexer_test_unicode_escape_three_byte() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\u4e2d\"", &arena, false, false),
        &[Token::String("\u{4e2d}".as_bytes().into())]
    );
}

#[test]
fn lexer_test_unicode_escape_uppercase() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\u004F\"", &arena, false, false),
        &[Token::String(b"O".into())]
    );
}

#[test]
fn lexer_test_unicode_escape_single_digit() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\u9\"", &arena, false, false),
        &[Token::String("\u{9}".as_bytes().into())]
    );
}

#[test]
fn lexer_test_hex_escape_no_digits() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\x\"", &arena, false, false),
        &[Token::String(b"x".into())]
    );
}

#[test]
fn lexer_test_unicode_escape_no_digits() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\u\"", &arena, false, false),
        &[Token::String(b"u".into())]
    );
}

#[test]
fn lexer_test_unicode_escape_eight_digits() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\u00000032\"", &arena, false, false),
        &[Token::String(b"2".into())]
    );
}

#[test]
fn lexer_test_operators() {
    let arena = Bump::new();
    let str = b"~ !~ | |& >> ** **= ^= += -= *= %= == != <= >= < > && || !";
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::Matching,
            Token::NotMatching,
            Token::Pipe,
            Token::DoublePipe,
            Token::AppendPipe,
            Token::Circumflex,
            Token::CaretAssign,
            Token::CaretAssign,
            Token::PlusAssign,
            Token::MinusAssign,
            Token::StarAssign,
            Token::PercentAssign,
            Token::EqualTo,
            Token::NotEqualTo,
            Token::LesserOrEqualThan,
            Token::GreaterOrEqualThan,
            Token::LesserThan,
            Token::GreaterThan,
            Token::BooleanAnd,
            Token::BooleanOr,
            Token::Negation,
        ]
    );
}

#[test]
fn lexer_test_slash_assign() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"a/=1", &arena, false, false),
        &[
            Token::Identifier(Identifier { namespace: None, literal: "a" }),
            Token::SlashAssign,
            Token::Integer(1),
        ]
    );
}

#[test]
fn lexer_test_control_flow_keywords() {
    let arena = Bump::new();
    let str = b"switch case default getline printf next nextfile exit return continue break";
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::Switch,
            Token::Case,
            Token::Default,
            Token::Getline,
            Token::Printf,
            Token::Next,
            Token::NextFile,
            Token::Exit,
            Token::Return,
            Token::Continue,
            Token::Break,
        ]
    );
}

#[test]
fn lexer_test_builtin_variables() {
    let arena = Bump::new();
    let str =
        b"NR NF FS RS OFS ORS FILENAME ARGC ARGV SUBSEP FNR ARGIND OFMT RSTART RLENGTH ENVIRON";
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::NrVariable,
            Token::NfVariable,
            Token::FsVariable,
            Token::RsVariable,
            Token::OfsVariable,
            Token::OrsVariable,
            Token::FilenameVariable,
            Token::ArgcVariable,
            Token::ArgvVariable,
            Token::SubsepVariable,
            Token::FnrVariable,
            Token::ArgindVariable,
            Token::OfmtVariable,
            Token::RstartVariable,
            Token::RlengthVariable,
            Token::EnvironVariable,
        ]
    );
}

#[test]
fn lexer_test_typed_regex() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"@/pat/", &arena, false, false),
        &[Token::TypedRegex(b"pat".into())]
    );
}

#[test]
fn lexer_test_comments() {
    let arena = Bump::new();
    let str = b"print 1 # comment\nprint 2";
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::Print,
            Token::Integer(1),
            Token::Newline,
            Token::Print,
            Token::Integer(2),
        ]
    );
}

#[test]
fn lexer_test_indirect_call() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"@foo @ns::bar", &arena, false, false),
        &[
            Token::IndirectCall(Identifier { namespace: None, literal: "foo" }),
            Token::IndirectCall(Identifier { namespace: Some("ns"), literal: "bar" }),
        ]
    );
}

#[test]
#[should_panic]
fn lexer_test_indirect_call_posix() {
    let arena = Bump::new();
    lex(b"@foo", &arena, true, false);
}

#[test]
fn lexer_test_concurrent_directive() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"@concurrent", &arena, false, false),
        &[Token::ConcurrentDirective]
    );
}

#[test]
fn lexer_test_load_and_namespace_directives() {
    let arena = Bump::new();
    assert_eq!(
        &lex(br#"@load "lib.so" @namespace "ns""#, &arena, false, false),
        &[
            Token::LoadDirective,
            Token::String(b"lib.so".into()),
            Token::NamespaceDirective,
            Token::String(b"ns".into()),
        ]
    );
}

#[test]
fn lexer_test_regex_literals() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"/abc/", &arena, false, false),
        &[Token::Regex(b"abc".into())]
    );
    assert_eq!(
        &lex(b"/a\\/b/", &arena, false, false),
        &[Token::Regex(b"a/b".into())]
    );
    assert_eq!(
        &lex(b"x~/dot+/", &arena, false, false),
        &[
            Token::Identifier(Identifier { namespace: None, literal: "x" }),
            Token::Matching,
            Token::Regex(b"dot+".into()),
        ]
    );
}

#[test]
fn lexer_test_switch_snippet() {
    let arena = Bump::new();
    let str = br"switch (x) { case 1: print; default: break }";
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::Switch,
            Token::OpenParent,
            Token::Identifier(Identifier { namespace: None, literal: "x" }),
            Token::ClosedParent,
            Token::OpenBrace,
            Token::Case,
            Token::Integer(1),
            Token::Colon,
            Token::Print,
            Token::Semicolon,
            Token::Default,
            Token::Colon,
            Token::Break,
            Token::ClosedBrace,
        ]
    );
}

#[test]
fn lexer_test_getline_redirection() {
    let arena = Bump::new();
    let str = br#"getline getline x < "f" "cmd" | getline "cmd" |& getline"#;
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::Getline,
            Token::Getline,
            Token::Identifier(Identifier { namespace: None, literal: "x" }),
            Token::LesserThan,
            Token::String(b"f".into()),
            Token::String(b"cmd".into()),
            Token::Pipe,
            Token::Getline,
            Token::String(b"cmd".into()),
            Token::DoublePipe,
            Token::Getline,
        ]
    );
}

#[test]
fn lexer_test_print_redirection() {
    let arena = Bump::new();
    let str = br#"print > "out" print >> "out" print | "cmd" print |& "cmd""#;
    assert_eq!(
        &lex(str, &arena, false, false),
        &[
            Token::Print,
            Token::GreaterThan,
            Token::String(b"out".into()),
            Token::Print,
            Token::AppendPipe,
            Token::String(b"out".into()),
            Token::Print,
            Token::Pipe,
            Token::String(b"cmd".into()),
            Token::Print,
            Token::DoublePipe,
            Token::String(b"cmd".into()),
        ]
    );
}

#[test]
fn lexer_test_func_keyword() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"func function", &arena, false, false),
        &[Token::Function, Token::Function]
    );
}

#[test]
fn lexer_test_func_keyword_posix() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"func", &arena, true, false),
        &[Token::Identifier(Identifier {
            namespace: None,
            literal: "func"
        })]
    );
}

#[test]
fn lexer_test_backslash_escape_in_string() {
    let arena = Bump::new();
    assert_eq!(
        &lex(b"\"\\n\\t\\\\\"", &arena, false, false),
        &[Token::String(b"\n\t\\".into())]
    );
}
