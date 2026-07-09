// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::vec::Vec;

use bumpalo::{Bump, collections::CollectIn};
use proptest::prelude::*;

use crate::{
    Ast, Body, Place, Rule,
    ast::{
        Atom, BinaryOperator, BinaryPlaceOperator, Command, Expr, Identifier, RulePattern,
        SimpleStatement, Statement, UnaryOperator, Variable,
    },
};

/// Owned, lifetime-free description of a program fragment for property testing.
#[derive(Clone, Debug)]
pub struct GenProgram {
    pub rules: Vec<GenRule>,
    pub begin: Option<GenBody>,
}

#[derive(Clone, Debug)]
pub struct GenRule {
    pub pattern: Option<GenPattern>,
    pub body: GenBody,
}

#[derive(Clone, Debug)]
pub enum GenPattern {
    Expr(GenExpr),
    Regex(u8),
}

#[derive(Clone, Debug)]
pub struct GenBody {
    pub statements: Vec<GenStatement>,
}

#[derive(Clone, Debug)]
pub enum GenStatement {
    Print(Vec<GenExpr>),
    Expr(GenExpr),
    If { condition: GenExpr, then_body: GenBody, else_body: Option<GenBody> },
    While { condition: GenExpr, body: GenBody },
}

#[derive(Clone, Debug)]
pub enum GenExpr {
    Atom(GenAtom),
    Unary(UnaryOperator, Box<Self>),
    Binary(BinaryOperator, Box<Self>, Box<Self>),
    Assign(GenPlace, Box<Self>),
    Index(u8, Box<Self>),
}

#[derive(Clone, Debug)]
pub enum GenAtom {
    SmallInt(i8),
    Var(u8),
    String(u8),
}

#[derive(Clone, Debug)]
pub enum GenPlace {
    Var(u8),
}

pub fn gen_program() -> impl Strategy<Value = GenProgram> {
    (
        prop::option::of(gen_body()),
        prop::collection::vec(gen_rule(), 1..=3),
    )
        .prop_map(|(begin, rules)| GenProgram { rules, begin })
}

fn gen_rule() -> impl Strategy<Value = GenRule> {
    (prop::option::of(gen_pattern()), gen_body())
        .prop_map(|(pattern, body)| GenRule { pattern, body })
}

fn gen_pattern() -> impl Strategy<Value = GenPattern> {
    prop_oneof![
        2 => gen_expr().prop_map(GenPattern::Expr),
        1 => (0u8..4).prop_map(GenPattern::Regex),
    ]
}

fn gen_body() -> impl Strategy<Value = GenBody> {
    prop::collection::vec(gen_statement(), 1..=4).prop_map(|statements| GenBody { statements })
}

fn gen_simple_body() -> impl Strategy<Value = GenBody> {
    prop::collection::vec(gen_simple_statement(), 1..=3)
        .prop_map(|statements| GenBody { statements })
}

fn gen_simple_statement() -> impl Strategy<Value = GenStatement> {
    prop_oneof![
        2 => prop::collection::vec(gen_leaf(), 0..=3).prop_map(GenStatement::Print),
        1 => gen_expr().prop_map(GenStatement::Expr),
    ]
}

fn gen_statement() -> impl Strategy<Value = GenStatement> {
    prop_oneof![
        3 => prop::collection::vec(gen_leaf(), 0..=3).prop_map(GenStatement::Print),
        2 => gen_expr().prop_map(GenStatement::Expr),
        1 => (
            gen_expr(),
            gen_simple_body(),
            prop::option::of(gen_simple_body()),
        )
            .prop_map(|(condition, then_body, else_body)| GenStatement::If {
                condition,
                then_body,
                else_body,
            }),
        1 => (gen_expr(), gen_simple_body())
            .prop_map(|(condition, body)| GenStatement::While { condition, body }),
    ]
}

fn gen_leaf() -> impl Strategy<Value = GenExpr> {
    prop_oneof![
        (0i8..=127)
            .prop_map(GenAtom::SmallInt)
            .prop_map(GenExpr::Atom),
        (0u8..4).prop_map(GenAtom::Var).prop_map(GenExpr::Atom),
        (0u8..3).prop_map(GenAtom::String).prop_map(GenExpr::Atom),
    ]
}

fn gen_arith_binary_operator() -> impl Strategy<Value = BinaryOperator> {
    prop_oneof![
        Just(BinaryOperator::Multiply),
        Just(BinaryOperator::Add),
        Just(BinaryOperator::Subtract),
        Just(BinaryOperator::And),
        Just(BinaryOperator::Or),
    ]
}

fn gen_cmp_binary_operator() -> impl Strategy<Value = BinaryOperator> {
    prop_oneof![
        Just(BinaryOperator::Eq),
        Just(BinaryOperator::NEq),
        Just(BinaryOperator::Lt),
        Just(BinaryOperator::Gt),
        Just(BinaryOperator::LtE),
        Just(BinaryOperator::GtE),
    ]
}

fn gen_arith_expr() -> impl Strategy<Value = GenExpr> {
    gen_leaf().prop_recursive(3, 32, 8, |inner| {
        prop_oneof![
            2 => inner
                .clone()
                .prop_map(|e| GenExpr::Unary(UnaryOperator::Negation, Box::new(e))),
            2 => (gen_arith_binary_operator(), inner.clone(), inner.clone())
                .prop_map(|(op, a, b)| GenExpr::Binary(op, Box::new(a), Box::new(b))),
            1 => (gen_place(), inner.clone())
                .prop_map(|(place, rhs)| GenExpr::Assign(place, Box::new(rhs))),
            1 => (0u8..4, inner.clone())
                .prop_map(|(var, idx)| GenExpr::Index(var, Box::new(idx))),
        ]
    })
}

