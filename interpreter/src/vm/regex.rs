// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! TODO: Locale-aware `\s`, `\w`. Engine w/ POSIX leftmost-longest derivation
//!       that maintains GNU extensions (we use Redox's one for POSIX mode).
//!
//! This is kinda ugly and a huge hack, but way more maintainable than patching
//! an existing crate and vendoring it, at least for now.
//!
//! Known limitation: only leftmost-first derivations with the GNU backend.
//! The `regast` crate seems like an interesting alternative, but we can't
//! support all the GNU operators without vendoring it and patching it, which I
//! do not have the heart to do right now. It's also quite new and it seems not
//! at all optimized for our use case (single-threaded, manually-cached).

use parser::Span;
use posix_regex::compile::Error as PosixBuildError;
use posix_regex::{PosixRegex, PosixRegexBuilder};
use regex_automata::meta::{BuildError, Regex};
use regex_syntax::ast::Error as AstError;
use regex_syntax::ast::parse::Parser as AstParser;
use regex_syntax::hir::translate::Translator;
use regex_syntax::hir::{Error as HirError, Hir, HirKind, Look};

use crate::ExecMode;

// PUA codepoints "guaranteed" absent from the source pattern.
// Yes, I know, sentinels, yuck. It is what it is -- for now.
// This is a temporary solution while we the use `regex-*` crates.
const SENT_WORD_START: char = '\u{F0000}';
const SENT_WORD_END: char = '\u{F0001}';
const SENT_BETWEEN: char = '\u{F0002}';

const SENT_WORD_START_BYTES: [u8; 4] = encode(SENT_WORD_START);
const SENT_WORD_END_BYTES: [u8; 4] = encode(SENT_WORD_END);
const SENT_BETWEEN_BYTES: [u8; 4] = encode(SENT_BETWEEN);

const fn encode(c: char) -> [u8; 4] {
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf);
    buf
}

