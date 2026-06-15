// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#[cfg(test)]
mod tests;

use core::str;
use std::{
    cmp::Ordering,
    fmt::{Debug, Display},
    ptr::NonNull,
    slice::SliceIndex,
};

use bumpalo::{Bump, collections::Vec};
use logos::{Logos, Skip};
pub use logos::{Span, SpannedIter};
use memchr::{memchr, memchr3};
use thiserror::Error;

pub type Lexer<'a> = logos::Lexer<'a, Token<'a>>;
pub type Result<T, E = LexingError> = std::result::Result<T, E>;

#[derive(Logos, Debug, PartialEq)]
#[logos(utf8 = false)]
#[logos(skip(r"(?&ignore)"))]
#[logos(skip("#", skip_line))]
#[logos(extras = Extra)]
#[logos(subpattern identifier = r"[a-zA-Z_][a-zA-Z0-9_]*")]
#[logos(subpattern ignore = r"(?:[ \t]|(\\\n))+")]
#[logos(subpattern ignore_with_nl = r"(?:(?&ignore)|\n)*")]
#[logos(error(LexingError, callback = |lex| LexingError::unexpected(lex)))]
pub enum Token<'a> {
    #[regex(r"(-)?\d+", parse_num, priority = 9)]
    Numeric,
    // Not emitted by Logos directly.
    Integer(i32),
    #[regex(r"(-)?[0-9]+(\.[0-9]*)?([eE][+-]?[0-9]+)?", parse_float, priority = 8)]
    #[regex(r"\.[0-9]+([eE][+-]?[0-9]+)?", parse_float)]
    Number(f64),
    #[token("\"", parse_string)]
    String(Slice<'a>),
    #[token("BEGIN", accept_expression)]
    BeginPattern,
    #[token("END", accept_expression)]
    EndPattern,
    #[token("BEGINFILE", |lex| parse_non_posix_keyword(lex, Token::BeginFilePattern))]
    BeginFilePattern,
    #[token("ENDFILE", |lex| parse_non_posix_keyword(lex, Token::EndFilePattern))]
    EndFilePattern,
    #[token("@load")]
    LoadDirective,
    #[token("@include")]
    IncludeDirective,
    #[token("@nsinclude", parse_non_posix_operator)]
    NsIncludeDirective,
    #[regex("@namespace", parse_non_posix_operator)]
    NamespaceDirective,
    #[token("@concurrent", parse_non_gnu_directive)]
    ConcurrentDirective,
    #[token("if", accept_expression)]
    If,
    #[regex(r"else(?&ignore_with_nl)", accept_expression)]
    Else,
    #[token("switch", accept_expression)]
    Switch,
    #[token("case", accept_expression)]
    Case,
    #[token("default", accept_expression)]
    Default,
    #[regex(r"do(?&ignore_with_nl)", accept_expression)]
    Do,
    #[token("while", accept_expression)]
    While,
    #[token("for", accept_expression)]
    For,
    #[token("in", accept_expression)]
    In,
    #[token("print", accept_expression)]
    Print,
    #[token("printf", accept_expression)]
    Printf,
    #[token("getline", accept_expression)]
    Getline,
    #[token("next", accept_expression)]
    Next,
    #[token("nextfile", accept_expression)]
    NextFile,
    #[token("exit", accept_expression)]
    Exit,
    #[token("break", accept_expression)]
    Break,
    #[token("continue", accept_expression)]
    Continue,
    #[token("return", accept_expression)]
    Return,
    #[token("delete", accept_expression)]
    Delete,
    #[token("function", accept_expression)]
    #[token("func", |lex| parse_non_posix_keyword(lex, Token::Function))]
    Function,
    #[token("length", accept_expression)]
    Length,
    #[token("substr", accept_expression)]
    Substr,
    #[token("split", accept_expression)]
    Split,
    #[token("sub", accept_expression)]
    Sub,
    #[token("gsub", accept_expression)]
    Gsub,
    #[token("match", accept_expression)]
    MatchFn,
    #[token("index", accept_expression)]
    Index,
    #[token("sprintf", accept_expression)]
    Sprintf,
    #[token("toupper", accept_expression)]
    Toupper,
    #[token("tolower", accept_expression)]
    Tolower,
    #[token("gensub", |lex| parse_non_posix_keyword(lex, Token::Gensub))]
    Gensub,
    #[token("patsplit", |lex| parse_non_posix_keyword(lex, Token::Patsplit))]
    Patsplit,
    #[token("strtonum", |lex| parse_non_posix_keyword(lex, Token::Strtonum))]
    Strtonum,
    #[token("close", accept_expression)]
    Close,
    #[token("fflush", accept_expression)]
    Fflush,
    #[token("system", accept_expression)]
    System,
    #[token("int", accept_expression)]
    Int,
    #[token("sqrt", accept_expression)]
    Sqrt,
    #[token("exp", accept_expression)]
    Exp,
    #[token("log", accept_expression)]
    Log,
    #[token("sin", accept_expression)]
    Sin,
    #[token("cos", accept_expression)]
    Cos,
    #[token("atan2", accept_expression)]
    Atan2,
    #[token("rand", accept_expression)]
    Rand,
    #[token("srand", accept_expression)]
    Srand,
    #[token("systime", |lex| parse_non_posix_keyword(lex, Token::Systime))]
    Systime,
    #[token("mktime", |lex| parse_non_posix_keyword(lex, Token::Mktime))]
    Mktime,
    #[token("strftime", |lex| parse_non_posix_keyword(lex, Token::Strftime))]
    Strftime,
    #[token("typeof", |lex| parse_non_posix_keyword(lex, Token::Typeof))]
    Typeof,
    #[token("isarray", |lex| parse_non_posix_keyword(lex, Token::Isarray))]
    Isarray,
    #[token("asort", |lex| parse_non_posix_keyword(lex, Token::Asort))]
    Asort,
    #[token("asorti", |lex| parse_non_posix_keyword(lex, Token::Asorti))]
    Asorti,
    #[token("and", |lex| parse_non_posix_keyword(lex, Token::And))]
    And,
    #[token("or", |lex| parse_non_posix_keyword(lex, Token::Or))]
    Or,
    #[token("xor", |lex| parse_non_posix_keyword(lex, Token::Xor))]
    Xor,
    #[token("compl", |lex| parse_non_posix_keyword(lex, Token::Compl))]
    Compl,
    #[token("lshift", |lex| parse_non_posix_keyword(lex, Token::Lshift))]
    Lshift,
    #[token("rshift", |lex| parse_non_posix_keyword(lex, Token::Rshift))]
    Rshift,
    #[token("NR", accept_expression)]
    NrVariable,
    #[token("NF", accept_expression)]
    NfVariable,
    #[token("FS", accept_expression)]
    FsVariable,
    #[token("RS", accept_expression)]
    RsVariable,
    #[token("OFS", accept_expression)]
    OfsVariable,
    #[token("ORS", accept_expression)]
    OrsVariable,
    #[token("FILENAME", accept_expression)]
    FilenameVariable,
    #[token("ARGC", accept_expression)]
    ArgcVariable,
    #[token("ARGV", accept_expression)]
    ArgvVariable,
    #[token("SUBSEP", accept_expression)]
    SubsepVariable,
    #[token("FNR", accept_expression)]
    FnrVariable,
    #[token("ARGIND")]
    ArgindVariable,
    #[token("OFMT", accept_expression)]
    OfmtVariable,
    #[token("RSTART", accept_expression)]
    RstartVariable,
    #[token("RLENGTH", accept_expression)]
    RlengthVariable,
    #[token("ENVIRON", accept_expression)]
    EnvironVariable,
    #[regex("(?&identifier)", Identifier::without_namespace::<0>)]
    #[regex(r"(?&identifier)::(?&identifier)", Identifier::with_namespace::<0>)]
    Identifier(Identifier<'a>),
    #[regex("@(?&identifier)", parse_indirect_call::<false>)]
    #[regex(r"@(?&identifier)::(?&identifier)", parse_indirect_call::<true>)]
    IndirectCall(Identifier<'a>),
    #[token("+", accept_expression)]
    Plus,
    #[token("-", accept_expression)]
    Minus,
    #[token("*", accept_expression)]
    Star,
    #[token("/", parse_regex_or_op)]
    Slash,
    /// Not generated by Logos directly.
    Regex(Slice<'a>),
    #[token("@/", |lex| { accept_operator(lex); parse_content::<true, '/'>(lex) })]
    TypedRegex(Slice<'a>),
    #[token("%", accept_expression)]
    Percent,
    #[token("^", accept_expression)]
    #[token("**", parse_non_posix_operator)]
    Circumflex,
    #[token("++", accept_expression)]
    Increment,
    #[token("--", accept_expression)]
    Decrement,
    #[token("=", accept_expression)]
    Assignment,
    #[token("+=", accept_expression)]
    PlusAssign,
    #[token("-=", accept_expression)]
    MinusAssign,
    #[token("*=", accept_expression)]
    StarAssign,
    /// Not generated by Logos directly.
    SlashAssign,
    #[token("%=", accept_expression)]
    PercentAssign,
    #[token("^=", accept_expression)]
    #[token("**=", parse_non_posix_operator)]
    CaretAssign,
    #[token("==", accept_expression)]
    EqualTo,
    #[token("!=", accept_expression)]
    NotEqualTo,
    #[token("<", accept_expression)]
    LesserThan,
    #[token("<=", accept_expression)]
    LesserOrEqualThan,
    #[token(">", accept_expression)]
    GreaterThan,
    #[token(">=", accept_expression)]
    GreaterOrEqualThan,
    #[regex(r"&&(?&ignore_with_nl)", accept_expression)]
    BooleanAnd,
    #[regex(r"\|\|(?&ignore_with_nl)", accept_expression)]
    BooleanOr,
    #[token("!", accept_expression)]
    Negation,
    #[token("~", accept_expression)]
    Matching,
    #[token("!~", accept_expression)]
    NotMatching,
    #[token("|", accept_expression)]
    Pipe,
    #[token("|&", parse_non_posix_operator)]
    DoublePipe,
    #[token(">>", accept_expression)]
    AppendPipe,
    #[regex(r"\?(?&ignore)?", accept_expression, priority = 11)]
    #[regex(r"\?(?&ignore_with_nl)", parse_non_posix_operator, priority = 10)]
    QuestionMark,
    #[regex(":(?&ignore)?", accept_expression, priority = 11)]
    #[regex(r":(?&ignore_with_nl)", parse_non_posix_operator, priority = 10)]
    Colon,
    #[regex(r"\{(?&ignore_with_nl)", accept_expression)]
    OpenBrace,
    #[token("}", accept_expression)]
    ClosedBrace,
    #[token("(", accept_expression)]
    OpenParent,
    #[token(")", accept_operator)]
    ClosedParent,
    #[token("[", accept_expression)]
    OpenBracket,
    #[token("]", accept_operator)]
    ClosedBracket,
    #[regex(r",(?&ignore_with_nl)", accept_expression)]
    Comma,
    #[token("$", accept_expression)]
    Record,
    #[regex(r"\n+", accept_expression)]
    Newline,
    #[regex(";", accept_expression)]
    Semicolon,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Extra {
    ctx: Context,
    arena: NonNull<Bump>,
    posix_strict: bool,
    gnu_strict: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub enum Context {
    #[default]
    AcceptExpression,
    AcceptOperator,
}

#[derive(Debug, Default, Error, Clone, PartialEq, Eq)]
pub enum LexingError {
    #[default]
    #[error("Unknown error")]
    Unknown,
    #[error("Unexpected token at {:?}: {:?}", .0, .1)]
    Unexpected(Span, String),
    #[error("Unterminated string at {:?}", .0)]
    UnterminatedString(Span),
    #[error("Unterminated regex at {:?}", .0)]
    UnterminatedRegex(Span),
    #[error("Unexpected End of File!")]
    UnexpectedEof,
    #[error("Unknown token: {:?}.", .1)]
    UnavailableOnPosix(Span, String),
    #[error("Unknown token: {:?}.", .1)]
    UnavailableOnGnu(Span, String),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Identifier<'a> {
    pub namespace: Option<&'a str>,
    pub literal: &'a str,
}

impl<'a> Token<'a> {
    pub fn lex(
        source: &'a [u8],
        arena: &'a Bump,
        posix_strict: bool,
        gnu_strict: bool,
    ) -> logos::Lexer<'a, Self> {
        Lexer::with_extras(
            source,
            Extra {
                ctx: Context::AcceptExpression,
                arena: NonNull::from_ref(arena),
                posix_strict,
                gnu_strict,
            },
        )
    }
}

impl LexingError {
    fn to_utf8(lex: &mut Lexer<'_>) -> String {
        String::from_utf8_lossy(lex.slice()).to_string()
    }

    #[cold]
    fn unexpected(lex: &mut Lexer<'_>) -> Self {
        Self::Unexpected(lex.span(), Self::to_utf8(lex))
    }

    #[cold]
    fn non_posix(lex: &mut Lexer<'_>) -> Self {
        Self::UnavailableOnPosix(lex.span(), Self::to_utf8(lex))
    }

    #[cold]
    #[allow(dead_code)] // Remove if we add extensions that require it.
    fn non_uu(lex: &mut Lexer<'_>) -> Self {
        Self::UnavailableOnGnu(lex.span(), Self::to_utf8(lex))
    }
}

fn skip_line(lex: &mut Lexer<'_>) -> Skip {
    lex.bump(memchr(b'\n', lex.remainder()).unwrap_or(lex.remainder().len()));
    logos::skip(lex)
}

fn parse_string<'a>(lex: &mut logos::Lexer<'a, Token<'a>>) -> Result<Slice<'a>> {
    accept_operator(lex);
    parse_content::<false, '"'>(lex)
}

fn parse_regex_or_op<'a>(lex: &mut logos::Lexer<'a, Token<'a>>) -> Result<Token<'a>> {
    match lex.extras.ctx {
        Context::AcceptExpression => {
            accept_operator(lex);
            parse_content::<true, '/'>(lex).map(Token::Regex)
        }
        Context::AcceptOperator => {
            accept_expression(lex);
            if lex.remainder().first() == Some(&b'=') {
                lex.bump(1);
                Ok(Token::SlashAssign)
            } else {
                Ok(Token::Slash)
            }
        }
    }
}

fn parse_content<'a, const REGEX: bool, const DELIMITER: char>(
    lex: &mut Lexer<'a>,
) -> Result<Slice<'a>> {
    let rest = lex.remainder();
    let mut start = 0;
    let mut out = Slice::Borrowed(&[]);

    while let Some(rel_i) = memchr3(b'\n', b'\\', DELIMITER as u8, &rest[start..]) {
        let i = start + rel_i;

        match rest[i] {
            d if d == DELIMITER as u8 => {
                // push remaining segment
                lex.bump(i + 1);
                if start == 0 {
                    out = Slice::Borrowed(&rest[..i]);
                } else {
                    out.to_mut(lex.extras.arena())
                        .extend_from_slice(&rest[start..i]);
                }
                return Ok(out);
            }
            b'\\' => {
                out.to_mut(lex.extras.arena())
                    .extend_from_slice(&rest[start..i]);
                let consumed = parse_escape::<REGEX>(
                    &rest[i..],
                    out.to_mut(lex.extras.arena()),
                    lex.extras.posix_strict,
                )?;
                start = i + consumed;
            }
            _ => break,
        }
    }
    if REGEX {
        Err(LexingError::UnterminatedRegex(lex.span()))
    } else {
        Err(LexingError::UnterminatedString(lex.span()))
    }
}

fn parse_escape<const REGEX: bool>(
    slice: &[u8],
    out: &mut Vec<u8>,
    posix_strict: bool,
) -> Result<usize> {
    let mut count = 2;
    let is_oct = |x: char| ('0'..'8').contains(&x);
    let is_hex = |i: usize| {
        slice
            .get(i)
            .is_some_and(|&x| (x as char).is_ascii_hexdigit())
    };
    let is_slice_oct = |i| slice.get(i).map(|&x| x as char).is_some_and(is_oct);
    let Some(to_escape) = slice.get(1).map(|x| *x as char) else {
        return Err(LexingError::UnexpectedEof);
    };

    if to_escape == '\n' {
        return Ok(count);
    }

    // On minimal, only drop backslash for '"' and '\n'
    let escaped = match to_escape {
        c @ ('\\' | '"') if !REGEX => c,
        c @ ('[' | ']' | '{' | '}' | '(' | ')' | '*' | '+' | '^' | '$' | '.' | '?') if REGEX => {
            out.push(b'\\');
            c
        }
        'a' => 7 as char,
        'b' => 8 as char,
        'f' => 12 as char,
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => 11 as char,
        n if is_oct(n) => {
            count += is_slice_oct(2) as usize + (is_slice_oct(2) && is_slice_oct(3)) as usize;
            slice[1..]
                .iter()
                .take(count - 1)
                .fold(0, |acc, digit| acc * 8 + digit - b'0') as char
        }
        'x' if !posix_strict => {
            let num_digits = is_hex(2) as usize + (is_hex(2) && is_hex(3)) as usize;

            if num_digits == 0 {
                'x'
            } else {
                count += num_digits;

                let value = parse_hex_digits(&slice[2..2 + num_digits]) as u8;
                value as char
            }
        }
        'u' if !posix_strict => {
            let num_digits = (2..=9).take_while(|&i| is_hex(i)).count();

            if num_digits == 0 {
                'u'
            } else {
                count += num_digits;

                let codepoint = parse_hex_digits(&slice[2..2 + num_digits]);

                // FIXME: assumes UTF-8 locale; replacement character and encoding may differ
                // for non-UTF-8 locales.
                let c = char::from_u32(codepoint).unwrap_or('\u{FFFD}');
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf);
                out.extend_from_slice_copy(encoded.as_bytes());

                return Ok(count);
            }
        }
        // Unspecified by POSIX; we ditto GNU.
        c => c, // TODO: Output warning
    };
    out.push(escaped as u8);
    Ok(count)
}

fn parse_hex_digits(slice: &[u8]) -> u32 {
    slice.iter().fold(0u32, |acc, &digit| {
        acc * 16
            + match digit {
                b'0'..=b'9' => (digit - b'0') as u32,
                b'a'..=b'f' => (digit - b'a' + 10) as u32,
                b'A'..=b'F' => (digit - b'A' + 10) as u32,
                _ => unreachable!(),
            }
    })
}

fn parse_ident<'a>(lex: &mut Lexer<'a>, index: impl SliceIndex<[u8], Output = [u8]>) -> &'a str {
    accept_operator(lex);
    // SAFETY: The regex matching ensures it is ASCII.
    unsafe { str::from_utf8_unchecked(lex.slice().get_unchecked(index)) }
}

