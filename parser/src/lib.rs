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

use std::mem::replace;

use ahash::RandomState;
use bumpalo::{Bump, boxed::Box, collections::Vec, vec};
use derive_more::Debug;
use either::Either::{Left, Right};
use hashbrown::HashMap;
use lexer::{Span, Token};

pub use crate::{
    ast::*,
    diagnostics::{AriadneSpan, Diagnostic, DiagnosticStore, FileCache, ParsingError},
    lex::Lexer,
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
    namespace: &'a str,
    concurrent: bool,
    // Disables file include materialization ands enables metadata recording.
    dry: bool,
    /// Whether `break` is allowed (inside a loop or `switch`).
    break_allowed: bool,
    /// Whether `continue` is allowed (inside a loop only).
    continue_allowed: bool,
    /// Whether `return` is allowed (inside a function body).
    return_allowed: bool,
}

impl<'a> Parser<'a> {
    #[tracing::instrument]
    pub fn new(arena: &'a Bump, dry: bool) -> Self {
        Self {
            ast: Ast::new(arena),
            arena,
            preprocessor: Preprocessor {},
            file: FileCache(None),
            namespace: "awk",
            concurrent: false,
            dry,
            break_allowed: false,
            continue_allowed: false,
            return_allowed: false,
        }
    }

    pub fn parse(
        &mut self,
        file: FileCache,
        source: &'a [u8],
    ) -> Result<&mut Ast<'a>, DiagnosticStore<'a>> {
        let source = self.arena.alloc_slice_copy(source);
        self.file.clone_from(&file);
        let mut lex = Lexer::new(source, self.arena);
        match &self.parse_top(&mut lex, true) {
            Ok(_) => Ok(&mut self.ast),
            Err(error) => {
                let mut store = DiagnosticStore::new();
                error.add_diagnostic(&mut store, file, source);
                Err(store)
            }
        }
    }

