// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{fmt::Debug, iter::Peekable};

use lexer::{BlobId, IdentId, LexingError, Span, SpannedIter, Token};

use super::Result;
use crate::{
    ParsingError, PlacingFunction,
    ast::{BuiltinFunction, Command, SpecialPattern},
};

pub struct Lexer<'a> {
    inner: Peekable<SpannedIter<'a, Token>>,
    span: Span,
    source: &'a [u8],
}

type LexItem<'a> = <Lexer<'a> as Iterator>::Item;

impl<'a> Lexer<'a> {
    pub fn new(source: &'a [u8]) -> Self {
        Self {
            // TODO: wire in POSIX & GNU strict conformance.
            inner: Token::lex(source, false, false).spanned().peekable(),
            span: Span::default(),
            source,
        }
    }

    pub fn peek(&mut self) -> Option<&LexItem<'a>> {
        self.inner.peek().map(|(tok, _)| tok)
    }

    pub fn peek_with(&mut self, f: impl FnOnce(&Token) -> bool) -> bool {
        self.peek().is_some_and(|r| r.as_ref().is_ok_and(f))
    }

    pub fn peek_is(&mut self, b: &Token) -> bool {
        self.peek()
            .is_some_and(|r| r.as_ref().is_ok_and(|t| t == b))
    }

    pub fn expect(
        &mut self,
        expected: &Token,
        err: impl FnOnce(Span) -> ParsingError,
    ) -> Result<Token> {
        match self.next() {
            Some(Ok(tok)) if expected == &tok => Ok(tok),
            Some(Ok(_)) => Err(err(self.span())),
            Some(err @ Err(_)) => err.map_err(Into::into),
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
        }
    }

    pub fn expect_with(
        &mut self,
        expected: impl FnOnce(&Token) -> bool,
        err: impl FnOnce(Span) -> ParsingError,
    ) -> Result<Token> {
        match self.next() {
            Some(Ok(tok)) if expected(&tok) => Ok(tok),
            Some(Ok(_)) => Err(err(self.span())),
            Some(err @ Err(_)) => err.map_err(Into::into),
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
        }
    }

    pub fn expect_identifier(&mut self) -> Result<IdentId> {
        if let Some(Token::Identifier(ident)) =
            self.next_if(|t| matches!(t, Token::Identifier(_)))?
        {
            Ok(ident)
        } else {
            Err(ParsingError::ExpectedIdentifier(
                self.peeked_span().unwrap_or(self.span()),
                None,
            ))
        }
    }

    pub fn expect_string(&mut self) -> Result<BlobId> {
        if let Some(Token::String(string)) = self.next_if(|t| matches!(t, Token::String(_)))? {
            Ok(string)
        } else {
            Err(ParsingError::UnexpectedToken(
                self.peeked_span().unwrap_or(self.span()),
                "expected a string".into(),
            ))
        }
    }

    pub fn lex_ident(&mut self, source: &[u8]) -> Result<IdentId> {
        let Some(Ok(Token::Identifier(ident))) = Token::lex(source, true, true).next() else {
            return Err(ParsingError::UnexpectedToken(
                (self.span().start + 1..self.span().end - 1).into(),
                "expected a valid, non-qualified identifier.".into(),
            ));
        };
        Ok(ident)
    }

    pub fn consume(&mut self, token: &Token) -> bool {
        if let Some(Ok(next)) = self.peek()
            && next == token
        {
            self.next();
            true
        } else {
            false
        }
    }

    pub fn consume_with(&mut self, f: impl FnOnce(&Token) -> bool) -> bool {
        if let Some(Ok(next)) = self.peek()
            && f(next)
        {
            self.next();
            true
        } else {
            false
        }
    }

    pub fn next_if(&mut self, f: impl FnOnce(&Token) -> bool) -> lexer::Result<Option<Token>> {
        let next = self.inner.next_if(|(tok, _)| tok.as_ref().is_ok_and(f));
        self.advance_span(next).transpose()
    }

    pub fn expect_next(&mut self) -> Result<Token> {
        match self.next() {
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
            Some(Ok(tok)) => Ok(tok),
            Some(Err(err)) => Err(ParsingError::LexingError(err)),
        }
    }

    pub fn expect_peek(&mut self) -> Result<&Token> {
        match self.peek() {
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
            Some(Ok(tok)) => Ok(tok),
            Some(Err(err)) => Err(ParsingError::LexingError(err.clone())),
        }
    }

    pub const fn span(&self) -> Span {
        self.span
    }

    pub fn peeked_span(&mut self) -> Result<Span> {
        self.inner
            .peek()
            .map(|(_, s)| s.clone().into())
            .ok_or(ParsingError::LexingError(LexingError::UnexpectedEof))
    }

    pub fn peek_with_span(&mut self) -> Option<(Result<&Token>, Span)> {
        self.inner.peek().map(|(a, b)| {
            (
                a.as_ref().map_err(|e| ParsingError::LexingError(e.clone())),
                b.clone().into(),
            )
        })
    }

    fn advance_span(
        &mut self,
        next: Option<(LexItem<'a>, impl Into<Span>)>,
    ) -> Option<LexItem<'a>> {
        next.map(|(token, span)| {
            self.span = span.into();
            token
        })
    }

    pub fn is_juxtaposed(&mut self) -> bool {
        self.peeked_span().is_ok_and(|x| x.start == self.span.end)
    }

    pub fn span_peeked_since(&mut self, start: u32) -> Span {
        Span::from(start..self.peeked_span().map_or(self.span().end, |s| s.end))
    }

    pub fn span_peeked_up_to(&mut self, start: u32) -> Span {
        Span::from(start..self.peeked_span().map_or(self.span().end, |s| s.start))
    }

    /// # Safety
    ///
    /// The current token must match UTF-8 source.
    pub unsafe fn src_as_str(&self) -> &'a str {
        unsafe { str::from_utf8_unchecked(&self.source[self.span().ix()]) }
    }
}

