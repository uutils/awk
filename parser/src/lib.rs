// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#![forbid(unsafe_code)]

mod ast;
mod diagnostics;
mod idempotency;
mod lex;
mod pratt;
mod sexpr;
#[cfg(test)]
mod tests;

use std::{fmt::Debug, mem::replace};

use ahash::RandomState;
use bumpalo::{Bump, boxed::Box, collections::Vec, vec};
use either::Either::{Left, Right};
use hashbrown::HashMap;
use lexer::{LexingError, Span, Token};

pub use crate::{ast::*, lex::Lexer};
use crate::{
    diagnostics::{ParsingError, report_error},
    lex::TokenExt,
    pratt::Pratt,
};

type Result<T, E = ParsingError> = std::result::Result<T, E>;

pub struct Parser<'a> {
    ast: Ast<'a>,
    arena: &'a Bump,
    preprocessor: Preprocessor,
    current_file: &'a str,
    namespace: &'a str,
    concurrent: bool,
}

impl From<LexingError> for ParsingError {
    fn from(value: LexingError) -> Self {
        Self::LexingError(value)
    }
}

type AriadneErr<'a> = (
    std::boxed::Box<ariadne::Report<'a, (&'a str, Span)>>,
    ariadne::Source<&'a str>,
);

impl<'a> Parser<'a> {
    #[tracing::instrument]
    pub fn new(arena: &'a Bump) -> Self {
        Self {
            ast: Ast::new(arena),
            arena,
            preprocessor: Preprocessor {},
            current_file: "",
            namespace: "awk",
            concurrent: false,
        }
    }

    pub fn parse(&mut self, name: &'a str, source: &'a [u8]) -> Result<&Ast<'a>, AriadneErr<'a>> {
        let source = self.arena.alloc_slice_copy(source);
        self.current_file = name;
        let mut lex = Lexer::new(source, self.arena);
        let parsed = self.parse_top(&mut lex, true);
        parsed.map_err(|error| report_error(error, name, source))
    }

    #[tracing::instrument]
    fn parse_top(&mut self, lex: &mut Lexer<'a>, awk_namespace: bool) -> Result<&Ast<'a>> {
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
                        self.namespace = lex.lex_ident(namespace.as_ref(), self.arena)?;
                        lex.expect_with(Token::is_stmnt_end, "expected statement end.".into())?;
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
                    Token::Newline | Token::Semicolon => {}
                    _ => {
                        return Err(ParsingError::UnexpectedToken(
                            lex.span(),
                            "invalid rule beginning.".into(),
                        ));
                    }
                }
            }
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

