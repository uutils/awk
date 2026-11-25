// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

mod ast;
mod diagnostics;
mod idempotency;
mod lex;
mod pratt;
mod sexpr;
#[cfg(test)]
mod tests;

use std::{
    borrow::Cow,
    ffi::OsString,
    fs::File,
    io,
    mem::{replace, take},
    path::{Path, PathBuf},
    rc::Rc,
};

use ahash::RandomState;
use bumpalo::{Bump, boxed::Box, collections::Vec, vec};
use derive_more::Debug;
use either::Either::{Left, Right};
use hashbrown::HashMap;
use lexer::Token;
pub use lexer::{BlobId, IdentId, Span, StaticInterner};

pub use crate::{
    ast::*,
    diagnostics::{
        AriadneSpan, Diagnostic, DiagnosticStore, FileCache, ParsingError, cannot_open_source,
    },
    lex::{Lexer, SpanExt},
};
use crate::{lex::TokenExt, pratt::Pratt};

type Result<T, E = ParsingError> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct Parser<'a> {
    ast: Ast<'a>,
    #[debug(ignore)]
    arena: &'a Bump,
    #[debug(ignore)]
    preprocessor: Preprocessor,
    #[debug(ignore)]
    file: FileCache,
    namespace: IdentId,
    concurrent: bool,
    // Disables file include materialization and enables metadata recording.
    dry: bool,
    /// Whether `break` is allowed (inside a loop or `switch`).
    break_allowed: bool,
    /// Whether `continue` is allowed (inside a loop only).
    continue_allowed: bool,
    /// Whether `return` is allowed (inside a function body).
    return_allowed: bool,
    /// Whether `next` is allowed (forbidden in all special rules).
    next_allowed: bool,
    /// Whether `nextfile` is allowed (allowed in rules and BEGINFILE only).
    nextfile_allowed: bool,
}

impl<'a> Parser<'a> {
    pub fn new(arena: &'a Bump, dry: bool) -> Self {
        Self {
            ast: Ast::new(arena),
            arena,
            preprocessor: Preprocessor {},
            file: FileCache::Cli(0),
            namespace: StaticInterner::awk_ident(),
            concurrent: false,
            dry,
            break_allowed: false,
            continue_allowed: false,
            return_allowed: false,
            next_allowed: true,
            nextfile_allowed: true,
        }
    }