impl Iterator for Lexer<'_> {
    type Item = Result<Token, LexingError>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.inner.next();
        self.advance_span(next)
    }
}

pub trait TokenExt {
    fn is_prefix_op(&self) -> bool;
    fn is_atom(&self) -> bool;
    fn is_expr_start(&self) -> bool;
    fn is_ident_place(&self) -> bool;
    fn is_place(&self) -> bool;
    fn is_pattern_start(&self) -> bool;
    fn maps_to_command(&self) -> Option<Command>;
    fn maps_to_builtin(&self) -> Option<BuiltinFunction>;
    fn maps_to_placing_call(&self) -> Option<PlacingFunction>;
    fn maps_to_special_pat(&self) -> Option<SpecialPattern>;
    fn is_stmnt_end(&self) -> bool;
    fn is_stmnt_or_block_end(&self) -> bool;
}

impl TokenExt for Token {
    fn is_prefix_op(&self) -> bool {
        matches!(
            self,
            Self::Increment
                | Self::Decrement
                | Self::Record
                | Self::Negation
                | Self::Minus
                | Self::Plus
        )
    }
    fn is_atom(&self) -> bool {
        matches!(
            self,
            Self::Number(_) | Self::Integer(_) | Self::String(_) | Self::Regex(_)
        ) || self.is_place() && self != &Self::Record
    }
    fn is_expr_start(&self) -> bool {
        self.is_atom()
            || self.is_prefix_op()
            || self.maps_to_builtin().is_some()
            || matches!(
                self,
                Self::IndirectCall(_) | Self::Getline | Self::OpenParent
            )
    }
    fn is_ident_place(&self) -> bool {
        matches!(
            self,
            Self::Identifier(_)
                | Self::NrVariable
                | Self::NfVariable
                | Self::FsVariable
                | Self::FpatVariable
                | Self::FieldwidthsVariable
                | Self::RsVariable
                | Self::RtVariable
                | Self::OfsVariable
                | Self::OrsVariable
                | Self::FilenameVariable
                | Self::ArgcVariable
                | Self::ArgvVariable
                | Self::SubsepVariable
                | Self::FnrVariable
                | Self::OfmtVariable
                | Self::RstartVariable
                | Self::RlengthVariable
                | Self::EnvironVariable
                | Self::ProcinfoVariable
                | Self::SymtabVariable
                | Self::FunctabVariable
        )
    }
    fn is_place(&self) -> bool {
        matches!(self, Self::Record) || self.is_ident_place()
    }
    fn is_pattern_start(&self) -> bool {
        self.is_expr_start() || self.maps_to_special_pat().is_some()
    }
    fn maps_to_command(&self) -> Option<Command> {
        match self {
            Self::Print => Some(Command::Print),
            Self::Printf => Some(Command::Printf),
            _ => None,
        }
    }