fn parse_float(lex: &mut Lexer<'_>) -> f64 {
    parse_ident(lex, ..).parse().unwrap_or(0.)
}

fn parse_num<'a>(lex: &mut Lexer<'a>) -> Token<'a> {
    if let Ok(num) = parse_ident(lex, ..).parse() {
        Token::Integer(num)
    } else {
        Token::Number(parse_float(lex))
    }
}

fn parse_non_posix_keyword<'a>(lex: &mut Lexer<'a>, other: Token<'a>) -> Token<'a> {
    if lex.extras.posix_strict {
        Token::Identifier(Identifier::without_namespace::<0>(lex))
    } else {
        accept_expression(lex);
        other
    }
}

fn parse_non_posix_operator(lex: &mut Lexer<'_>) -> Result<()> {
    if lex.extras.posix_strict {
        Err(LexingError::non_posix(lex))
    } else {
        accept_expression(lex);
        Ok(())
    }
}

fn parse_non_gnu_directive<'a>(lex: &mut Lexer<'a>) -> Result<Token<'a>> {
    accept_expression(lex);
    if lex.extras.posix_strict {
        Err(LexingError::non_posix(lex))
    } else if lex.extras.gnu_strict {
        Ok(Token::IndirectCall(Identifier::without_namespace::<1>(lex)))
    } else {
        Ok(Token::ConcurrentDirective)
    }
}

