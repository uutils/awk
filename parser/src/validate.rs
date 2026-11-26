// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that were distributed with this source code.

//! Post-parse validation keeping the function name namespace disjoint from the
//! variable namespace, as required by POSIX awk and GNU awk.
//!
//! A function name may not be used as a variable or an array. Functions may be
//! defined after their use, so this check runs over the whole AST instead of
//! while parsing a single statement. (A function parameter reusing a function
//! name is rejected at parse time, in `Parser::parse_signature`, where the
//! parameter's span is still available.)

use crate::{
    AriadneSpan, ArrayPlace, Ast, Atom, Body, Expr, ExprNode, Getline, Identifier, MetaId,
    ParsingError, Place, RulePattern, SimpleStatement, Statement, Variable,
};

struct Validator<'a> {
    ast: &'a Ast<'a>,
}

impl<'a> Validator<'a> {
    fn span_of(&self, meta: MetaId) -> lexer::Span {
        self.ast
            .loc_metadata
            .get(meta)
            .map(|(_, AriadneSpan(_, span))| *span)
            .unwrap_or_default()
    }

    fn is_function(&self, ident: Identifier) -> bool {
        self.ast.functions.contains_key(&ident)
    }

    fn check_variable(&self, var: Variable, meta: MetaId) -> Result<(), ParsingError> {
        match var {
            Variable::User(ident) if self.is_function(ident) => Err(
                ParsingError::FunctionNameAsVariable(self.span_of(meta), format!("{ident:?}")),
            ),
            _ => Ok(()),
        }
    }

    fn check_array(&self, array: &ArrayPlace<'a>, meta: MetaId) -> Result<(), ParsingError> {
        self.check_variable(array.0, meta)
    }

    fn check_place(&self, place: &Place<'a>, meta: MetaId) -> Result<(), ParsingError> {
        match place {
            Place::Variable(var) => self.check_variable(*var, meta),
            Place::Array(array) => self.check_array(array, meta),
            Place::Record(expr) => self.check_expr(expr),
        }
    }

    fn check_expr(&self, expr: &Expr<'a>) -> Result<(), ParsingError> {
        match expr {
            Expr::Leaf(atom, meta) => {
                if let Atom::Variable(var) = atom {
                    self.check_variable(*var, *meta)?;
                }
                Ok(())
            }
            Expr::Node(node, meta) => match node.as_ref() {
                ExprNode::Parenthesized(inner) => self.check_expr(inner),
                ExprNode::FunctionCall(_, args)
                | ExprNode::IndirectCall(_, args)
                | ExprNode::BuiltinCall(_, args) => {
                    for arg in args {
                        self.check_expr(arg)?;
                    }
                    Ok(())
                }
                ExprNode::PlacingCall(_, args, place) => {
                    for arg in args {
                        self.check_expr(arg)?;
                    }
                    self.check_place(place, *meta)
                }
                ExprNode::UnaryOperation(_, a) => self.check_expr(a),
                ExprNode::BinaryOperation(_, a, b) => {
                    self.check_expr(a)?;
                    self.check_expr(b)
                }
                ExprNode::UnaryPlaceOperation(_, place) => self.check_place(place, *meta),
                ExprNode::BinaryPlaceOperation(_, place, expr) => {
                    self.check_place(place, *meta)?;
                    self.check_expr(expr)
                }
                ExprNode::ArrayIndex(array) => self.check_array(array, *meta),
                ExprNode::InArray(array, tests) => {
                    self.check_array(array, *meta)?;
                    for test in tests {
                        self.check_expr(test)?;
                    }
                    Ok(())
                }
                ExprNode::Ternary(a, b, c) => {
                    self.check_expr(a)?;
                    self.check_expr(b)?;
                    self.check_expr(c)
                }
                ExprNode::Getline(getline) => self.check_getline(getline, *meta),
            },
        }
    }

    fn check_getline(&self, getline: &Getline<'a>, meta: MetaId) -> Result<(), ParsingError> {
        let (dest, expr) = match getline {
            Getline::FromInput(dest) => (dest.as_ref(), None),
            Getline::FromFile(dest, expr)
            | Getline::PipeOut(dest, expr)
            | Getline::CoprocessOut(dest, expr) => (dest.as_ref(), Some(expr)),
        };
        if let Some(dest) = dest {
            self.check_place(dest, meta)?;
        }
        if let Some(expr) = expr {
            self.check_expr(expr)?;
        }
        Ok(())
    }