        loop {
            if lex.peek_with(|tok| tok.is_stmnt_end() || tok.is_brace()) {
                match lex.expect_next()? {
                    Token::ClosedBrace => {
                        if depth == 0 {
                            break Ok(Body(body));
                        }
                        depth -= 1;
                    }
                    Token::OpenBrace => {
                        depth += 1;
                    }
                    _ => {}
                }
            } else if lex.peek().is_some() {
                body.push(self.parse_statement(lex)?);
            } else {
                break Err(ParsingError::UnclosedScope(
                    lex.peeked_span().unwrap_or_else(|_| lex.span()),
                ));
            }
        }
    }

    /// These are a subset of statements usable in places like for-loop defs.
    #[tracing::instrument]
    fn parse_simple_statement(
        &mut self,
        lex: &mut Lexer<'a>,
    ) -> Option<Result<SimpleStatement<'a>>> {
        let peek = lex.expect_peek().ok()?;
        if peek.is_expr_start() {
            Some(
                self.parse_expression(lex, false)
                    .map(SimpleStatement::Expression),
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
        let statement = if let Some(statement) = self.parse_simple_statement(lex) {
            Statement::Simple(statement?)
        } else {
            match lex.expect_next()? {
                Token::If => {
                    let condition = self.parse_parenthesized_expr(lex)?;
                    let then_body = self.parse_statement_body(lex)?;
                    let else_body = lex
                        .consume(&Token::Else)
                        .then(|| self.parse_statement_body(lex))
                        .transpose()?;
                    Statement::If { condition, then_body, else_body }
                }
                Token::For => {
                    lex.expect(&Token::OpenParent, ParsingError::ExpectedOpeningParenthesis)?;

                    if lex.peek_is(&Token::Semicolon) {
                        self.parse_for_loop(lex, None)?
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
                            self.parse_for_loop(lex, Some(stmnt))?
                        } else {
                            self.parse_for_ambiguous(lex, Some(stmnt))?
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
                    match case.take() {
                        Some(Right(())) => default = Some((body.into(), branches.len())),
                        Some(Left(atom)) => branches.push((atom, body.into())),
                        _ => {}
                    }

                    Statement::Switch { scrutinee, branches, default }
                }
                Token::While => {
                    let condition = self.parse_parenthesized_expr(lex)?;
                    let then_body = self.parse_statement_body(lex)?;
                    Statement::While { condition, then_body }
                }
                Token::Do => {
                    let then_body = self.parse_body(lex)?;
                    lex.expect(&Token::While, ParsingError::MissingWhileAfterDo)?;
                    let condition = self.parse_parenthesized_expr(lex)?;
                    Statement::DoWhile { then_body, condition }
                }
                Token::Break => Statement::Break,
                Token::Continue => Statement::Continue,
                Token::Return => Statement::Return(
                    (!lex.peek_with(Token::is_stmnt_or_block_end))
                        .then(|| self.parse_expression(lex, true))
                        .transpose()?,
                ),
                Token::Next => Statement::Next,
                Token::NextFile => Statement::NextFile,
                Token::Exit => Statement::Exit(
                    (!lex.peek_with(Token::is_stmnt_or_block_end))
                        .then(|| self.parse_expression(lex, false))
                        .transpose()?,
                ),
                _ => {
                    return Err(ParsingError::UnexpectedToken(
                        lex.span(),
                        "invalid statement start.".into(),
                    ));
                }
            }
        };

        lex.consume_with(Token::is_stmnt_end);
        Ok(statement)
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
        let body = self.parse_statement_body(lex)?;
        Ok(Statement::For { init, condition, update, body })
    }

    #[tracing::instrument]
    fn parse_for_ambiguous(
        &mut self,
        lex: &mut Lexer<'a>,
        expr: Option<SimpleStatement<'a>>,
    ) -> Result<Statement<'a>> {
        let (array, variable) = match expr {
            Some(SimpleStatement::Expression(Expr::Node(node)))
                if matches!(&*node, ExprNode::ArrayOperation(ArrayOperator::In, _, _)) =>
            {
                if let ExprNode::ArrayOperation(ArrayOperator::In, array, mut args) =
                    Box::into_inner(node)
                    && let [Expr::Leaf(Atom::Variable(var))] = &mut *args
                {
                    (array, replace(var, Variable::Nr))
                } else {
                    return Err(ParsingError::InvalidForLoop(lex.span()));
                }
            }
            expr => return self.parse_for_loop(lex, expr),
        };

        lex.expect(
            &Token::ClosedParent,
            ParsingError::UnclosedParenthesisInStatement,
        )?;

        let body = self.parse_statement_body(lex)?;
        Ok(Statement::ForEach { variable, array, body })
    }

    #[tracing::instrument]
    fn parse_statement_body(&mut self, lex: &mut Lexer<'a>) -> Result<Body<'a>> {
        if lex.peek_is(&Token::OpenBrace) {
            self.parse_body(lex)
        } else {
            Ok(vec![in self.arena; self.parse_statement(lex)?].into())
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
        let parent = lex.consume(&Token::OpenParent);
        let args = if parent {
            let expr = self.parse_function_args(lex)?;
            lex.expect(
                &Token::ClosedParent,
                ParsingError::UnclosedParenthesisInStatement,
            )?;
            expr
        } else {
            self.parse_command_args(lex)?
        };
        let redirection = self.parse_command_redirection(lex)?;
        Ok(SimpleStatement::Command { name, args, redirection })
    }

    #[tracing::instrument]
    fn parse_builtin_call(
        &mut self,
        lex: &mut Lexer<'a>,
        builtin: BuiltinFunction,
    ) -> Result<SimpleStatement<'a>> {
        let expr =
            self.parse_function_call(lex, |args| ExprNode::BuiltinCall(builtin, args), lex.span())?;
        Ok(SimpleStatement::Expression(expr))
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

    fn parse_command_args(&mut self, lex: &mut Lexer<'a>) -> Result<Vec<'a, Expr<'a>>> {
        let mut arguments = Vec::new_in(self.arena);
        let mut pratt = Pratt::new(self, false);
        if !lex.peek_with(Token::is_expr_start) {
            return Ok(arguments);
        }

        arguments.push(pratt.parse_command_argument(lex)?);
        while lex.consume(&Token::Comma) {
            arguments.push(pratt.parse_command_argument(lex)?);
        }
        Ok(arguments)
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
        Ok(SimpleStatement::Delete(var, index))
    }

    #[tracing::instrument]
    fn parse_function(&mut self, lex: &mut Lexer<'a>) -> Result<()> {
        let name = lex.expect_identifier()?.qualify(self.namespace);
        let args = self.parse_signature(lex, &name)?;
        lex.consume(&Token::Newline);
        let body = self.parse_body(lex)?;

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
            ParsingError::NoFunctionSignature(s, name.to_string())
        })?;

        if lex.consume(&Token::ClosedParent) {
            return Ok(args);
        }

        loop {
            let name = lex.expect_identifier()?.qualify(self.namespace);
            // Linear search is fine for the numbers we are working with.
            if let Some(arg) = args.iter().find(|&a| a == &name) {
                return Err(ParsingError::DuplicatedArgument(
                    lex.span(),
                    name.to_string(),
                    arg.to_string(),
                ));
            }
            args.push(name);

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

    fn parse_function_call(
        &mut self,
        lex: &mut Lexer<'a>,
        generate: impl FnOnce(Vec<'a, Expr<'a>>) -> ExprNode<'a>,
        span: Span,
    ) -> Result<Expr<'a>> {
        lex.expect(&Token::OpenParent, ParsingError::ExpectedOpeningParenthesis)?;
        if lex.span().start != span.end {
            return Err(ParsingError::FunctionCallSeparatedIdent(span));
        }
        let expr = generate(self.parse_function_args(lex)?);
        lex.expect(
            &Token::ClosedParent,
            ParsingError::FunctionCallMissingParenthesis,
        )?;
        Ok(Expr::node(expr, self.arena))
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
                Ok(var) => Ok(Atom::Variable(var)),
                Err(_) => Err(ParsingError::UnexpectedToken(
                    lex.span(),
                    "is not valid data.".into(),
                )),
            },
        }
    }

    #[tracing::instrument]
    fn get_place(&self, lex: &mut Lexer<'a>, token: Token<'a>) -> Result<Variable<'a>, Token<'a>> {
        match token {
            Token::Identifier(a) if !(lex.peek_is(&Token::OpenParent) && lex.is_yuxtaposed()) => {
                Ok(a.qualify(self.namespace).into())
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
            tok => Err(tok),
        }
    }
}