    pub fn parse(
        &mut self,
        file: FileCache,
        source: &'a [u8],
    ) -> Result<&mut Ast<'a>, DiagnosticStore> {
        self.file.clone_from(&file);
        self.ast.diagnostics.cache(self.file.clone(), source);
        let mut lex = Lexer::new(source);
        match self.parse_top(&mut lex, true) {
            Ok(_) => Ok(&mut self.ast),
            Err(error) => {
                let mut store = take(&mut self.ast.diagnostics);
                error.add_diagnostic(&mut store, file, source);
                Err(store)
            }
        }
    }

    fn parse_top(&mut self, lex: &mut Lexer<'a>, _awk_namespace: bool) -> Result<&Ast<'a>> {
        // Reset statement-context flags in case this parser is reused after an error.
        self.break_allowed = false;
        self.continue_allowed = false;
        self.return_allowed = false;

        // Expects:
        //   * Directive
        //     * Namespace: Either handle here or in interpreter; idk.
        //     * Include: recursively lex & parse the filename.
        //     * Concurrent: Pass on to interpreter.
        //     * Load: Pass on to interpreter.
        //   * Pattern (Expression)
        //     * Expects brackets afterwards (body) or a newline (default).
        //   * Action (Statement)
        //     * Expects a newline afterwards.
        while let Some(tok) = lex.peek() {
            if tok.as_ref().is_ok_and(Token::is_pattern_start) {
                match self.parse_pattern(lex)? {
                    Left(rule_pattern) => {
                        let body = lex.peek_is(&Token::OpenBrace).then(|| self.parse_body(lex));
                        self.add_rule(Rule {
                            pattern: Some(rule_pattern),
                            actions: body.transpose()?,
                        });
                    }
                    Right(special_pattern) => {
                        lex.next();

                        self.next_allowed = false;
                        self.nextfile_allowed =
                            matches!(special_pattern, SpecialPattern::BeginFile);
                        let body = self.parse_body(lex);
                        self.nextfile_allowed = true;
                        self.next_allowed = true;

                        match special_pattern {
                            SpecialPattern::Begin => &mut self.ast.begin,
                            SpecialPattern::End => &mut self.ast.end,
                            SpecialPattern::BeginFile => &mut self.ast.begin_file,
                            SpecialPattern::EndFile => &mut self.ast.end_file,
                        }
                        .push(body?);
                    }
                }
            } else if lex.peek_is(&Token::OpenBrace) {
                let actions = Some(self.parse_body(lex)?);
                self.add_rule(Rule { pattern: None, actions });
            } else {
                match lex.expect_next()? {
                    Token::LoadDirective => {
                        let lib = lex.expect_string()?;
                        self.ast.loads.push(lib);
                        lex.expect_with(Token::is_stmnt_end, ParsingError::ExpectedStatementEnd)?;
                    }
                    Token::IncludeDirective => {
                        let path = lex.expect_string()?;
                        let span = lex.span();
                        if let (Err(e), _) =
                            self.include(make_path(path), span, Some(StaticInterner::awk_ident()))
                        {
                            return Err(e);
                        }
                        lex.expect_with(Token::is_stmnt_end, ParsingError::ExpectedStatementEnd)?;
                    }
                    Token::NsIncludeDirective => {
                        let path = lex.expect_string()?;
                        let span = lex.span();
                        if let (Err(e), _) = self.include(make_path(path), span, None) {
                            return Err(e);
                        }
                        lex.expect_with(Token::is_stmnt_end, ParsingError::ExpectedStatementEnd)?;
                    }
                    Token::NamespaceDirective => {
                        let ns = StaticInterner::resolve(lex.expect_string()?, <[u8]>::to_vec);
                        let ns = lex.lex_ident(&ns)?;
                        lex.expect_with(Token::is_stmnt_end, ParsingError::ExpectedStatementEnd)?;
                        if self.namespace == ns {
                            continue; // skip counting.
                        }
                        self.namespace = ns;
                        if self.dry {
                            self.ast.ns_metadata.store(ns);
                        }
                    }
                    Token::ConcurrentDirective => {
                        if lex.peek_with(|t| t.maps_to_special_pat().is_some()) || self.concurrent {
                            return Err(ParsingError::UnexpectedToken(
                                lex.span(),
                                "specified more than once.".into(),
                            ));
                        }
                        self.concurrent = true;
                    }
                    Token::Function => self.parse_function(lex)?,
                    Token::Newline | Token::Semicolon if self.concurrent => {
                        return Err(ParsingError::UnexpectedToken(
                            lex.span(),
                            "a pattern was expected.".into(),
                        ));
                    }
                    Token::Newline | Token::Semicolon => continue, // skip counting.
                    _ => {
                        return Err(ParsingError::UnexpectedToken(
                            lex.span(),
                            "invalid rule beginning.".into(),
                        ));
                    }
                }
            }
            self.ast.ns_metadata.tick();
        }
        Ok(&self.ast)
    }

    /// Parses up until `{`.
    fn parse_pattern(&mut self, lex: &mut Lexer<'a>) -> Result<Pattern<'a>> {
        match lex.expect_peek()? {
            Token::BeginPattern => Ok(Right(SpecialPattern::Begin)),
            Token::EndPattern => Ok(Right(SpecialPattern::End)),
            Token::BeginFilePattern => Ok(Right(SpecialPattern::BeginFile)),
            Token::EndFilePattern => Ok(Right(SpecialPattern::EndFile)),
            _ => {
                let expr = self.parse_expression(lex, false)?;
                Ok(Left(if lex.consume(&Token::Comma) {
                    let expr_end = self.parse_expression(lex, false)?;
                    RulePattern::Range(expr, expr_end)
                } else {
                    RulePattern::Expression(expr)
                }))
            }
        }
    }

    /// Parses up until `}`. Inserts a lone print statement if none.
    fn parse_body(&mut self, lex: &mut Lexer<'a>) -> Result<Body<'a>> {
        lex.expect(&Token::OpenBrace, ParsingError::ExpectedOpeningBrace)?;
        let mut body = Vec::new_in(self.arena);
        let mut depth = 0;
        let mut after_separator = true;
        let start_span = lex.span().start;

        loop {
            after_separator |= lex.consume_with(Token::is_stmnt_end);

            match lex.peek() {
                Some(Ok(Token::ClosedBrace)) if depth == 0 => {
                    lex.next();
                    break Ok(Body(body));
                }
                Some(Ok(Token::ClosedBrace)) => {
                    depth -= 1;
                    lex.next();
                    after_separator = true;
                }
                Some(Ok(Token::OpenBrace)) if after_separator || depth > 0 => {
                    depth += 1;
                    lex.next();
                    after_separator = false;
                }
                Some(Ok(Token::OpenBrace)) => {
                    lex.next();
                    break Err(ParsingError::ExpectedStatementEnd(lex.span()));
                }
                Some(_) if after_separator || depth > 0 => {
                    let (statement, consumed) = self.parse_statement_with_trailing(lex)?;
                    body.push(statement);
                    after_separator = consumed;
                }
                Some(_) => {
                    break Err(ParsingError::ExpectedStatementEnd(lex.span()));
                }
                None => {
                    break Err(ParsingError::UnclosedScope(lex.span().since(start_span)));
                }
            }
        }
    }

    /// These are a subset of statements usable in places like for-loop defs.
    fn parse_simple_statement(
        &mut self,
        lex: &mut Lexer<'a>,
    ) -> Option<Result<SimpleStatement<'a>>> {
        let (peek, start) = lex.peek_with_span()?;
        let (peek, start) = (peek.ok()?, start.start);
        if peek.is_expr_start() {
            Some(self.parse_expression(lex, false).map(|e| {
                SimpleStatement::Expression(e, self.gen_metadata(lex.span().since(start)))
            }))
        } else {
            match peek {
                token if let Some(name) = token.maps_to_command() => {
                    lex.next();
                    Some(self.parse_command(lex, name))
                }
                Token::Delete => {
                    lex.next();
                    Some(self.parse_delete(lex))
                }
                _ => None,
            }
        }
    }

    fn parse_statement(&mut self, lex: &mut Lexer<'a>) -> Result<Statement<'a>> {
        self.parse_statement_with_trailing(lex)
            .map(|(statement, _)| statement)
    }

    fn parse_statement_with_trailing(
        &mut self,
        lex: &mut Lexer<'a>,
    ) -> Result<(Statement<'a>, bool)> {
        let mut self_terminated = false;
        let statement = if let Some(statement) = self.parse_simple_statement(lex) {
            Statement::Simple(statement?)
        } else {
            let next = lex.expect_next()?;
            let start = lex.span().start;
            if lex.is_juxtaposed() && lex.peek_is(&Token::PathSpec) {
                return Err(ParsingError::ExpectedIdentifier(lex.span(), None));
            }
            match next {
                Token::If => {
                    let condition = self.parse_parenthesized_expr(lex)?;
                    let (then_body, then_terminated) = self.parse_statement_body(lex)?;
                    let (else_body, terminated) = if lex.consume(&Token::Else) {
                        let (body, terminated) = self.parse_statement_body(lex)?;
                        (Some(body), terminated)
                    } else {
                        (None, then_terminated)
                    };
                    self_terminated = terminated;
                    let metadata = self.gen_metadata(lex.span().since(start));
                    Statement::If { condition, then_body, else_body, metadata }
                }
                Token::For => {
                    lex.expect(&Token::OpenParent, ParsingError::ExpectedOpeningParenthesis)?;

                    let (statement, terminated) = if lex.peek_is(&Token::Semicolon) {
                        self.parse_for_loop(lex, None, start)?
                    } else {
                        let enclosed = lex.consume(&Token::OpenParent);
                        let Some(stmnt) = self.parse_simple_statement(lex).transpose()? else {
                            return Err(ParsingError::InvalidForLoop(lex.span()));
                        };
                        if enclosed {
                            lex.expect(
                                &Token::ClosedParent,
                                ParsingError::UnclosedParenthesisInStatement,
                            )?;
                            self.parse_for_loop(lex, Some(stmnt), start)?
                        } else {
                            self.parse_for_ambiguous(lex, Some(stmnt), start)?
                        }
                    };
                    self_terminated = terminated;
                    statement
                }
                Token::Switch => {
                    let scrutinee = self.parse_parenthesized_expr(lex)?;
                    lex.expect(&Token::OpenBrace, ParsingError::ExpectedOpeningBrace)?;
                    let mut default = None;
                    let mut branches = Vec::new_in(self.arena);
                    let mut case = None;
                    let mut body = Vec::new_in(self.arena);

                    // `break` is allowed in `switch`; `continue` is not (gawk-compatible).
                    // Only set if currently unset; only clear if this frame set it.
                    let set_break = !self.break_allowed;
                    if set_break {
                        self.break_allowed = true;
                    }

                    while !lex.consume(&Token::ClosedBrace) {
                        if lex.peek_is(&Token::Case) {
                            match case.take() {
                                Some(Right(())) => {
                                    default = Some((
                                        replace(&mut body, Vec::new_in(self.arena)).into(),
                                        branches.len(),
                                    ));
                                }
                                Some(Left(atom)) => branches.push((
                                    atom,
                                    replace(&mut body, Vec::new_in(self.arena)).into(),
                                )),
                                _ => {}
                            }
                            case = Some(Left(self.parse_case(lex)?));
                        } else if lex.consume(&Token::Default) {
                            let span = lex.span();
                            lex.expect(&Token::Colon, ParsingError::ColonMustFollowCase)?;
                            if default.is_some() || matches!(case, Some(Right(()))) {
                                return Err(ParsingError::DuplicatedDefaultBranch(span));
                            } else if let Some(Left(atom)) = case {
                                branches.push((
                                    atom,
                                    replace(&mut body, Vec::new_in(self.arena)).into(),
                                ));
                            }
                            case = Some(Right(()));
                        } else {
                            if case.is_none() {
                                return Err(ParsingError::MissingSwitchBranch(lex.span()));
                            }
                            let statement = self.parse_statement(lex)?;
                            body.push(statement);
                        }
                    }
                    if set_break {
                        self.break_allowed = false;
                    }
                    match case.take() {
                        Some(Right(())) => default = Some((body.into(), branches.len())),
                        Some(Left(atom)) => branches.push((atom, body.into())),
                        _ => {}
                    }
                    let metadata = self.gen_metadata(lex.span().since(start));

                    // Always ends in an explicit `}`, so it's self-terminated.
                    self_terminated = true;
                    Statement::Switch { scrutinee, branches, default, metadata }
                }
                Token::While => {
                    let condition = self.parse_parenthesized_expr(lex)?;
                    let (then_body, terminated) =
                        self.with_loop_context(|this| this.parse_statement_body(lex))?;
                    self_terminated = terminated;
                    let metadata = self.gen_metadata(lex.span().since(start));
                    Statement::While { condition, then_body, metadata }
                }
                Token::Do => {
                    let then_body = self.with_loop_context(|this| this.parse_body(lex))?;
                    lex.expect(&Token::While, ParsingError::MissingWhileAfterDo)?;
                    let condition = self.parse_parenthesized_expr(lex)?;
                    let metadata = self.gen_metadata(lex.span().since(start));
                    Statement::DoWhile { then_body, condition, metadata }
                }
                Token::Break => {
                    if !self.break_allowed {
                        return Err(ParsingError::BreakOutsideLoopOrSwitch(lex.span()));
                    }
                    Statement::Break(self.gen_metadata(lex.span()))
                }
                Token::Continue => {
                    if !self.continue_allowed {
                        return Err(ParsingError::ContinueOutsideLoop(lex.span()));
                    }
                    Statement::Continue(self.gen_metadata(lex.span()))
                }
                Token::Return => {
                    if !self.return_allowed {
                        return Err(ParsingError::ReturnOutsideFunction(lex.span()));
                    }
                    Statement::Return(
                        (!lex.peek_with(Token::is_stmnt_or_block_end))
                            .then(|| self.parse_expression(lex, true))
                            .transpose()?,
                        self.gen_metadata(lex.span().since(start)),
                    )
                }
                Token::Next if !self.next_allowed => {
                    return Err(ParsingError::NextInForbiddenCtx(lex.span()));
                }
                Token::NextFile if !self.nextfile_allowed => {
                    return Err(ParsingError::NextFileInForbiddenCtx(lex.span()));
                }
                Token::Next => Statement::Next(self.gen_metadata(lex.span())),
                Token::NextFile => Statement::NextFile(self.gen_metadata(lex.span())),
                Token::Exit => Statement::Exit(
                    (!lex.peek_with(Token::is_stmnt_or_block_end))
                        .then(|| self.parse_expression(lex, false))
                        .transpose()?,
                    self.gen_metadata(lex.span().since(start)),
                ),
                _ => {
                    return Err(ParsingError::UnexpectedToken(
                        lex.span(),
                        "invalid statement start.".into(),
                    ));
                }
            }
        };

        let consumed = lex.consume_with(Token::is_stmnt_end) || self_terminated;
        Ok((statement, consumed))
    }

    fn parse_parenthesized_expr(&mut self, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        lex.expect(
            &Token::OpenParent,
            ParsingError::MissingParenthesisInStatement,
        )?;
        let expr = self.parse_expression(lex, false)?;
        lex.expect(
            &Token::ClosedParent,
            ParsingError::UnclosedParenthesisInStatement,
        )?;
        Ok(expr)
    }

    fn parse_for_loop(
        &mut self,
        lex: &mut Lexer<'a>,
        init: Option<SimpleStatement<'a>>,
        start: u32,
    ) -> Result<(Statement<'a>, bool)> {
        lex.consume(&Token::Semicolon);
        lex.consume(&Token::Newline);
        let condition = (!lex.peek_is(&Token::Semicolon))
            .then(|| self.parse_expression(lex, false))
            .transpose()?;
        lex.expect(&Token::Semicolon, ParsingError::InvalidForLoop)?;

        lex.consume(&Token::Newline);
        let update = if lex.peek_is(&Token::ClosedParent) {
            None
        } else {
            let Some(stmnt) = self.parse_simple_statement(lex) else {
                return Err(ParsingError::InvalidForLoop(lex.span()));
            };
            Some(stmnt?)
        };

        lex.expect(&Token::ClosedParent, ParsingError::InvalidForLoop)?;
        let (body, terminated) = self.with_loop_context(|this| this.parse_statement_body(lex))?;
        let metadata = self.gen_metadata(lex.span().since(start));
        Ok((
            Statement::For { init, condition, update, body, metadata },
            terminated,
        ))
    }

    fn parse_for_ambiguous(
        &mut self,
        lex: &mut Lexer<'a>,
        expr: Option<SimpleStatement<'a>>,
        start: u32,
    ) -> Result<(Statement<'a>, bool)> {
        let (variable, array) = match expr {
            Some(SimpleStatement::Expression(Expr::Node(node, _), _))
                if matches!(&*node, ExprNode::InArray(_, _)) =>
            {
                let node = Box::into_inner(node);
                if let ExprNode::InArray(array, expr) = node
                    && let [Expr::Leaf(Atom::Variable(var), _)] = expr.as_slice()
                {
                    (*var, array)
                } else {
                    return Err(ParsingError::InvalidForLoop(lex.span()));
                }
            }
            expr => return self.parse_for_loop(lex, expr, start),
        };

        lex.expect(
            &Token::ClosedParent,
            ParsingError::UnclosedParenthesisInStatement,
        )?;

        let (body, terminated) = self.with_loop_context(|this| this.parse_statement_body(lex))?;
        let metadata = self.gen_metadata(lex.span().since(start));
        Ok((
            Statement::ForEach { variable, array, body, metadata },
            terminated,
        ))
    }

    fn parse_statement_body(&mut self, lex: &mut Lexer<'a>) -> Result<(Body<'a>, bool)> {
        // Ignore newlines in this position.
        lex.consume(&Token::Newline);

        // Braced body, with >=1 statements.
        if lex.peek_is(&Token::OpenBrace) {
            return Ok((self.parse_body(lex)?, true));
        }

        // Empty body check. Accepts cases like `if (...);`.
        if lex.consume(&Token::Semicolon) {
            return Ok((Vec::new_in(self.arena).into(), true));
        }

        // Parses a single statement.
        let start = lex.peeked_span().map_or(lex.span().start, |s| s.start);
        let (statement, terminator) = self.parse_statement_with_trailing(lex)?;

        if terminator || lex.peek_is(&Token::ClosedBrace) {
            Ok((vec![in self.arena; statement].into(), terminator))
        } else {
            Err(ParsingError::ExpectedStatementEnd(lex.span().since(start)))
        }
    }

    fn parse_case(&mut self, lex: &mut Lexer<'a>) -> Result<Atom> {
        lex.expect(&Token::Case, ParsingError::MissingSwitchBranch)?;
        let next = lex.expect_next()?;
        let value = self.parse_atom(lex, next, true)?;
        lex.expect(&Token::Colon, ParsingError::ColonMustFollowCase)?;
        match value {
            Atom::Variable(_) => Err(ParsingError::InvalidCaseValue(lex.span())),
            _ => Ok(value),
        }
    }

    fn parse_command(&mut self, lex: &mut Lexer<'a>, name: Command) -> Result<SimpleStatement<'a>> {
        let start = lex.span().start;
        let parent = lex.consume(&Token::OpenParent);
        let args = if parent {
            let mut args = self.parse_function_args(lex)?;
            lex.expect(
                &Token::ClosedParent,
                ParsingError::UnclosedParenthesisInStatement,
            )?;
            // Handles ambiguities of `print (1, 2) in arr`. Lol.
            if lex.consume(&Token::In) {
                let array = match Pratt::new(self, false).parse_place(lex)? {
                    Place::Variable(var) => ArrayPlace(var, Vec::new_in(self.arena)),
                    Place::Array(place) => place,
                    Place::Record(_) => {
                        return Err(ParsingError::OperatorExpectsVariable(
                            lex.span().since(start),
                        ));
                    }
                };
                let test = replace(&mut args, Vec::new_in(self.arena));

                args.push(Expr::node(
                    ExprNode::InArray(array, test),
                    self,
                    lex.span().since(start),
                ));

                if lex.consume(&Token::Comma) && !lex.peek_with(Token::is_expr_start) {
                    return Err(ParsingError::InvalidExpression(
                        lex.span(),
                        String::from("Expected next argument!"),
                    ));
                }
                self.parse_command_args(lex, &mut args)?;
            } else if lex.consume(&Token::Comma) {
                let is_err = args.len() > 1;
                // We parse anyway to advance the lexer.
                let start_extra = lex.span().start;
                let parent_args = lex.span().since(start);
                self.parse_command_args(lex, &mut args)?;
                if is_err {
                    return Err(ParsingError::CommandDoubleCall(
                        parent_args,
                        lex.span().since(start_extra),
                    ));
                }
            // Handles ambiguities like `print (1 + 1) / 2`
            } else if args.len() == 1 && !lex.peek_with(Token::is_stmnt_or_block_end) {
                let mut parser = Pratt::new(self, false);
                let lhs = args.pop().unwrap();
                args.push(
                    parser.fold_rhs(lex, lhs, start, 0, |t| Redirection::parse(t).is_some())?,
                );
                // `print (1) + 1, 2`: the rest of the list follows a comma
                if lex.consume(&Token::Comma) {
                    if !lex.peek_with(Token::is_expr_start) {
                        return Err(ParsingError::InvalidExpression(
                            lex.span(),
                            String::from("Expected next argument!"),
                        ));
                    }
                    self.parse_command_args(lex, &mut args)?;
                }
            }
            args
        } else {
            let mut args = Vec::new_in(self.arena);
            self.parse_command_args(lex, &mut args).map(|()| args)?
        };
        let redirection = self.parse_command_redirection(lex)?;
        let metadata = self.gen_metadata(lex.span().since(start));
        Ok(SimpleStatement::Command { name, args, redirection, metadata })
    }

    /// Parses arguments to command or function calls; consumes to the end of
    /// the argument list or short-circuits with `delimiter` if empty.
    fn parse_function_args(&mut self, lex: &mut Lexer<'a>) -> Result<Vec<'a, Expr<'a>>> {
        let mut arguments = Vec::new_in(self.arena);
        if lex.peek_is(&Token::ClosedParent) {
            return Ok(arguments);
        }

        arguments.push(self.parse_expression(lex, true)?);
        while lex.consume(&Token::Comma) {
            arguments.push(self.parse_expression(lex, true)?);
        }
        Ok(arguments)
    }

    fn parse_command_args(
        &mut self,
        lex: &mut Lexer<'a>,
        arguments: &mut Vec<'a, Expr<'a>>,
    ) -> Result<()> {
        let mut pratt = Pratt::new(self, false);
        if !lex.peek_with(Token::is_expr_start) {
            return Ok(());
        }

        arguments.push(pratt.parse_command_argument(lex)?);
        while lex.consume(&Token::Comma) {
            arguments.push(pratt.parse_command_argument(lex)?);
        }
        Ok(())
    }

    fn parse_command_redirection(
        &mut self,
        lex: &mut Lexer<'a>,
    ) -> Result<Option<(Redirection, Expr<'a>)>> {
        if let Some(Ok(token)) = lex.peek()
            && let Some(redirection) = Redirection::parse(token)
        {
            lex.next();
            Ok(Some((
                redirection,
                Pratt::new(self, false).parse_redirection(lex)?,
            )))
        } else {
            Ok(None)
        }
    }

    fn parse_delete(&mut self, lex: &mut Lexer<'a>) -> Result<SimpleStatement<'a>> {
        let start = lex.span().start;
        let mut pratt = Pratt::new(self, false);
        let place = pratt.parse_place(lex)?;
        let metadata = self.gen_metadata(lex.span().since(start));

        match place {
            Place::Variable(var) => Ok(SimpleStatement::Delete(var, metadata)),
            Place::Array(place) => Ok(SimpleStatement::DeleteElement(place, metadata)),
            Place::Record(_) => Err(ParsingError::OperatorExpectsVariable(
                lex.span().since(start),
            )),
        }
    }

    fn parse_function(&mut self, lex: &mut Lexer<'a>) -> Result<()> {
        let start = lex.span().start;
        let name = lex.expect_identifier()?.qualify(lex, self.namespace)?;
        let args = self.parse_signature(lex, name)?;
        let span = lex.span().since(start);
        lex.consume(&Token::Newline);
        let body = self.with_return_context(|this| this.parse_body(lex))?;

        let Ok(_) = self.ast.functions.try_insert(name, Function { args, body }) else {
            return Err(ParsingError::DuplicatedFunction(span, name.to_string()));
        };
        Ok(())
    }

    fn parse_signature(
        &mut self,
        lex: &mut Lexer<'a>,
        name: Identifier,
    ) -> Result<Vec<'a, Identifier>> {
        let mut args = Vec::new_in(self.arena);
        lex.expect(&Token::OpenParent, |s| {
            ParsingError::NoFunctionSignature(s, format!("{name:?}"))
        })?;

        if lex.consume(&Token::ClosedParent) {
            return Ok(args);
        }

        loop {
            let arg = lex.expect_identifier()?.qualify(lex, self.namespace)?;
            // Linear search is fine for the numbers we are working with.
            if args.contains(&arg) {
                return Err(ParsingError::DuplicatedArgument(
                    lex.span(),
                    format!("{arg:?}"),
                    format!("{name:?}"),
                ));
            }
            args.push(arg);

            if !lex.consume(&Token::Comma) {
                lex.expect(
                    &Token::ClosedParent,
                    ParsingError::FunctionCallMissingParenthesis,
                )?;
                break;
            }
        }
        Ok(args)
    }

    fn parse_expression(&mut self, lex: &mut Lexer<'a>, typed_regex: bool) -> Result<Expr<'a>> {
        Pratt::new(self, typed_regex).parse(lex)
    }

    fn add_rule(&mut self, rule: Rule<'a>) {
        if self.concurrent {
            self.concurrent = false;
            &mut self.ast.concurrent
        } else {
            &mut self.ast.rules
        }
        .push(rule);
    }

    /// Enable `break`/`continue` while parsing a loop body.
    ///
    /// Flags are only set when currently unset, and only cleared if this call
    /// set them — so nested loops keep the outer context intact.
    fn with_loop_context<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        let set_break = !self.break_allowed;
        let set_continue = !self.continue_allowed;
        if set_break {
            self.break_allowed = true;
        }
        if set_continue {
            self.continue_allowed = true;
        }
        let result = f(self);
        if set_break {
            self.break_allowed = false;
        }
        if set_continue {
            self.continue_allowed = false;
        }
        result
    }

    /// Enable `return` while parsing a function body.
    fn with_return_context<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        self.return_allowed = true;
        let result = f(self);
        self.return_allowed = false;
        result
    }

    fn parse_function_call<const SPACE_SENSITIVE: bool>(
        &mut self,
        lex: &mut Lexer<'a>,
        generate: impl FnOnce(Vec<'a, Expr<'a>>) -> ExprNode<'a>,
    ) -> Result<Expr<'a>> {
        let ident_span = lex.span();
        let is_juxtaposed = !SPACE_SENSITIVE || lex.is_juxtaposed();

        lex.expect(&Token::OpenParent, ParsingError::ExpectedOpeningParenthesis)?;
        let expr = generate(self.parse_function_args(lex)?);
        debug_assert!(!matches!(expr, ExprNode::FunctionCall(_, _)),);

        lex.expect(
            &Token::ClosedParent,
            ParsingError::FunctionCallMissingParenthesis,
        )?;

        let c_span = lex.span().since(ident_span.start);
        if is_juxtaposed {
            Ok(Expr::node(expr, self, c_span))
        } else {
            Err(ParsingError::FunctionCallSeparatedIdent(c_span))
        }
    }

    fn parse_user_call(&mut self, fun: Identifier, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        let ident_span = lex.span();
        let is_juxtaposed = lex.is_juxtaposed();

        lex.expect(&Token::OpenParent, ParsingError::ExpectedOpeningParenthesis)?;
        let mut args = self.parse_function_args(lex)?;
        lex.expect(
            &Token::ClosedParent,
            ParsingError::FunctionCallMissingParenthesis,
        )?;

        let c_span = lex.span().since(ident_span.start);
        match (is_juxtaposed, args.len()) {
            (true, _) => Ok(Expr::node(ExprNode::FunctionCall(fun, args), self, c_span)),
            (false, 1) if let Some(arg) = args.pop() => Ok(Expr::node(
                ExprNode::BinaryOperation(
                    BinaryOperator::Concat,
                    Expr::leaf(Atom::Variable(Variable::User(fun)), self, ident_span),
                    arg,
                ),
                self,
                c_span,
            )),
            (false, _) => Err(ParsingError::FunctionCallSeparatedIdent(c_span)),
        }
    }

    fn parse_atom(&self, lex: &mut Lexer<'a>, token: Token, typed_regex: bool) -> Result<Atom> {
        match token {
            Token::Number(n) => Ok(Atom::Number(n)),
            Token::Integer(n) => Ok(Atom::Integer(n)),
            Token::String(s) => Ok(Atom::String(s)),
            Token::Regex(r) => Ok(Atom::Regex(r)),
            Token::TypedRegex(r) if typed_regex => Ok(Atom::TypedRegex(r)),
            Token::TypedRegex(_) => Err(ParsingError::UnexpectedTypedRegex(lex.span())),
            token => match self.get_place(lex, token) {
                Ok(var) => Ok(Atom::Variable(var)),
                Err((e, _)) => Err(e),
            },
        }
    }

    fn get_place(
        &self,
        lex: &mut Lexer<'a>,
        token: Token,
    ) -> Result<Variable, (ParsingError, Token)> {
        match token {
            Token::Identifier(a) if !(lex.peek_is(&Token::OpenParent) && lex.is_juxtaposed()) => {
                match a.qualify(lex, self.namespace) {
                    Ok(ident) => Ok(ident.into()),
                    Err(e) => Err((e, token)),
                }
            }
            Token::NrVariable => Ok(Variable::Nr),
            Token::NfVariable => Ok(Variable::Nf),
            Token::FsVariable => Ok(Variable::Fs),
            Token::FpatVariable => Ok(Variable::Fpat),
            Token::FieldwidthsVariable => Ok(Variable::Fieldwidths),
            Token::RsVariable => Ok(Variable::Rs),
            Token::RtVariable => Ok(Variable::Rt),
            Token::OfsVariable => Ok(Variable::Ofs),
            Token::OrsVariable => Ok(Variable::Ors),
            Token::FilenameVariable => Ok(Variable::Filename),
            Token::ArgcVariable => Ok(Variable::Argc),
            Token::ArgvVariable => Ok(Variable::Argv),
            Token::SubsepVariable => Ok(Variable::Subsep),
            Token::FnrVariable => Ok(Variable::Fnr),
            Token::OfmtVariable => Ok(Variable::Ofmt),
            Token::RstartVariable => Ok(Variable::Rstart),
            Token::RlengthVariable => Ok(Variable::Rlength),
            Token::EnvironVariable => Ok(Variable::Environ),
            Token::ProcinfoVariable => Ok(Variable::Procinfo),
            Token::SymtabVariable => Ok(Variable::Symtab),
            Token::FunctabVariable => Ok(Variable::Functab),
            token => Err((
                ParsingError::UnexpectedToken(lex.span(), "is not valid data".to_string()),
                token,
            )),
        }
    }

    fn gen_metadata(&mut self, span: Span) -> MetaId {
        let span = AriadneSpan(self.file.clone(), span);
        self.ast.loc_metadata.store(span)
    }

    fn enter_file<T>(
        &mut self,
        file: FileCache,
        source: &'a [u8],
        f: impl FnOnce(&mut Self, &mut Lexer<'a>) -> T,
    ) -> (T, FileCache) {
        let old = replace(&mut self.file, file);
        self.ast.diagnostics.cache(self.file.clone(), source);
        let mut lex = Lexer::new(source);

        let ret = f(self, &mut lex);
        (ret, replace(&mut self.file, old))
    }

    /// `span` is the `@include` path's string, where a read error is reported.
    fn include(
        &mut self,
        path: Rc<Path>,
        span: Span,
        ns: Option<IdentId>,
    ) -> (Result<()>, FileCache) {
        match self.preprocessor.read_to_arena(&path, self.arena) {
            Ok(source) => self.include_source(FileCache::new_file(path), source, ns),
            Err(err) => {
                let reason = read_reason(&err).into_owned();
                let error =
                    ParsingError::CannotOpenSource(span, path.display().to_string(), reason);
                (Err(error), self.file.clone())
            }
        }
    }

    fn include_source(
        &mut self,
        file: FileCache,
        source: &'a [u8],
        ns: Option<IdentId>,
    ) -> (Result<()>, FileCache) {
        let old_namespace = self.namespace;

        if let Some(ns) = ns {
            self.namespace = ns;
        }

        let file = match self.enter_file(file, source, |this, lex| {
            this.parse_top(lex, false).map(|_| ())
        }) {
            (Ok(()), file) => file,
            e => return e,
        };
        self.namespace = old_namespace;

        (Ok(()), file)
    }

    /// Parses the `-f` files and `--source` texts in command-line order. Like gawk,
    /// each one is parsed on its own and starts in the `awk` namespace.
    pub fn include_many(
        &mut self,
        sources: &'a [ProgramSource],
    ) -> std::result::Result<&mut Ast<'a>, IncludeError> {
        let mut texts = 0;
        for source in sources {
            let (file, source) = match source {
                ProgramSource::File(path) => {
                    let path = Rc::from(path.as_path());
                    match self.preprocessor.read_to_arena(&path, self.arena) {
                        Ok(source) => (FileCache::new_file(path), source),
                        Err(err) => return Err(IncludeError::CannotOpen { path, err }),
                    }
                }
                ProgramSource::Text(text) => {
                    texts += 1;
                    (FileCache::Cli(texts - 1), text.as_encoded_bytes())
                }
            };
            let (res, file) = self.include_source(file, source, Some(StaticInterner::awk_ident()));

            if let Err(error) = res {
                error.add_diagnostic_cached(
                    &mut self.ast.diagnostics,
                    AriadneSpan(file, error.span().unwrap_or_default()),
                );
            }
        }

        if self.ast.diagnostics.is_unrecoverable() {
            Err(IncludeError::Invalid(take(&mut self.ast.diagnostics)))
        } else {
            Ok(&mut self.ast)
        }
    }
}

/// One piece of the program: a `-f` file, or `--source` text.
#[derive(Clone, Debug)]
pub enum ProgramSource {
    File(PathBuf),
    Text(OsString),
}

/// Why [`Parser::include_many`] has no program to run.
#[derive(Debug, thiserror::Error)]
pub enum IncludeError {
    #[error("{}", cannot_open_source(.path.display(), read_reason(.err)))]
    CannotOpen { path: Rc<Path>, err: io::Error },
    #[error("the program has errors")]
    Invalid(DiagnosticStore),
}

/// GNU's wording for the errors reading a source file can hit, which Rust words differently.
fn read_reason(err: &io::Error) -> Cow<'static, str> {
    match err.kind() {
        io::ErrorKind::NotFound => "No such file or directory".into(),
        io::ErrorKind::PermissionDenied => "Permission denied".into(),
        io::ErrorKind::IsADirectory => "Is a directory".into(),
        _ => err.to_string().into(),
    }
}