    fn check_simple(&self, stmt: &SimpleStatement<'a>) -> Result<(), ParsingError> {
        match stmt {
            SimpleStatement::Expression(expr, _) => self.check_expr(expr),
            SimpleStatement::Command { args, redirection, .. } => {
                for arg in args {
                    self.check_expr(arg)?;
                }
                if let Some((_, expr)) = redirection {
                    self.check_expr(expr)?;
                }
                Ok(())
            }
            SimpleStatement::Delete(var, meta) => self.check_variable(*var, *meta),
            SimpleStatement::DeleteElement(array, meta) => self.check_array(array, *meta),
        }
    }

    fn check_body(&self, body: &Body<'a>) -> Result<(), ParsingError> {
        for stmt in &body.0 {
            self.check_statement(stmt)?;
        }
        Ok(())
    }

    fn check_statement(&self, stmt: &Statement<'a>) -> Result<(), ParsingError> {
        match stmt {
            Statement::Simple(simple) => self.check_simple(simple),
            Statement::If { condition, then_body, else_body, .. } => {
                self.check_expr(condition)?;
                self.check_body(then_body)?;
                if let Some(else_body) = else_body {
                    self.check_body(else_body)?;
                }
                Ok(())
            }
            Statement::While { condition, then_body, .. }
            | Statement::DoWhile { then_body, condition, .. } => {
                self.check_expr(condition)?;
                self.check_body(then_body)
            }
            Statement::For { init, condition, update, body, .. } => {
                if let Some(init) = init {
                    self.check_simple(init)?;
                }
                if let Some(condition) = condition {
                    self.check_expr(condition)?;
                }
                if let Some(update) = update {
                    self.check_simple(update)?;
                }
                self.check_body(body)
            }
            Statement::ForEach { variable, array, body, .. } => {
                // These carry no dedicated metadata; a zero span is acceptable
                // because this form cannot legally name a function anyway.
                self.check_variable(*variable, MetaId::default())?;
                self.check_array(array, MetaId::default())?;
                self.check_body(body)
            }
            Statement::Switch { scrutinee, branches, default, .. } => {
                self.check_expr(scrutinee)?;
                for (_, body) in branches {
                    self.check_body(body)?;
                }
                if let Some((body, _)) = default {
                    self.check_body(body)?;
                }
                Ok(())
            }
            Statement::Break(_)
            | Statement::Continue(_)
            | Statement::Next(_)
            | Statement::NextFile(_) => Ok(()),
            Statement::Return(expr, _) | Statement::Exit(expr, _) => match expr {
                Some(expr) => self.check_expr(expr),
                None => Ok(()),
            },
        }
    }

    fn check_rule_pattern(&self, pattern: &RulePattern<'a>) -> Result<(), ParsingError> {
        match pattern {
            RulePattern::Expression(expr) => self.check_expr(expr),
            RulePattern::Range(a, b) => {
                self.check_expr(a)?;
                self.check_expr(b)
            }
        }
    }
}

/// Validate that no function name is used as a variable or array anywhere in
/// the parsed program. Returns the first violation found.
pub fn validate(ast: &Ast<'_>) -> Result<(), ParsingError> {
    let validator = Validator { ast };

    // A parameter that reuses a function name is an error in the same shared
    // namespace. Function names may be defined after the function in question,
    // so this also runs as a whole-AST pass.
    let fallback = ast
        .loc_metadata
        .iter()
        .next()
        .map_or_else(Default::default, |(_, AriadneSpan(_, span))| *span);
    for (name, function) in &ast.functions {
        for arg in &function.args {
            if validator.is_function(*arg) {
                return Err(ParsingError::FunctionNameAsParameter(
                    fallback,
                    format!("{arg:?}"),
                    format!("{name:?}"),
                ));
            }
        }
    }

    for body in &ast.begin {
        validator.check_body(body)?;
    }
    for body in &ast.end {
        validator.check_body(body)?;
    }
    for body in &ast.begin_file {
        validator.check_body(body)?;
    }
    for body in &ast.end_file {
        validator.check_body(body)?;
    }
    for rule in ast.rules.iter().chain(ast.concurrent.iter()) {
        if let Some(pattern) = &rule.pattern {
            validator.check_rule_pattern(pattern)?;
        }
        if let Some(actions) = &rule.actions {
            validator.check_body(actions)?;
        }
    }
    for function in ast.functions.values() {
        validator.check_body(&function.body)?;
    }
    Ok(())
}