    #[tracing::instrument]
    fn parse_top(&mut self, lex: &mut Lexer<'a>, awk_namespace: bool) -> Result<&Ast<'a>> {
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
                        let body = self.parse_body(lex)?;
                        match special_pattern {
                            SpecialPattern::Begin => &mut self.ast.begin,
                            SpecialPattern::End => &mut self.ast.end,
                            SpecialPattern::BeginFile => &mut self.ast.begin_file,
                            SpecialPattern::EndFile => &mut self.ast.end_file,
                        }
                        .push(body);
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
                        lex.expect_with(Token::is_stmnt_end, "expected statement end.".into())?;
                    }
                    Token::IncludeDirective => {
                        let path = lex.expect_string()?;
                        let old_namespace = self.namespace;
                        let content = self.preprocessor.include_in(path.as_ref(), self.arena);
                        self.parse_top(&mut Lexer::new(content, self.arena), true)?;
                        lex.expect_with(Token::is_stmnt_end, "expected statement end.".into())?;
                        self.namespace = old_namespace;
                    }
                    Token::NsIncludeDirective => {
                        let path = lex.expect_string()?;
                        let old_namespace = self.namespace;
                        let content = self.preprocessor.include_in(path.as_ref(), self.arena);
                        self.parse_top(&mut Lexer::new(content, self.arena), false)?;
                        lex.expect_with(Token::is_stmnt_end, "expected statement end.".into())?;
                        self.namespace = old_namespace;
                    }
                    Token::NamespaceDirective => {
                        let namespace = lex.expect_string()?;
                        let namespace = lex.lex_ident(namespace.as_ref(), self.arena)?;
                        lex.expect_with(Token::is_stmnt_end, "expected statement end.".into())?;
                        if self.namespace == namespace {
                            continue; // skip counting.
                        }
                        self.namespace = namespace;
                        if self.dry {
                            self.ast.ns_metadata.store(namespace);
                        }
                    }
                    Token::ConcurrentDirective => {
                        if lex.peek_with(|t| t.maps_to_special_pat().is_some()) || self.concurrent {
                            return Err(ParsingError::UnexpectedToken(
                                lex.span(),
                                "especified more than once.".into(),
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
    #[tracing::instrument]
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
    #[tracing::instrument]
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
                Some(_) => {
                    let (statement, consumed) = self.parse_statement_with_trailing(lex)?;
                    body.push(statement);
                    after_separator = consumed;
                }
                None => {
                    break Err(ParsingError::UnclosedScope(start_span..lex.span().end));
                }
            }
        }
    }

    /// These are a subset of statements usable in places like for-loop defs.
    #[tracing::instrument]
    fn parse_simple_statement(
        &mut self,
        lex: &mut Lexer<'a>,
    ) -> Option<Result<SimpleStatement<'a>>> {
        let (peek, start) = lex.peek_with_span()?;
        let (peek, start) = (peek.ok()?, start.start);
        if peek.is_expr_start() {
            Some(
                self.parse_expression(lex, false).map(|e| {
                    SimpleStatement::Expression(e, self.gen_metadata(start..lex.span().end))
                }),
            )
        } else {
            match peek {
                token if let Some(name) = token.maps_to_command() => {
                    lex.next();
                    Some(self.parse_command(lex, name))
                }
                token if let Some(builtin) = token.maps_to_builtin() => {
                    lex.next();
                    Some(self.parse_builtin_call(lex, builtin))
                }
                Token::Delete => {
                    lex.next();
                    Some(self.parse_delete(lex))
                }
                _ => None,
            }
        }
    }

    #[tracing::instrument]
    fn parse_statement(&mut self, lex: &mut Lexer<'a>) -> Result<Statement<'a>> {
        self.parse_statement_with_trailing(lex)
            .map(|(statement, _)| statement)
    }

    #[tracing::instrument]
    fn parse_statement_with_trailing(
        &mut self,
        lex: &mut Lexer<'a>,
    ) -> Result<(Statement<'a>, bool)> {
        let statement = if let Some(statement) = self.parse_simple_statement(lex) {
            Statement::Simple(statement?)
        } else {
            let next = lex.expect_next()?;
            let start = lex.span().start;
            if lex.is_yuxtaposed() && lex.peek_is(&Token::PathSpec) {
                return Err(ParsingError::ExpectedIdentifier(lex.span(), None));
            }
            match next {
                Token::If => {
                    let condition = self.parse_parenthesized_expr(lex)?;
                    let then_body = self.parse_statement_body(lex)?;
                    let else_body = lex
                        .consume(&Token::Else)
                        .then(|| self.parse_statement_body(lex))
                        .transpose()?;
                    let metadata = self.gen_metadata(start..lex.span().end);
                    Statement::If { condition, then_body, else_body, metadata }
                }
                Token::For => {
                    lex.expect(&Token::OpenParent, ParsingError::ExpectedOpeningParenthesis)?;

                    if lex.peek_is(&Token::Semicolon) {
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
                    }
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
                    let metadata = self.gen_metadata(start..lex.span().end);

                    Statement::Switch { scrutinee, branches, default, metadata }
                }
                Token::While => {
                    let condition = self.parse_parenthesized_expr(lex)?;
                    let then_body =
                        self.with_loop_context(|this| this.parse_statement_body(lex))?;
                    let metadata = self.gen_metadata(start..lex.span().end);
                    Statement::While { condition, then_body, metadata }
                }
                Token::Do => {
                    let then_body = self.with_loop_context(|this| this.parse_body(lex))?;
                    lex.expect(&Token::While, ParsingError::MissingWhileAfterDo)?;
                    let condition = self.parse_parenthesized_expr(lex)?;
                    let metadata = self.gen_metadata(start..lex.span().end);
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
                        self.gen_metadata(start..lex.span().end),
                    )
                }
                Token::Next => Statement::Next(self.gen_metadata(lex.span())),
                Token::NextFile => Statement::NextFile(self.gen_metadata(lex.span())),
                Token::Exit => Statement::Exit(
                    (!lex.peek_with(Token::is_stmnt_or_block_end))
                        .then(|| self.parse_expression(lex, false))
                        .transpose()?,
                    self.gen_metadata(start..lex.span().end),
                ),
                _ => {
                    return Err(ParsingError::UnexpectedToken(
                        lex.span(),
                        "invalid statement start.".into(),
                    ));
                }
            }
        };

        let consumed = lex.consume_with(Token::is_stmnt_end);
        Ok((statement, consumed))
    }

    #[tracing::instrument]
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

    #[tracing::instrument]
    fn parse_for_loop(
        &mut self,
        lex: &mut Lexer<'a>,
        init: Option<SimpleStatement<'a>>,
        start: usize,
    ) -> Result<Statement<'a>> {
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
        let body = self.with_loop_context(|this| this.parse_statement_body(lex))?;
        let metadata = self.gen_metadata(start..lex.span().end);
        Ok(Statement::For { init, condition, update, body, metadata })
    }