impl<'a> Ast<'a> {
    fn new(arena: &'a Bump) -> Self {
        Self {
            loads: Vec::new_in(arena),
            begin: Vec::new_in(arena),
            end: Vec::new_in(arena),
            begin_file: Vec::new_in(arena),
            end_file: Vec::new_in(arena),
            rules: Vec::new_in(arena),
            concurrent: Vec::new_in(arena),
            functions: HashMap::with_hasher_in(RandomState::new(), arena),
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
    fn qualify(self, namespace: &'a str) -> Identifier<'a>
    where
        Self: 'a;
}

impl<'a> IdentifierExt<'a> for lexer::Identifier<'_> {
    fn qualify(self, namespace: &'a str) -> Identifier<'a>
    where
        Self: 'a,
    {
        let literal = self.literal;
        if let Some(namespace) = self.namespace {
            Identifier { namespace, literal }
        } else {
            Identifier { namespace, literal }
        }
    }
}

impl Debug for Parser<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Parser<'a> {
            ast: &'a Ast<'a>,
            preprocessor: &'a Preprocessor,
            namespace: &'a str,
            concurrent: bool,
        }
        Parser {
            ast: &self.ast,
            preprocessor: &self.preprocessor,
            namespace: self.namespace,
            concurrent: self.concurrent,
        }
        .fmt(f)
    }
}