impl<'a> Ast<'a> {
    pub(crate) fn new(arena: &'a Bump) -> Self {
        Self {
            loads: Vec::new_in(arena),
            begin: Vec::new_in(arena),
            end: Vec::new_in(arena),
            begin_file: Vec::new_in(arena),
            end_file: Vec::new_in(arena),
            rules: Vec::new_in(arena),
            concurrent: Vec::new_in(arena),
            functions: HashMap::with_hasher_in(RandomState::new(), arena),
            ns_metadata: MetadataStore::new(),
            loc_metadata: MetadataStore::new(),
            diagnostics: DiagnosticStore::new(),
        }
    }
}

#[derive(Debug)]
struct Preprocessor {}

impl Preprocessor {
    fn read_to_arena<'a>(&self, path: impl AsRef<Path>, arena: &'a Bump) -> io::Result<&'a [u8]> {
        let mut f = File::open(path)?;
        let mut buf = Vec::with_capacity_in(f.metadata()?.len() as _, arena);
        io::copy(&mut f, &mut buf)?;
        Ok(buf.into_bump_slice())
    }
}

#[cfg(unix)]
fn make_path(blob: BlobId) -> Rc<Path> {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    StaticInterner::resolve(blob, |s| Rc::from(Path::new(OsStr::from_bytes(s))))
}