    #[tracing::instrument]
    fn parse_for_ambiguous(
        &mut self,
        lex: &mut Lexer<'a>,
        expr: Option<SimpleStatement<'a>>,
        start: usize,
    ) -> Result<Statement<'a>> {
        let (array, variable) = match expr {
            Some(SimpleStatement::Expression(Expr::Node(node, _), _))
                if matches!(&*node, ExprNode::ArrayOperation(ArrayOperator::In, _, _)) =>
            {
                if let ExprNode::ArrayOperation(ArrayOperator::In, array, mut args) =
                    Box::into_inner(node)
                    && let [Expr::Leaf(Atom::Variable(var), _)] = &mut *args
                {
                    (array, replace(var, Variable::Nr))
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

        let body = self.with_loop_context(|this| this.parse_statement_body(lex))?;
        let metadata = self.gen_metadata(start..lex.span().end);
        Ok(Statement::ForEach { variable, array, body, metadata })
    }

    #[tracing::instrument]
    fn parse_statement_body(&mut self, lex: &mut Lexer<'a>) -> Result<Body<'a>> {
        // Ignore newlines in this position.
        lex.consume(&Token::Newline);

        // Braced body, with >=1 statements.
        if lex.peek_is(&Token::OpenBrace) {
            return self.parse_body(lex);
        }

        // Empty body check. Accepts cases like `if (...);`.
        if lex.consume(&Token::Semicolon) {
            return Ok(Vec::new_in(self.arena).into());
        }

        // Parses a single statement.
        let start = lex.peeked_span().map_or(lex.span().start, |s| s.start);
        let (statement, terminator) = self.parse_statement_with_trailing(lex)?;

        if terminator || lex.peek_is(&Token::ClosedBrace) {
            Ok(vec![in self.arena; statement].into())
        } else {
            Err(ParsingError::ExpectedStatementEnd(start..lex.span().end))
        }
    }

    #[tracing::instrument]
    fn parse_case(&mut self, lex: &mut Lexer<'a>) -> Result<Atom<'a>> {
        lex.expect(&Token::Case, ParsingError::MissingSwitchBranch)?;
        let next = lex.expect_next()?;
        let value = self.parse_atom(lex, next, true)?;
        lex.expect(&Token::Colon, ParsingError::ColonMustFollowCase)?;
        match value {
            Atom::Variable(_) => Err(ParsingError::InvalidCaseValue(lex.span())),
            _ => Ok(value),
        }
    }

    #[tracing::instrument]
    fn parse_command(&mut self, lex: &mut Lexer<'a>, name: Command) -> Result<SimpleStatement<'a>> {
        let start = lex.span().start;
        let parent = lex.consume(&Token::OpenParent);
        let args = if parent {
            let mut args = self.parse_function_args(lex)?;
            lex.expect(
                &Token::ClosedParent,
                ParsingError::UnclosedParenthesisInStatement,
            )?;
            if lex.consume(&Token::Comma) {
                let is_err = args.len() > 1;
                // We parse anyway to advance the lexer.
                let start_extra = lex.span().start;
                self.parse_command_args(lex, &mut args)?;
                if is_err {
                    return Err(ParsingError::CommandDoubleCall(
                        start..lex.span().end,
                        start_extra..lex.span().end,
                    ));
                }
            }
            args
        } else {
            let mut args = Vec::new_in(self.arena);
            self.parse_command_args(lex, &mut args).map(|_| args)?
        };
        let redirection = self.parse_command_redirection(lex)?;
        let metadata = self.gen_metadata(start..lex.span().end);
        Ok(SimpleStatement::Command { name, args, redirection, metadata })
    }

    #[tracing::instrument]
    fn parse_builtin_call(
        &mut self,
        lex: &mut Lexer<'a>,
        builtin: BuiltinFunction,
    ) -> Result<SimpleStatement<'a>> {
        let start = lex.span().start;
        let expr =
            self.parse_function_call(lex, |args| ExprNode::BuiltinCall(builtin, args), lex.span())?;
        let metadata = self.gen_metadata(start..lex.span().end);
        Ok(SimpleStatement::Expression(expr, metadata))
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
        let next = lex.expect_next()?;
        let Ok(var) = self.get_place(lex, next) else {
            return Err(ParsingError::OperatorExpectsVariable(lex.span()));
        };
        let index = if lex.consume(&Token::OpenBracket) {
            let mut pratt = Pratt::new(self, false);
            let expr = pratt.parse(lex)?;
            let expr = pratt.parse_comma_expr(lex, expr)?;
            lex.expect(&Token::ClosedBracket, ParsingError::UnclosedArrayAccess)?;
            Some(expr)
        } else {
            None
        };
        let metadata = self.gen_metadata(start..lex.span().end);
        Ok(SimpleStatement::Delete(var, index, metadata))
    }

    #[tracing::instrument]
    fn parse_function(&mut self, lex: &mut Lexer<'a>) -> Result<()> {
        let name = lex.expect_identifier()?.qualify(lex, self.namespace)?;
        let args = self.parse_signature(lex, &name)?;
        lex.consume(&Token::Newline);
        let body = self.with_return_context(|this| this.parse_body(lex))?;

        self.ast.functions.insert(name, Function { args, body });
        Ok(())
    }

    #[tracing::instrument]
    fn parse_signature(
        &mut self,
        lex: &mut Lexer<'a>,
        name: &Identifier<'a>,
    ) -> Result<Vec<'a, Identifier<'a>>> {
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

    #[tracing::instrument]
    fn parse_expression(&mut self, lex: &mut Lexer<'a>, typed_regex: bool) -> Result<Expr<'a>> {
        Pratt::new(self, typed_regex).parse(lex)
    }

    #[tracing::instrument]
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

    fn parse_function_call(
        &mut self,
        lex: &mut Lexer<'a>,
        generate: impl FnOnce(Vec<'a, Expr<'a>>) -> ExprNode<'a>,
        span: Span,
    ) -> Result<Expr<'a>> {
        let start = lex.span().start;
        lex.expect(&Token::OpenParent, ParsingError::ExpectedOpeningParenthesis)?;
        if lex.span().start != span.end {
            return Err(ParsingError::FunctionCallSeparatedIdent(span));
        }
        let expr = generate(self.parse_function_args(lex)?);
        lex.expect(
            &Token::ClosedParent,
            ParsingError::FunctionCallMissingParenthesis,
        )?;
        Ok(Expr::node(expr, self, start..lex.span().end))
    }

    #[tracing::instrument]
    fn parse_atom(
        &self,
        lex: &mut Lexer<'a>,
        token: Token<'a>,
        typed_regex: bool,
    ) -> Result<Atom<'a>> {
        match token {
            Token::Number(n) => Ok(Atom::Number(n)),
            Token::Integer(n) => Ok(Atom::Integer(n)),
            Token::String(s) => Ok(Atom::String(s)),
            Token::Regex(r) => Ok(Atom::Regex(r)),
            Token::TypedRegex(r) if typed_regex => Ok(Atom::TypedRegex(r)),
            Token::TypedRegex(_) => Err(ParsingError::UnexpectedTypedRegex(lex.span())),
            token => match self.get_place(lex, token) {
                Ok(_) if lex.peek_is(&Token::PathSpec) => {
                    // SAFETY: tokens matching get_place() are UTF-8.
                    lexer::Identifier { literal: unsafe { lex.src_as_str() } }
                        .qualify(lex, self.namespace)
                        .map(Variable::User)
                        .map(Atom::Variable)
                }
                Ok(var) => Ok(Atom::Variable(var)),
                Err((e, _)) => Err(e),
            },
        }
    }

    #[tracing::instrument]
    fn get_place(
        &self,
        lex: &mut Lexer<'a>,
        token: Token<'a>,
    ) -> Result<Variable<'a>, (ParsingError, Token<'a>)> {
        match token {
            Token::Identifier(a) if !(lex.peek_is(&Token::OpenParent) && lex.is_yuxtaposed()) => {
                match a.qualify(lex, self.namespace) {
                    Ok(ident) => Ok(ident.into()),
                    Err(e) => Err((e, token)),
                }
            }
            Token::NrVariable => Ok(Variable::Nr),
            Token::NfVariable => Ok(Variable::Nf),
            Token::FsVariable => Ok(Variable::Fs),
            Token::RsVariable => Ok(Variable::Rs),
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
            token => Err((
                ParsingError::UnexpectedToken(lex.span(), "is not valid data".to_string()),
                token,
            )),
        }
    }

    fn gen_metadata(&mut self, span: Span) -> MetaId {
        self.ast.loc_metadata.store((span, self.file.clone()))
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
    fn include_in<'a: 'b, 'b>(&mut self, _path: &'b [u8], _alloc: &'a Bump) -> &'a [u8] {
        todo!()
    }
}

trait IdentifierExt<'a> {
    fn qualify(self, lex: &mut Lexer<'a>, namespace: &'a str) -> Result<Identifier<'a>>
    where
        Self: 'a;
}

impl<'a> IdentifierExt<'a> for lexer::Identifier<'_> {
    fn qualify(self, lex: &mut Lexer<'a>, mut namespace: &'a str) -> Result<Identifier<'a>>
    where
        Self: 'a,
    {
        let no_space = lex.is_yuxtaposed();
        let span = lex.span();

        if !lex.consume(&Token::PathSpec) {
            // No explicit namespace; use current namespace or global's.
            if self.literal.bytes().all(|c| c.is_ascii_uppercase()) {
                namespace = "awk";
            }
            Ok(Identifier { namespace, literal: self.literal })
        } else if !no_space {
            // Space between namespace and path specifier.
            let space_span = Some(Left(span.end..lex.span().start));
            lex.consume_with(Token::is_ident_place);
            let ident_span = span.start..lex.span().end;
            Err(ParsingError::ExpectedIdentifier(ident_span, space_span))
        } else if lex.peek_with(Token::is_ident_place) && lex.is_yuxtaposed() {
            lex.next();
            // SAFETY: the token is ensured to be UTF-8.
            let literal = unsafe { lex.src_as_str() };
            Ok(Identifier { namespace: self.literal, literal })
        } else {
            // Try to select space between the path specifier and next one.
            let space_span = (lex.peek().is_none() || !lex.is_yuxtaposed())
                .then(|| lex.span().end..lex.peeked_span().map_or(lex.span().end, |s| s.start))
                .map(Right);
            let err_span = span.start..lex.peeked_span().unwrap_or(lex.span()).end;
            Err(ParsingError::ExpectedIdentifier(err_span, space_span))
        }
    }
}