fn parse_indirect_call<'a, const QUALIFIED: bool>(lex: &mut Lexer<'a>) -> Result<Identifier<'a>> {
    if lex.extras.posix_strict {
        Err(LexingError::non_posix(lex))
    } else if QUALIFIED {
        Identifier::with_namespace::<1>(lex)
    } else {
        Ok(Identifier::without_namespace::<1>(lex))
    }
}

impl<'a> Identifier<'a> {
    fn without_namespace<const SKIP: usize>(lex: &mut Lexer<'a>) -> Self {
        Self { namespace: None, literal: parse_ident(lex, SKIP..) }
    }

    fn with_namespace<const SKIP: usize>(lex: &mut Lexer<'a>) -> Result<Self> {
        if lex.extras.posix_strict {
            Err(LexingError::non_posix(lex))
        } else {
            // SAFETY: The regex matching ensures it is present and well-formed.
            let separator = unsafe { memchr(b':', lex.slice()).unwrap_unchecked() };
            Ok(Self {
                namespace: Some(parse_ident(lex, SKIP..separator)),
                literal: parse_ident(lex, separator + 2..),
            })
        }
    }
}

fn accept_expression(lex: &mut Lexer<'_>) {
    lex.extras.ctx = Context::AcceptExpression;
}

fn accept_operator(lex: &mut Lexer<'_>) {
    lex.extras.ctx = Context::AcceptOperator;
}

