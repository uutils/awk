// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{fmt::Debug, iter::Peekable};

use lexer::{LexingError, Logos, Span, SpannedIter, Token};

use crate::{
    ParsingError,
    ast::{Command, SpecialPattern},
};

pub struct Lexer<'a> {
    inner: Peekable<SpannedIter<'a, Token<'a>>>,
    span: Span,
    // source: &'a [u8],
}

type LexItem<'a> = <Lexer<'a> as Iterator>::Item;

impl<'a> Lexer<'a> {
    pub fn new(source: &'a [u8]) -> Self {
        Self {
            inner: Token::lexer(source).spanned().peekable(),
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
    ) -> super::Result<Token<'a>> {
        match self.next() {
            Some(Ok(tok)) if expected == &tok => Ok(tok),
            Some(Ok(_)) => Err(err(self.span())),
            Some(err @ Err(_)) => err.map_err(Into::into),
            None => todo!(),
        }
    }

    pub fn expect_with(
        &mut self,
        expected: impl FnOnce(&Token<'a>) -> bool,
        msg: String,
    ) -> super::Result<Token<'a>> {
        match self.next() {
            Some(Ok(tok)) if expected(&tok) => Ok(tok),
            Some(Ok(_)) => Err(ParsingError::UnexpectedToken(self.span(), msg)),
            Some(err @ Err(_)) => err.map_err(Into::into),
            None => todo!(),
        }
    }

    pub fn expect_identifier(&mut self) -> super::Result<lexer::Identifier<'a>> {
        let Token::Identifier(name) = self.expect_with(
            |t| matches!(t, Token::Identifier(_)),
            "expected an identifier.".into(),
        )?
        else {
            unreachable!()
        };
        Ok(name)
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

    pub fn next_if(&mut self, f: impl FnOnce(&LexItem<'a>) -> bool) -> Option<LexItem<'a>> {
        let next = self.inner.next_if(|(tok, _)| f(tok));
        self.advance_span(next)
    }

    pub fn expect_next(&mut self) -> super::Result<Token<'a>> {
        match self.next() {
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
            Some(Ok(tok)) => Ok(tok),
            Some(Err(err)) => Err(ParsingError::LexingError(err)),
        }
    }

    pub fn expect_peek(&mut self) -> super::Result<&Token<'a>> {
        match self.peek() {
            None => Err(ParsingError::LexingError(LexingError::UnexpectedEof)),
            Some(Ok(tok)) => Ok(tok),
            Some(Err(err)) => Err(ParsingError::LexingError(err.clone())),
        }
    }

    pub fn span(&self) -> Span {
        self.span.clone()
    }

    pub fn peeked_span(&mut self) -> super::Result<Span> {
        self.inner
            .peek()
            .map(|(_, s)| s.clone())
            .ok_or(ParsingError::LexingError(LexingError::UnexpectedEof))
    }

    pub fn peek_with_span(&mut self) -> Option<(super::Result<&Token<'a>>, Span)> {
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
        matches!(self, Token::Number(_) | Token::String(_) | Token::Regex(_)) || self.is_place()
    }
    fn is_expr_start(&self) -> bool {
        self.is_atom()
            || self.is_prefix_op()
            || matches!(
                self,
                Token::OpenParent | Token::FunctionCall(_) | Token::Getline
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