#[derive(Debug, thiserror::Error)]
pub enum RegexError {
    #[error("Regexp encoding error!")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("Regexp syntax error!")]
    Parse(#[from] AstError),
    #[error("Regexp internal error!")]
    Hir(#[from] HirError),
    #[error("Regexp error!")]
    Build(#[from] BuildError),
    #[error("Regexp syntax error!")]
    Posix(PosixBuildError),
}

// Let me have some puns.
pub enum RegexMatcher {
    Redox(PosixRegex<'static>),
    Sushi(Regex),
}

pub struct Match {
    rstart: isize,
    rlength: isize,
}

impl RegexMatcher {
    pub fn new(pattern: &[u8], mode: ExecMode) -> Result<Self, Box<RegexError>> {
        if matches!(mode, ExecMode::Posix) {
            match PosixRegexBuilder::new(pattern)
                .extended(true)
                .with_default_classes()
                .compile()
            {
                Ok(r) => Ok(Self::Redox(r)),
                Err(e) => Err(Box::new(RegexError::Posix(e))),
            }
        } else {
            let pattern = match str::from_utf8(pattern) {
                Ok(pattern) => pattern,
                Err(e) => {
                    return Err(Box::new(e.into()));
                }
            };
            let hir = lower_sushi_regex(pattern)?;
            match Regex::builder().build_from_hir(&hir) {
                Ok(r) => Ok(Self::Sushi(r)),
                Err(e) => Err(Box::new(e.into())),
            }
        }
    }

    pub fn is_match(&self, input: &[u8]) -> bool {
        match self {
            Self::Redox(r) => !r.matches(input, Some(1)).is_empty(),
            Self::Sushi(r) => r.is_match(input),
        }
    }

    #[allow(dead_code)]
    pub fn find(&self, input: &[u8]) -> Option<Span> {
        match self {
            Self::Redox(r) => {
                let matches = r.matches(input, Some(1)).into_iter().next()?;
                let Some(&Some((start, end))) = matches.first() else {
                    debug_assert!(false);
                    return None;
                };

                Some(Span::from(start..end))
            }
            Self::Sushi(r) => r.find(input).map(|x| x.range().into()),
        }
    }
}

impl From<Option<Span>> for Match {
    fn from(value: Option<Span>) -> Self {
        match value {
            Some(value) => Self {
                rstart: 1 + value.start as isize,
                rlength: (value.end - value.start) as isize,
            },
            None => Self { rstart: 0, rlength: -1 },
        }
    }
}

impl From<Match> for Option<Span> {
    fn from(value: Match) -> Self {
        let length = usize::try_from(value.rlength).ok()?;
        let start = usize::try_from(value.rstart - 1).ok()?;
        Some(Span::from(start..start + length))
    }
}

fn lower_sushi_regex(pattern: &str) -> Result<Hir, Box<RegexError>> {
    let pattern = rewrite_gawk_ops(pattern);
    let ast = AstParser::new()
        .parse(&pattern)
        .map_err(|x| Box::new(x.into()))?;
    let hir = Translator::new()
        .translate(&pattern, &ast)
        .map_err(|x| Box::new(x.into()))?;

    Ok(patch_hir(hir))
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum BracketState {
    Outside,
    JustOpened { after_caret: bool },
    Inside,
    SubExpr(char),
}

fn rewrite_gawk_ops(pattern: &str) -> String {
    const UNICODE_PREFIX: &str = "(?u)";

    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.char_indices().peekable();
    let mut bracket = BracketState::Outside;

    out.push_str(UNICODE_PREFIX);
    while let Some((_, c)) = chars.next() {
        match bracket {
            BracketState::Outside => match c {
                '[' => {
                    bracket = BracketState::JustOpened { after_caret: false };
                    out.push(c);
                }
                '\\' => rewrite_escape(&mut chars, &mut out),
                _ => out.push(c),
            },

            BracketState::JustOpened { after_caret } => match c {
                '^' if !after_caret => {
                    bracket = BracketState::JustOpened { after_caret: true };
                    out.push(c);
                }
                // `]` as the first member (or first after `^`) is literal,
                // not the closed `[]abc]`, `[^]abc]`.
                ']' => {
                    out.push(c);
                    bracket = BracketState::Inside;
                }
                '[' if matches!(chars.peek(), Some((_, ':' | '.' | '='))) => {
                    let (_, delim) = chars.next().unwrap();
                    out.push('[');
                    out.push(delim);
                    bracket = BracketState::SubExpr(delim);
                }
                _ => {
                    out.push(c);
                    bracket = BracketState::Inside;
                }
            },

            BracketState::Inside => match c {
                '[' if matches!(chars.peek(), Some((_, ':' | '.' | '='))) => {
                    let (_, delim) = chars.next().unwrap();
                    out.push('[');
                    out.push(delim);
                    bracket = BracketState::SubExpr(delim);
                }
                ']' => {
                    out.push(c);
                    bracket = BracketState::Outside;
                }
                _ => out.push(c),
            },

            BracketState::SubExpr(delim) => {
                if c == delim && matches!(chars.peek(), Some((_, ']'))) {
                    chars.next();
                    out.push(delim);
                    out.push(']');
                    bracket = BracketState::Inside;
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

fn rewrite_escape(
    chars: &mut std::iter::Peekable<impl Iterator<Item = (usize, char)>>,
    out: &mut String,
) {
    match chars.peek().map(|&(_, c)| c) {
        Some('y') => {
            chars.next();
            out.push_str(r"\b");
        }
        Some('`') => {
            chars.next();
            out.push_str(r"\A");
        }
        Some('\'') => {
            chars.next();
            out.push_str(r"\z");
        }
        Some('<') => {
            chars.next();
            out.push(SENT_WORD_START);
        }
        Some('>') => {
            chars.next();
            out.push(SENT_WORD_END);
        }
        Some('B') => {
            chars.next();
            out.push(SENT_BETWEEN);
        }
        // Ignore escaped brackets.
        Some(c @ ('[' | ']')) => {
            chars.next();
            out.push('\\');
            out.push(c);
        }
        _ => out.push('\\'),
    }
}

/// Post-order walk replacing sentinel literals with real [`Look`] assertions.
fn patch_hir(hir: Hir) -> Hir {
    match hir.into_kind() {
        HirKind::Literal(lit) => {
            let pieces = split_literal(&lit.0);
            match pieces.len() {
                0 => Hir::empty(),
                1 => pieces.into_iter().next().unwrap(),
                _ => Hir::concat(pieces),
            }
        }
        HirKind::Concat(subs) => Hir::concat(subs.into_iter().map(patch_hir).collect()),
        HirKind::Alternation(subs) => Hir::alternation(subs.into_iter().map(patch_hir).collect()),
        HirKind::Repetition(mut rep) => {
            rep.sub = Box::new(patch_hir(*rep.sub));
            Hir::repetition(rep)
        }
        HirKind::Capture(mut cap) => {
            cap.sub = Box::new(patch_hir(*cap.sub));
            Hir::capture(cap)
        }
        HirKind::Class(class) => Hir::class(class),
        HirKind::Look(look) => Hir::look(look),
        HirKind::Empty => Hir::empty(),
    }
}

/// Scans a (possibly sentinel-merged) literal's raw bytes for the fixed
/// 4-byte UTF-8 sequences of our PUA sentinels and splits them out into
/// real [`Look`] assertions, preserving every other byte as literal content.
fn split_literal(bytes: &[u8]) -> Vec<Hir> {
    let flush = |buf: &mut Vec<u8>, out: &mut Vec<Hir>| {
        if !buf.is_empty() {
            out.push(Hir::literal(std::mem::take(buf).into_boxed_slice()));
        }
    };
    let (w_start, w_end, ehalf, shalf) = (
        Look::WordStartUnicode,
        Look::WordEndUnicode,
        Look::WordEndHalfUnicode,
        Look::WordStartHalfUnicode,
    );
    let mut out = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i..].starts_with(&SENT_WORD_START_BYTES) {
            flush(&mut buf, &mut out);
            out.push(Hir::look(w_start));
            i += SENT_WORD_START_BYTES.len();
        } else if bytes[i..].starts_with(&SENT_WORD_END_BYTES) {
            flush(&mut buf, &mut out);
            out.push(Hir::look(w_end));
            i += SENT_WORD_END_BYTES.len();
        } else if bytes[i..].starts_with(&SENT_BETWEEN_BYTES) {
            flush(&mut buf, &mut out);
            out.push(Hir::concat(vec![Hir::look(ehalf), Hir::look(shalf)]));
            i += SENT_BETWEEN_BYTES.len();
        } else {
            buf.push(bytes[i]);
            i += 1;
        }
    }
    flush(&mut buf, &mut out);
    out
}