#[cfg(not(unix))]
fn make_path(blob: BlobId) -> Rc<Path> {
    StaticInterner::resolve(blob, |s| match String::from_utf8_lossy(s) {
        Cow::Borrowed(s) => Rc::from(Path::new(s)),
        Cow::Owned(s) => Rc::from(PathBuf::from(s)),
    })
}

trait IdentifierExt<'a> {
    fn qualify(self, lex: &mut Lexer<'a>, namespace: IdentId) -> Result<Identifier>
    where
        Self: 'a;
}

impl<'a> IdentifierExt<'a> for IdentId {
    fn qualify(self, lex: &mut Lexer<'a>, mut namespace: IdentId) -> Result<Identifier>
    where
        Self: 'a,
    {
        let no_space = lex.is_juxtaposed();
        let span = lex.span();

        if !lex.consume(&Token::PathSpec) {
            // No explicit namespace; use current namespace or default's.
            if StaticInterner::resolve_ident(self, |s| s.bytes().all(|c| c.is_ascii_uppercase())) {
                namespace = StaticInterner::awk_ident();
            }
            Ok(Identifier { namespace, literal: self })
        } else if !no_space {
            // Space between namespace and path specifier.
            let space_span = Some(Left(lex.span().up_from(span.end)));
            lex.consume_with(Token::is_ident_place);
            let ident_span = lex.span().since(span.start);
            Err(ParsingError::ExpectedIdentifier(ident_span, space_span))
        } else if lex.peek_with(Token::is_ident_place) && lex.is_juxtaposed() {
            let literal = if let Some(Ok(Token::Identifier(literal))) = lex.next() {
                literal
            } else {
                // SAFETY: the token is ensured to be UTF-8.
                StaticInterner::intern_ident(unsafe { lex.src_as_str() })
            };
            Ok(Identifier { namespace: self, literal })
        } else {
            // Try to select space between the path specifier and next one.
            let space_span = (lex.peek().is_none() || !lex.is_juxtaposed())
                .then(|| Right(lex.span_peeked_up_to(lex.span().end)));
            let err_span = lex.span_peeked_since(span.start);
            Err(ParsingError::ExpectedIdentifier(err_span, space_span))
        }
    }
}
