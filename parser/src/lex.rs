// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{fmt::Debug, iter::Peekable};

use bumpalo::Bump;
use lexer::{Identifier, LexingError, Slice, Span, SpannedIter, Token};

use super::Result;
use crate::{
    ParsingError,
    ast::{BuiltinFunction, Command, SpecialPattern},
};

pub struct Lexer<'a> {
    inner: Peekable<SpannedIter<'a, Token<'a>>>,
    span: Span,
    // source: &'a [u8],
}

type LexItem<'a> = <Lexer<'a> as Iterator>::Item;

impl<'a> Lexer<'a> {
    pub fn new(source: &'a [u8], arena: &'a Bump) -> Self {
        Self {
            // TODO: wire in POSIX & GNU strict conformance.
            inner: Token::lex(source, arena, false, false).spanned().peekable(),
            span: Span::default(),
            // source,
        }
    }

    pub fn peek(&mut self) -> Option<&LexItem<'a>> {
        self.inner.peek().map(|(tok, _)| tok)
    }

    pub fn peek_with(&mut self, f: impl FnOnce(&Token<'a>) -> bool) -> bool {
        self.peek().is_some_and(|r| r.as_ref().is_ok_and(f))
    }

    pub fn peek_is(&mut self, b: &Token<'a>) -> bool {
        self.peek()
            .is_some_and(|r| r.as_ref().is_ok_and(|t| t == b))
    }

    pub fn expect(
        &mut self,
        expected: &Token,
        err: impl FnOnce(Span) -> ParsingError,
    ) -> Result<Token<'a>> {
        match self.next() {
            Some(Ok(tok)) if expected == &tok => Ok(tok),
            Some(Ok(_)) => Err(err(self.span())),
            Some(err @ Err(_)) => err.map_err(Into::into),
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
        }
    }

    pub fn expect_with(
        &mut self,
        expected: impl FnOnce(&Token<'a>) -> bool,
        msg: String,
    ) -> Result<Token<'a>> {
        match self.next() {
            Some(Ok(tok)) if expected(&tok) => Ok(tok),
            Some(Ok(_)) => Err(ParsingError::UnexpectedToken(self.span(), msg)),
            Some(err @ Err(_)) => err.map_err(Into::into),
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
        }
    }

    pub fn expect_identifier(&mut self) -> Result<Identifier<'a>> {
        if let Some(Token::Identifier(ident)) =
            self.next_if(|t| matches!(t, Token::Identifier(_)))?
        {
            Ok(ident)
        } else {
            Err(ParsingError::UnexpectedToken(
                self.peeked_span().unwrap_or(self.span()),
                "expected an identifier.".into(),
            ))
        }
    }

    pub fn expect_string(&mut self) -> Result<Slice<'a>> {
        if let Some(Token::String(string)) = self.next_if(|t| matches!(t, Token::String(_)))? {
            Ok(string)
        } else {
            Err(ParsingError::UnexpectedToken(
                self.peeked_span().unwrap_or(self.span()),
                "expected a string".into(),
            ))
        }
    }

    pub fn lex_ident(&self, source: &[u8], arena: &'a Bump) -> Result<&'a str> {
        let Some(Ok(Token::Identifier(ident))) = Token::lex(source, arena, true, true).next()
        else {
            return Err(ParsingError::UnexpectedToken(
                self.span().start + 1..self.span().end - 1,
                "expected a valid, non-qualified identifier.".into(),
            ));
        };
        Ok(arena.alloc_str(ident.literal))
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

    pub fn consume_with(&mut self, f: impl FnOnce(&Token<'a>) -> bool) -> bool {
        if let Some(Ok(next)) = self.peek()
            && f(next)
        {
            self.next();
            true
        } else {
            false
        }
    }

    pub fn next_if(
        &mut self,
        f: impl FnOnce(&Token<'a>) -> bool,
    ) -> lexer::Result<Option<Token<'a>>> {
        let next = self.inner.next_if(|(tok, _)| tok.as_ref().is_ok_and(f));
        self.advance_span(next).transpose()
    }

    pub fn expect_next(&mut self) -> Result<Token<'a>> {
        match self.next() {
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
            Some(Ok(tok)) => Ok(tok),
            Some(Err(err)) => Err(ParsingError::LexingError(err)),
        }
    }

    pub fn expect_peek(&mut self) -> Result<&Token<'a>> {
        match self.peek() {
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
            Some(Ok(tok)) => Ok(tok),
            Some(Err(err)) => Err(ParsingError::LexingError(err.clone())),
        }
    }

    pub fn span(&self) -> Span {
        self.span.clone()
    }

    pub fn peeked_span(&mut self) -> Result<Span> {
        self.inner
            .peek()
            .map(|(_, s)| s.clone())
            .ok_or(ParsingError::LexingError(LexingError::UnexpectedEof))
    }

    pub fn peek_with_span(&mut self) -> Option<(Result<&Token<'a>>, Span)> {
        self.inner.peek().map(|(a, b)| {
            (
                a.as_ref().map_err(|e| ParsingError::LexingError(e.clone())),
                b.clone(),
            )
        })
    }

    fn advance_span(&mut self, next: Option<(LexItem<'a>, Span)>) -> Option<LexItem<'a>> {
        next.map(|(token, span)| {
            self.span = span.clone();
            token
        })
    }

    pub fn is_yuxtaposed(&mut self) -> bool {
        self.peeked_span().is_ok_and(|x| x.start == self.span.end)
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Result<Token<'a>, LexingError>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.inner.next();
        self.advance_span(next)
    }
}

pub trait TokenExt {
    fn is_prefix_op(&self) -> bool;
    fn is_atom(&self) -> bool;
    fn is_expr_start(&self) -> bool;
    fn is_place(&self) -> bool;
    fn is_pattern_start(&self) -> bool;
    fn maps_to_command(&self) -> Option<Command>;
    fn maps_to_builtin(&self) -> Option<BuiltinFunction>;
    fn maps_to_special_pat(&self) -> Option<SpecialPattern>;
    fn is_stmnt_end(&self) -> bool;
    fn is_stmnt_or_block_end(&self) -> bool;
    fn is_brace(&self) -> bool;
}

impl TokenExt for Token<'_> {
    fn is_prefix_op(&self) -> bool {
        matches!(
            self,
            Token::Increment
                | Token::Decrement
                | Token::Record
                | Token::Negation
                | Token::Minus
                | Token::Plus
        )
    }
    fn is_atom(&self) -> bool {
        matches!(
            self,
            Token::Number(_) | Token::Integer(_) | Token::String(_) | Token::Regex(_)
        ) || self.is_place() && self != &Token::Record
    }
    fn is_expr_start(&self) -> bool {
        self.is_atom()
            || self.is_prefix_op()
            || matches!(
                self,
                Token::IndirectCall(_) | Token::Getline | Token::OpenParent
            )
    }
    fn is_place(&self) -> bool {
        matches!(
            self,
            Token::NrVariable
                | Token::NfVariable
                | Token::FsVariable
                | Token::RsVariable
                | Token::OfsVariable
                | Token::OrsVariable
                | Token::FilenameVariable
                | Token::ArgcVariable
                | Token::ArgvVariable
                | Token::SubsepVariable
                | Token::FnrVariable
                | Token::OfmtVariable
                | Token::RstartVariable
                | Token::RlengthVariable
                | Token::EnvironVariable
                | Token::Identifier(_)
                | Token::Record
        )
    }
    fn is_pattern_start(&self) -> bool {
        self.is_expr_start() || self.maps_to_special_pat().is_some()
    }
    fn maps_to_command(&self) -> Option<Command> {
        match self {
            Token::Print => Some(Command::Print),
            Token::Printf => Some(Command::Printf),
            _ => None,
        }
    }

    fn maps_to_builtin(&self) -> Option<BuiltinFunction> {
        match self {
            Token::Length => Some(BuiltinFunction::Length),
            Token::Substr => Some(BuiltinFunction::Substr),
            Token::Split => Some(BuiltinFunction::Split),
            Token::Sub => Some(BuiltinFunction::Sub),
            Token::Gsub => Some(BuiltinFunction::Gsub),
            Token::MatchFn => Some(BuiltinFunction::Match),
            Token::Index => Some(BuiltinFunction::Index),
            Token::Sprintf => Some(BuiltinFunction::Sprintf),
            Token::Toupper => Some(BuiltinFunction::Toupper),
            Token::Tolower => Some(BuiltinFunction::Tolower),
            Token::Gensub => Some(BuiltinFunction::Gensub),
            Token::Patsplit => Some(BuiltinFunction::Patsplit),
            Token::Strtonum => Some(BuiltinFunction::Strtonum),
            Token::Close => Some(BuiltinFunction::Close),
            Token::Fflush => Some(BuiltinFunction::Fflush),
            Token::System => Some(BuiltinFunction::System),
            Token::Int => Some(BuiltinFunction::Int),
            Token::Sqrt => Some(BuiltinFunction::Sqrt),
            Token::Exp => Some(BuiltinFunction::Exp),
            Token::Log => Some(BuiltinFunction::Log),
            Token::Sin => Some(BuiltinFunction::Sin),
            Token::Cos => Some(BuiltinFunction::Cos),
            Token::Atan2 => Some(BuiltinFunction::Atan2),
            Token::Rand => Some(BuiltinFunction::Rand),
            Token::Srand => Some(BuiltinFunction::Srand),
            Token::Systime => Some(BuiltinFunction::Systime),
            Token::Mktime => Some(BuiltinFunction::Mktime),
            Token::Strftime => Some(BuiltinFunction::Strftime),
            Token::Typeof => Some(BuiltinFunction::Typeof),
            Token::Isarray => Some(BuiltinFunction::Isarray),
            Token::Asort => Some(BuiltinFunction::Asort),
            Token::Asorti => Some(BuiltinFunction::Asorti),
            Token::And => Some(BuiltinFunction::And),
            Token::Or => Some(BuiltinFunction::Or),
            Token::Xor => Some(BuiltinFunction::Xor),
            Token::Compl => Some(BuiltinFunction::Compl),
            Token::Lshift => Some(BuiltinFunction::Lshift),
            Token::Rshift => Some(BuiltinFunction::Rshift),
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
        matches!(self, Token::Newline | Token::Semicolon)
    }
    fn is_stmnt_or_block_end(&self) -> bool {
        self.is_stmnt_end() || self == &Token::ClosedBrace
    }
    fn is_brace(&self) -> bool {
        matches!(self, Token::OpenBrace | Token::ClosedBrace)
    }
}

impl Debug for Lexer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Lexer {{ span: {:?} }}", self.span)
    }
}