fn gen_expr() -> impl Strategy<Value = GenExpr> {
    prop_oneof![
        4 => gen_arith_expr(),
        1 => (
            gen_cmp_binary_operator(),
            gen_arith_expr(),
            gen_arith_expr(),
        )
            .prop_map(|(op, a, b)| GenExpr::Binary(op, Box::new(a), Box::new(b))),
    ]
}

fn gen_place() -> impl Strategy<Value = GenPlace> {
    (0u8..4).prop_map(GenPlace::Var)
}

pub fn materialize<'a>(program: &GenProgram, arena: &'a Bump) -> Ast<'a> {
    let mut ast = Ast::new(arena);
    if let Some(body) = &program.begin {
        ast.begin.push(materialize_body(body, arena));
    }
    for rule in &program.rules {
        ast.rules.push(materialize_rule(rule, arena));
    }
    ast
}

fn materialize_rule<'a>(rule: &GenRule, arena: &'a Bump) -> Rule<'a> {
    Rule {
        pattern: rule
            .pattern
            .as_ref()
            .map(|pat| materialize_pattern(pat, arena)),
        actions: Some(materialize_body(&rule.body, arena)),
    }
}

fn materialize_pattern<'a>(pattern: &GenPattern, arena: &'a Bump) -> RulePattern<'a> {
    match pattern {
        GenPattern::Expr(expr) => RulePattern::Expression(materialize_expr(expr, arena)),
        GenPattern::Regex(n) => {
            RulePattern::Expression(Expr::leaf(Atom::Regex(regex_slice(arena, *n))))
        }
    }
}

fn materialize_body<'a>(body: &GenBody, arena: &'a Bump) -> Body<'a> {
    Body(
        body.statements
            .iter()
            .map(|s| materialize_statement(s, arena))
            .collect_in(arena),
    )
}

fn materialize_statement<'a>(stmnt: &GenStatement, arena: &'a Bump) -> Statement<'a> {
    match stmnt {
        GenStatement::Print(args) => Statement::Simple(SimpleStatement::Command {
            name: Command::Print,
            args: args
                .iter()
                .map(|e| materialize_expr(e, arena))
                .collect_in(arena),
            redirection: None,
        }),
        GenStatement::Expr(expr) => {
            Statement::Simple(SimpleStatement::Expression(materialize_expr(expr, arena)))
        }
        GenStatement::If { condition, then_body, else_body } => Statement::If {
            condition: materialize_expr(condition, arena),
            then_body: materialize_body(then_body, arena),
            else_body: else_body.as_ref().map(|body| materialize_body(body, arena)),
        },
        GenStatement::While { condition, body } => Statement::While {
            condition: materialize_expr(condition, arena),
            then_body: materialize_body(body, arena),
        },
    }
}

fn materialize_expr<'a>(expr: &GenExpr, arena: &'a Bump) -> Expr<'a> {
    match expr {
        GenExpr::Atom(atom) => Expr::leaf(materialize_atom(atom, arena)),
        GenExpr::Unary(op, inner) => Expr::node(op.expr(materialize_expr(inner, arena)), arena),
        GenExpr::Binary(op, a, b) => Expr::node(
            op.expr(materialize_expr(a, arena), materialize_expr(b, arena)),
            arena,
        ),
        GenExpr::Assign(place, rhs) => Expr::node(
            BinaryPlaceOperator::Assignment.expr(
                materialize_place(place, arena),
                materialize_expr(rhs, arena),
            ),
            arena,
        ),
        GenExpr::Index(var, idx) => Expr::node(
            crate::ast::ArrayOperator::Index.expr(
                materialize_var(*var, arena),
                bumpalo::vec![in arena; materialize_expr(idx, arena)],
            ),
            arena,
        ),
    }
}

fn materialize_place<'a>(place: &GenPlace, arena: &'a Bump) -> Place<'a> {
    match place {
        GenPlace::Var(v) => Place::Variable(materialize_var(*v, arena)),
    }
}

fn materialize_atom<'a>(atom: &GenAtom, arena: &'a Bump) -> Atom<'a> {
    match atom {
        GenAtom::SmallInt(n) => Atom::Integer(i32::from(*n)),
        GenAtom::Var(v) => Atom::Variable(materialize_var(*v, arena)),
        GenAtom::String(n) => Atom::String(text_slice(arena, &format!("s{n}"))),
    }
}

fn materialize_var(index: u8, arena: &Bump) -> Variable<'_> {
    Variable::User(ident(arena, index))
}

fn ident(arena: &Bump, index: u8) -> Identifier<'_> {
    let literal = match index % 4 {
        0 => "a",
        1 => "b",
        2 => "c",
        _ => "d",
    };
    Identifier {
        namespace: "awk",
        literal: arena.alloc_str(literal),
    }
}

fn text_slice<'a>(arena: &'a Bump, content: &str) -> lexer::Slice<'a> {
    arena.alloc_str(content).as_bytes().into()
}

fn regex_slice<'a>(arena: &'a Bump, index: u8) -> lexer::Slice<'a> {
    text_slice(arena, &format!("p{index}"))
}