    fn maps_to_builtin(&self) -> Option<BuiltinFunction> {
        match self {
            Self::Length => Some(BuiltinFunction::Length),
            Self::Substr => Some(BuiltinFunction::Substr),
            Self::Split => Some(BuiltinFunction::Split),
            Self::Sub => Some(BuiltinFunction::Sub),
            Self::Gsub => Some(BuiltinFunction::Gsub),
            Self::MatchFn => Some(BuiltinFunction::Match),
            Self::Index => Some(BuiltinFunction::Index),
            Self::Sprintf => Some(BuiltinFunction::Sprintf),
            Self::Toupper => Some(BuiltinFunction::Toupper),
            Self::Tolower => Some(BuiltinFunction::Tolower),
            Self::Gensub => Some(BuiltinFunction::Gensub),
            Self::Patsplit => Some(BuiltinFunction::Patsplit),
            Self::Strtonum => Some(BuiltinFunction::Strtonum),
            Self::Close => Some(BuiltinFunction::Close),
            Self::Fflush => Some(BuiltinFunction::Fflush),
            Self::System => Some(BuiltinFunction::System),
            Self::Int => Some(BuiltinFunction::Int),
            Self::Sqrt => Some(BuiltinFunction::Sqrt),
            Self::Exp => Some(BuiltinFunction::Exp),
            Self::Log => Some(BuiltinFunction::Log),
            Self::Sin => Some(BuiltinFunction::Sin),
            Self::Cos => Some(BuiltinFunction::Cos),
            Self::Atan2 => Some(BuiltinFunction::Atan2),
            Self::Rand => Some(BuiltinFunction::Rand),
            Self::Srand => Some(BuiltinFunction::Srand),
            Self::Systime => Some(BuiltinFunction::Systime),
            Self::Mktime => Some(BuiltinFunction::Mktime),
            Self::Strftime => Some(BuiltinFunction::Strftime),
            Self::Typeof => Some(BuiltinFunction::Typeof),
            Self::Isarray => Some(BuiltinFunction::Isarray),
            Self::Asort => Some(BuiltinFunction::Asort),
            Self::Asorti => Some(BuiltinFunction::Asorti),
            Self::And => Some(BuiltinFunction::And),
            Self::Or => Some(BuiltinFunction::Or),
            Self::Xor => Some(BuiltinFunction::Xor),
            Self::Compl => Some(BuiltinFunction::Compl),
            Self::Lshift => Some(BuiltinFunction::Lshift),
            Self::Rshift => Some(BuiltinFunction::Rshift),
            _ => None,
        }
    }
    fn maps_to_placing_call(&self) -> Option<PlacingFunction> {
        match self {
            Self::Sub => Some(PlacingFunction::Sub),
            Self::Gensub => Some(PlacingFunction::Gsub),
            _ => None,
        }
    }
    fn maps_to_special_pat(&self) -> Option<SpecialPattern> {
        match self {
            Self::BeginPattern => Some(SpecialPattern::Begin),
            Self::EndPattern => Some(SpecialPattern::End),
            Self::BeginFilePattern => Some(SpecialPattern::BeginFile),
            Self::EndFilePattern => Some(SpecialPattern::EndFile),
            _ => None,
        }
    }
    fn is_stmnt_end(&self) -> bool {
        matches!(self, Self::Newline | Self::Semicolon)
    }
    fn is_stmnt_or_block_end(&self) -> bool {
        self.is_stmnt_end() || self == &Self::ClosedBrace
    }
}

impl Debug for Lexer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Lexer {{ span: {:?} }}", self.span)
    }
}

pub trait SpanExt: Copy {
    fn since(self, end: u32) -> Self;
    fn up_from(self, start: u32) -> Self;
}

impl SpanExt for Span {
    #[inline]
    fn since(self, start: u32) -> Self {
        Self { start, end: self.end }
    }

    #[inline]
    fn up_from(self, start: u32) -> Self {
        Self { start, end: self.start }
    }
}
