// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::io::Write;

use crate::{BlobId, LocaleEncoding, StaticInterner, Token};

fn lex(src: &[u8], posix_strict: bool, gnu_strict: bool) -> Vec<Token> {
    lex_with_encoding(src, posix_strict, gnu_strict, LocaleEncoding::utf8())
}

fn lex_with_encoding(
    src: &[u8],
    posix_strict: bool,
    gnu_strict: bool,
    encoding: LocaleEncoding,
) -> Vec<Token> {
    Token::lex_with_encoding(src, posix_strict, gnu_strict, encoding)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

// The interner is thread-local and tests run one per thread, so interning the
// expected value here yields the same ID the lexer produced for equal bytes.

fn blob(bytes: impl AsRef<[u8]>) -> BlobId {
    StaticInterner::intern(bytes.as_ref())
}

fn string(bytes: impl AsRef<[u8]>) -> Token {
    Token::String(blob(bytes))
}

fn regex(bytes: impl AsRef<[u8]>) -> Token {
    Token::Regex(blob(bytes))
}

fn typed_regex(bytes: impl AsRef<[u8]>) -> Token {
    Token::TypedRegex(blob(bytes))
}

fn ident(name: &str) -> Token {
    Token::Identifier(StaticInterner::intern_ident(name))
}

fn indirect_call(name: &str) -> Token {
    Token::IndirectCall(StaticInterner::intern_ident(name))
}

#[test]
fn lexer_test_newlines_non_posix() {
    let mixed = " \t \n \t\n\n \\\n \t";
    let mut str: Vec<u8> = Vec::new();
    for tok in ["BEGIN", "{", "else", "do", "&&", "||", "?", ":", ","] {
        write!(str, "{tok}{mixed}").unwrap();
    }
    str.push(b'}');
    assert_eq!(
        &lex(&str, false, false),
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
    let mut str: Vec<u8> = Vec::new();
    for tok in ["BEGIN", "{", "else", "do", "&&", "||", "?", ":", ","] {
        write!(str, "{tok}{mixed}").unwrap();
    }
    str.push(b'}');
    lex(&str, true, false);
}

#[test]
fn lexer_test_collapsible_delimiters() {
    let str = b";\\\n;\n\n\n\n;;\n\\\n\n";
    assert_eq!(
        &lex(str, false, false),
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
fn lexer_test_carriage_returns_are_ignored() {
    let lf = |src: &'static [u8]| lex(src, false, false);
    // CRLF line endings
    assert_eq!(
        lf(b"BEGIN {\r\n  x = 1\r\n}\r\n"),
        lf(b"BEGIN {\n  x = 1\n}\n")
    );
    // a bare carriage return is white space, anywhere between tokens
    assert_eq!(lf(b"x\r=\r1\r"), lf(b"x = 1"));
    assert_eq!(lf(b"\"foo\"\r\" bar\""), lf(b"\"foo\" \" bar\""));
    // an escaped newline may be preceded by a carriage return
    assert_eq!(lf(b"1 +\\\r\n2"), lf(b"1 +\\\n2"));
    assert_eq!(lf(b"1 +\\\r\n2"), lf(b"1 + 2"));
}

#[test]
fn lexer_test_multiline() {
    let str = b"\"apple\\\n orange\", /lemon\\\n fruit/";
    assert_eq!(
        &lex(str, false, false),
        &[string(b"apple orange"), Token::Comma, regex(b"lemon fruit")]
    );
}

#[test]
fn lexer_test_uu_extensions() {
    assert_eq!(
        lex(b"@concurrent", false, true),
        &[indirect_call("concurrent")]
    );
}

#[test]
fn lexer_test_gnu_pattern() {
    assert_eq!(
        &lex(b"BEGINFILE ENDFILE", true, false),
        &[ident("BEGINFILE"), ident("ENDFILE")]
    );
}

#[test]
fn lexer_test_nums() {
    let str = b"1 20. 0. .3 2e4 -3.e2 5e+1 2.1e-3 -2147483649 -128 -0 127 2147483648";
    assert_eq!(
        &lex(str, false, false),
        &[
            Token::Integer(1),
            Token::Number(20.),
            Token::Number(0.),
            Token::Number(0.3),
            Token::Number(2e4),
            Token::Minus,
            Token::Number(3e2),
            Token::Number(5e1),
            Token::Number(2.1e-3),
            Token::Minus,
            Token::Number(2_147_483_649.),
            Token::Minus,
            Token::Integer(128),
            Token::Minus,
            Token::Integer(0),
            Token::Integer(127),
            Token::Number(2_147_483_648.)
        ]
    );
}

#[test]
fn lexer_test_directive_escaping() {
    let str = br#" @include "aa\"a\ta" @nsinclude "b\"\nb" "#;
    assert_eq!(
        &lex(str, false, false),
        &[
            Token::IncludeDirective,
            string(b"aa\"a\ta"),
            Token::NsIncludeDirective,
            string(b"b\"\nb")
        ]
    );
}

#[test]
fn lexer_test_ident_rules_non_posix() {
    assert_eq!(
        &lex(b"1a::a a::1a _a", false, false),
        &[
            Token::Integer(1),
            ident("a"),
            Token::PathSpec,
            ident("a"),
            ident("a"),
            Token::PathSpec,
            Token::Integer(1),
            ident("a"),
            ident("_a")
        ]
    );
}

#[test]
#[should_panic]
fn lexer_test_ident_rules_posix() {
    lex(b"@namespace \"foo\"; foo::a", true, false);
}

#[test]
fn lexer_test_general_tokens() {
    let str = br#"
        @load "lib1.so.1"
        BEGIN { print a + 1 }
        /2\..*/;
        END { $1 == foo::bar }
    "#;
    assert_eq!(
        &lex(str, false, false),
        &[
            Token::Newline,
            Token::LoadDirective,
            string(b"lib1.so.1"),
            Token::Newline,
            Token::BeginPattern,
            Token::OpenBrace,
            Token::Print,
            ident("a"),
            Token::Plus,
            Token::Integer(1),
            Token::ClosedBrace,
            Token::Newline,
            regex(b"2\\..*"),
            Token::Semicolon,
            Token::Newline,
            Token::EndPattern,
            Token::OpenBrace,
            Token::Record,
            Token::Integer(1),
            Token::EqualTo,
            ident("foo"),
            Token::PathSpec,
            ident("bar"),
            Token::ClosedBrace,
            Token::Newline
        ]
    );
}

#[test]
fn lexer_test_regex_ambiguity() {
    assert_eq!(
        &lex(b"1/=1. a/=1 NR/=1", false, false),
        &[
            Token::Integer(1),
            Token::SlashAssign,
            Token::Number(1.),
            ident("a"),
            Token::SlashAssign,
            Token::Integer(1),
            Token::NrVariable,
            Token::SlashAssign,
            Token::Integer(1)
        ]
    );
}

#[test]
fn lexer_test_hex_escape() {
    assert_eq!(&lex(b"\"\\x41\"", false, false), &[string(b"A")]);
}

#[test]
fn lexer_test_hex_escape_uppercase() {
    assert_eq!(&lex(b"\"\\x4F\"", false, false), &[string(b"O")]);
}

#[test]
fn lexer_test_hex_escape_single_digit() {
    assert_eq!(&lex(b"\"\\x9\"", false, false), &[string(b"\x09")]);
}

#[test]
fn lexer_test_hex_escape_posix_strict() {
    assert_eq!(&lex(b"\"\\x41\"", true, false), &[string(b"x41")]);
}

#[test]
fn lexer_test_unicode_escape_posix_strict() {
    assert_eq!(&lex(b"\"\\u0041\"", true, false), &[string(b"u0041")]);
}

#[test]
fn lexer_test_unicode_escape_ascii() {
    assert_eq!(&lex(b"\"\\u0041\"", false, false), &[string(b"A")]);
}

#[test]
fn lexer_test_unicode_escape_two_byte() {
    assert_eq!(
        &lex(b"\"\\u00e9\"", false, false),
        &[string("\u{00e9}".as_bytes())]
    );
}

#[test]
fn lexer_test_unicode_escape_three_byte() {
    assert_eq!(
        &lex(b"\"\\u4e2d\"", false, false),
        &[string("\u{4e2d}".as_bytes())]
    );
}

#[test]
fn lexer_test_unicode_escape_uppercase() {
    assert_eq!(&lex(b"\"\\u004F\"", false, false), &[string(b"O")]);
}

#[test]
fn lexer_test_unicode_escape_single_digit() {
    assert_eq!(
        &lex(b"\"\\u9\"", false, false),
        &[string("\u{9}".as_bytes())]
    );
}

#[test]
fn lexer_test_hex_escape_no_digits() {
    assert_eq!(&lex(b"\"\\x\"", false, false), &[string(b"x")]);
}

#[test]
fn lexer_test_unicode_escape_no_digits() {
    assert_eq!(&lex(b"\"\\u\"", false, false), &[string(b"u")]);
}

#[test]
fn lexer_test_unicode_escape_eight_digits() {
    assert_eq!(&lex(b"\"\\u00000032\"", false, false), &[string(b"2")]);
}

#[test]
fn lexer_test_unicode_escape_iso8859_1() {
    assert_eq!(
        &lex_with_encoding(b"\"\\u00e9\"", false, false, LocaleEncoding::iso_8859_1()),
        &[string(b"\xe9")]
    );
    assert_eq!(
        &lex_with_encoding(b"\"\\u0041\"", false, false, LocaleEncoding::iso_8859_1()),
        &[string(b"A")]
    );
    assert_eq!(
        &lex_with_encoding(b"\"\\u4e2d\"", false, false, LocaleEncoding::iso_8859_1()),
        &[string(b"?")]
    );
}

#[test]
fn lexer_test_unicode_escape_ascii_locale() {
    assert_eq!(
        &lex_with_encoding(b"\"\\u0041\"", false, false, LocaleEncoding::ascii()),
        &[string(b"A")]
    );
    assert_eq!(
        &lex_with_encoding(b"\"\\u00e9\"", false, false, LocaleEncoding::ascii()),
        &[string(b"?")]
    );
}

#[test]
fn lexer_test_unicode_escape_invalid_codepoint() {
    assert_eq!(
        &lex_with_encoding(b"\"\\u110000\"", false, false, LocaleEncoding::utf8()),
        &[string(b"?")]
    );
    assert_eq!(
        &lex_with_encoding(b"\"\\uD800\"", false, false, LocaleEncoding::utf8()),
        &[string(b"?")]
    );
}

#[test]
fn locale_encoding_from_locale_name() {
    assert_eq!(
        LocaleEncoding::from_locale_name("C"),
        LocaleEncoding::ascii()
    );
    assert_eq!(
        LocaleEncoding::from_locale_name("POSIX"),
        LocaleEncoding::ascii()
    );
    assert_eq!(
        LocaleEncoding::from_locale_name("en_US.UTF-8"),
        LocaleEncoding::utf8()
    );
    assert_eq!(
        LocaleEncoding::from_locale_name("fr_FR.ISO-8859-1"),
        LocaleEncoding::iso_8859_1()
    );
}

#[test]
fn lexer_test_operators() {
    let str = b"~ !~ | |& >> ** **= ^= += -= *= %= == != <= >= < > && || !";
    assert_eq!(
        &lex(str, false, false),
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
    assert_eq!(
        &lex(b"a/=1", false, false),
        &[ident("a"), Token::SlashAssign, Token::Integer(1),]
    );
}

#[test]
fn lexer_test_slash_after_number_divides() {
    // A number is an operand, so a following `/` is division, not a regex.
    assert_eq!(
        &lex(b"1 / 2 / 3", false, false),
        &[
            Token::Integer(1),
            Token::Slash,
            Token::Integer(2),
            Token::Slash,
            Token::Integer(3),
        ]
    );
    assert_eq!(
        &lex(b"1./2 .5/2 2e3/4", false, false),
        &[
            Token::Number(1.),
            Token::Slash,
            Token::Integer(2),
            Token::Number(0.5),
            Token::Slash,
            Token::Integer(2),
            Token::Number(2e3),
            Token::Slash,
            Token::Integer(4),
        ]
    );
    // The field index in `$1 / 0` is a number too.
    assert_eq!(
        &lex(b"$1 / 0", false, false),
        &[
            Token::Record,
            Token::Integer(1),
            Token::Slash,
            Token::Integer(0),
        ]
    );
}

#[test]
fn lexer_test_slash_after_special_variable_divides() {
    assert_eq!(
        &lex(b"NR / 2, NF/NR", false, false),
        &[
            Token::NrVariable,
            Token::Slash,
            Token::Integer(2),
            Token::Comma,
            Token::NfVariable,
            Token::Slash,
            Token::NrVariable,
        ]
    );
}

#[test]
fn lexer_test_regex_after_operator_is_terminated() {
    // After an operator `/` opens a regex that ends at the next unescaped `/`,
    // and the lexer is back in operator context afterwards.
    assert_eq!(
        &lex(b"x ~ /a/ / 2", false, false),
        &[
            ident("x"),
            Token::Matching,
            regex("a"),
            Token::Slash,
            Token::Integer(2),
        ]
    );
}

#[test]
fn lexer_test_control_flow_keywords() {
    let str = b"switch case default getline printf next nextfile exit return continue break";
    assert_eq!(
        &lex(str, false, false),
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
    let str = b"NR FNR NF FS FPAT FIELDWIDTHS RS RT OFS ORS FILENAME ARGC ARGV SUBSEP ARGIND OFMT \
                RSTART RLENGTH ENVIRON PROCINFO SYMTAB FUNCTAB";
    assert_eq!(
        &lex(str, false, false),
        &[
            Token::NrVariable,
            Token::FnrVariable,
            Token::NfVariable,
            Token::FsVariable,
            Token::FpatVariable,
            Token::FieldwidthsVariable,
            Token::RsVariable,
            Token::RtVariable,
            Token::OfsVariable,
            Token::OrsVariable,
            Token::FilenameVariable,
            Token::ArgcVariable,
            Token::ArgvVariable,
            Token::SubsepVariable,
            Token::ArgindVariable,
            Token::OfmtVariable,
            Token::RstartVariable,
            Token::RlengthVariable,
            Token::EnvironVariable,
            Token::ProcinfoVariable,
            Token::SymtabVariable,
            Token::FunctabVariable
        ]
    );
}

#[test]
fn lexer_test_typed_regex() {
    assert_eq!(&lex(b"@/pat/", false, false), &[typed_regex(b"pat")]);
}

#[test]
fn lexer_test_comments() {
    let str = b"print 1 # comment\nprint 2";
    assert_eq!(
        &lex(str, false, false),
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
    assert_eq!(
        &lex(b"@foo @ns::bar", false, false),
        &[
            indirect_call("foo"),
            indirect_call("ns"),
            Token::PathSpec,
            ident("bar")
        ]
    );
}

#[test]
#[should_panic]
fn lexer_test_indirect_call_posix() {
    lex(b"@foo", true, false);
}

#[test]
fn lexer_test_concurrent_directive() {
    assert_eq!(
        &lex(b"@concurrent", false, false),
        &[Token::ConcurrentDirective]
    );
}

#[test]
fn lexer_test_load_and_namespace_directives() {
    assert_eq!(
        &lex(br#"@load "lib.so" @namespace "ns""#, false, false),
        &[
            Token::LoadDirective,
            string(b"lib.so"),
            Token::NamespaceDirective,
            string(b"ns"),
        ]
    );
}

#[test]
fn lexer_test_regex_literals() {
    assert_eq!(&lex(b"/abc/", false, false), &[regex(b"abc")]);
    assert_eq!(&lex(b"/a\\/b/", false, false), &[regex(b"a/b")]);
    assert_eq!(
        &lex(b"x~/dot+/", false, false),
        &[ident("x"), Token::Matching, regex(b"dot+"),]
    );
}

#[test]
fn lexer_test_switch_snippet() {
    let str = br"switch (x) { case 1: print; default: break }";
    assert_eq!(
        &lex(str, false, false),
        &[
            Token::Switch,
            Token::OpenParent,
            ident("x"),
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
    let str = br#"getline getline x < "f" "cmd" | getline "cmd" |& getline"#;
    assert_eq!(
        &lex(str, false, false),
        &[
            Token::Getline,
            Token::Getline,
            ident("x"),
            Token::LesserThan,
            string(b"f"),
            string(b"cmd"),
            Token::Pipe,
            Token::Getline,
            string(b"cmd"),
            Token::DoublePipe,
            Token::Getline,
        ]
    );
}

#[test]
fn lexer_test_print_redirection() {
    let str = br#"print > "out" print >> "out" print | "cmd" print |& "cmd""#;
    assert_eq!(
        &lex(str, false, false),
        &[
            Token::Print,
            Token::GreaterThan,
            string(b"out"),
            Token::Print,
            Token::AppendPipe,
            string(b"out"),
            Token::Print,
            Token::Pipe,
            string(b"cmd"),
            Token::Print,
            Token::DoublePipe,
            string(b"cmd"),
        ]
    );
}

#[test]
fn lexer_test_func_keyword() {
    assert_eq!(
        &lex(b"func function", false, false),
        &[Token::Function, Token::Function]
    );
}

#[test]
fn lexer_test_func_keyword_posix() {
    assert_eq!(&lex(b"func", true, false), &[ident("func")]);
}

#[test]
fn lexer_test_backslash_escape_in_string() {
    assert_eq!(&lex(b"\"\\n\\t\\\\\"", false, false), &[string(b"\n\t\\")]);
}