#[derive(Clone)]
pub enum Slice<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<'a, u8>),
}

impl Display for Slice<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.as_ref()).as_ref())
    }
}

impl Debug for Slice<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"{self}\"")
    }
}

impl AsRef<[u8]> for Slice<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Borrowed(x) => x,
            Self::Owned(x) => x,
        }
    }
}

impl<'a> Slice<'a> {
    pub fn to_mut(&mut self, arena: &'a Bump) -> &mut Vec<'a, u8> {
        if let Self::Borrowed(x) = self {
            let mut vec = Vec::new_in(arena);
            vec.extend_from_slice_copy(x);
            *self = Self::Owned(vec);
        }
        let Self::Owned(x) = self else { unreachable!() };
        x
    }
}

impl<'a> From<&'a [u8]> for Slice<'a> {
    fn from(value: &'a [u8]) -> Self {
        Self::Borrowed(value)
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for Slice<'a> {
    fn from(value: &'a [u8; N]) -> Self {
        Self::Borrowed(value)
    }
}

impl<'a> From<Vec<'a, u8>> for Slice<'a> {
    fn from(value: Vec<'a, u8>) -> Self {
        Self::Owned(value)
    }
}

impl PartialEq for Slice<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl Eq for Slice<'_> {}

impl PartialOrd for Slice<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Slice<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_ref().cmp(other.as_ref())
    }
}

impl Extra {
    fn arena<'a>(&self) -> &'a Bump {
        // SAFETY: lives for as long as self because it's the same lifetime as
        // the source being lexed; Logos just can't take lifetimes on extras.
        unsafe { self.arena.as_ref() }
    }
}
