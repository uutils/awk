// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::fmt::Debug;

use ahash::RandomState;
use bumpalo::{Bump, boxed::Box, collections::Vec};
use either::Either;
use hashbrown::HashMap;
use lexer::{Slice, Span, Token};

use crate::{ParsingError, Result, lex::TokenExt};

#[derive(Debug)]
pub struct Ast<'a> {
    pub loads: Vec<'a, Slice<'a>>,
    pub begin: Vec<'a, Body<'a>>,
    pub end: Vec<'a, Body<'a>>,
    pub begin_file: Vec<'a, Body<'a>>,
    pub end_file: Vec<'a, Body<'a>>,
    pub rules: Vec<'a, Rule<'a>>,
    pub concurrent: Vec<'a, Rule<'a>>,
    pub functions: HashMap<Identifier<'a>, Function<'a>, RandomState, &'a Bump>,
}

#[derive(Debug)]
pub struct Rule<'a> {
    pub pattern: Option<RulePattern<'a>>,
    pub actions: Option<Body<'a>>,
}

#[derive(Clone)]
pub enum Atom<'a> {
    Variable(Variable<'a>),
    String(Slice<'a>),
    SmallInt(i8),
    Number(f64),
    BigInt(),
    BigFloat(),
    Regex(Slice<'a>),
    TypedRegex(Slice<'a>),
}

pub enum RulePattern<'a> {
    Expression(Expr<'a>),
    Range(Expr<'a>, Expr<'a>),
}

#[derive(Debug, Clone)]
pub enum SpecialPattern {
    Begin,
    End,
    BeginFile,
    EndFile,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub struct Identifier<'a> {
    pub namespace: &'a str,
    pub literal: &'a str,
}

#[repr(u16)]
#[derive(Clone, Copy)]
pub enum Variable<'a> {
    User(Identifier<'a>),
    Nr,
    Nf,
    Fs,
    Rs,
    Ofs,
    Ors,
    Filename,
    Argc,
    Argv,
    Subsep,
    Fnr,
    Argind,
    Ofmt,
    Rstart,
    Rlength,
    Environ,
}

pub enum Expr<'a> {
    Leaf(Atom<'a>),
    Node(Box<'a, ExprNode<'a>>),
}

pub struct Body<'a>(pub Vec<'a, Statement<'a>>);
pub type Pattern<'a> = Either<RulePattern<'a>, SpecialPattern>;

#[derive(Debug)]
pub enum ExprNode<'a> {
    FunctionCall(Identifier<'a>, Vec<'a, Expr<'a>>),
    IndirectCall(Variable<'a>, Vec<'a, Expr<'a>>),
    UnaryOperation(UnaryOperator, Expr<'a>),
    BinaryOperation(BinaryOperator, Expr<'a>, Expr<'a>),
    UnaryPlaceOperation(UnaryPlaceOperator, Place<'a>),
    BinaryPlaceOperation(BinaryPlaceOperator, Place<'a>, Expr<'a>),
    ArrayOperation(ArrayOperator, Variable<'a>, Vec<'a, Expr<'a>>),
    NestedArray(Expr<'a>, Vec<'a, Expr<'a>>),
    Ternary(Expr<'a>, Expr<'a>, Expr<'a>),
    Getline(Getline<'a>),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum UnaryOperator {
    Record,
    Negation,
    ToInt,
    Negative,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BinaryOperator {
    Concat,
    Eq,
    NEq,
    Gt,
    Lt,
    LtE,
    GtE,
    And,
    Or,
    Matches,
    MatchesNot,
    Add,
    Subtract,
    Multiply,
    Divide,
    Raise,
    Modulo,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum UnaryPlaceOperator {
    IncrementL,
    DecrementL,
    IncrementR,
    DecrementR,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BinaryPlaceOperator {
    Assignment,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    PowAssign,
    ModAssign,
}
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ArrayOperator {
    Index,
    In,
}

/// Essentially lvalues. To the interpreter, these do not produce a value, but
/// get theirs modified. A place is a subset of all expressions.
pub enum Place<'a> {
    Record(Expr<'a>),
    Variable(Variable<'a>),
    Index(Variable<'a>, Vec<'a, Expr<'a>>),
    ChainedIndex(Expr<'a>, Vec<'a, Expr<'a>>),
}

/// GNU docs: https://www.gnu.org/software/gawk/manual/html_node/Redirection.html
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum Redirection {
    WriteFile,
    AppendFile,
    PipeIn,
    CoprocessIn,
}

#[derive(Debug, Clone, Copy)]
pub enum WriteKind {
    Pipe,
    /// GNU Extension, `|&`.
    Coprocess,
}

#[derive(Debug)]
pub enum Getline<'a> {
    // getline (var)?
    FromInput(Option<Place<'a>>),
    // getline (var)? < (file)
    FromFile(Option<Place<'a>>, Expr<'a>),
    // (expr) | getline (var)?
    PipeOut(Option<Place<'a>>, Expr<'a>),
    // (expr) |& getline (var)?
    CoprocessOut(Option<Place<'a>>, Expr<'a>),
}

pub enum Statement<'a> {
    Simple(SimpleStatement<'a>),
    If {
        condition: Expr<'a>,
        then_body: Body<'a>,
        else_body: Option<Body<'a>>,
    },
    While {
        condition: Expr<'a>,
        then_body: Body<'a>,
    },
    DoWhile {
        then_body: Body<'a>,
        condition: Expr<'a>,
    },
    For {
        init: Option<SimpleStatement<'a>>,
        condition: Option<Expr<'a>>,
        update: Option<SimpleStatement<'a>>,
        body: Body<'a>,
    },
    ForEach {
        variable: Variable<'a>,
        array: Variable<'a>,
        body: Body<'a>,
    },
    Switch {
        scrutinee: Expr<'a>,
        branches: Vec<'a, (Atom<'a>, Body<'a>)>,
        default: Option<(Body<'a>, usize)>,
    },
    Break,
    Continue,
    Return(Option<Expr<'a>>),
    Next,
    NextFile,
    Exit(Option<Expr<'a>>),
}

pub enum SimpleStatement<'a> {
    Expression(Expr<'a>),
    Command {
        name: Command,
        args: Vec<'a, Expr<'a>>,
        redirection: Option<(Redirection, Expr<'a>)>,
    },
    Delete(Variable<'a>, Option<Vec<'a, Expr<'a>>>),
}

#[derive(Debug)]
pub struct Function<'a> {
    pub args: Vec<'a, Identifier<'a>>,
    pub body: Body<'a>,
}

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum Command {
    Print,
    Printf,
}

impl<'a> Expr<'a> {
    pub fn leaf(from: impl Into<Atom<'a>>) -> Self {
        Self::Leaf(from.into())
    }

    pub fn node(op: impl Into<ExprNode<'a>>, arena: &'a Bump) -> Self {
        Self::Node(Box::new_in(op.into(), arena))
    }
}

impl UnaryOperator {
    pub fn expr(self, a: Expr<'_>) -> ExprNode<'_> {
        ExprNode::UnaryOperation(self, a)
    }
}

impl BinaryOperator {
    pub fn expr<'a>(self, a: Expr<'a>, b: Expr<'a>) -> ExprNode<'a> {
        ExprNode::BinaryOperation(self, a, b)
    }
}

impl UnaryPlaceOperator {
    pub fn expr(self, a: Place<'_>) -> ExprNode<'_> {
        ExprNode::UnaryPlaceOperation(self, a)
    }
}

impl BinaryPlaceOperator {
    pub fn expr<'a>(self, a: Place<'a>, b: Expr<'a>) -> ExprNode<'a> {
        ExprNode::BinaryPlaceOperation(self, a, b)
    }
}

impl ArrayOperator {
    pub fn expr<'a>(self, a: Variable<'a>, b: Vec<'a, Expr<'a>>) -> ExprNode<'a> {
        ExprNode::ArrayOperation(self, a, b)
    }
}

impl<'a> From<Getline<'a>> for ExprNode<'a> {
    fn from(value: Getline<'a>) -> Self {
        Self::Getline(value)
    }
}

impl Expr<'_> {
    pub fn take(&mut self) -> Self {
        std::mem::replace(self, Self::Leaf(Atom::Number(0.)))
    }
}

impl<'a> From<Identifier<'a>> for Variable<'a> {
    fn from(value: Identifier<'a>) -> Self {
        Self::User(value)
    }
}

impl<'a> From<Variable<'a>> for Atom<'a> {
    fn from(value: Variable<'a>) -> Self {
        Self::Variable(value)
    }
}

impl<'a> From<Variable<'a>> for Place<'a> {
    fn from(value: Variable<'a>) -> Self {
        Self::Variable(value)
    }
}

impl From<f64> for Atom<'_> {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl<'a> UnaryOperator {
    pub fn parse(value: &Token<'a>, span: &Span) -> Result<Self> {
        match value {
            Token::Record => Ok(Self::Record),
            Token::Negation => Ok(Self::Negation),
            Token::Plus => Ok(Self::ToInt),
            Token::Minus => Ok(Self::Negative),
            _ => Err(ParsingError::UnexpectedToken(
                span.clone(),
                "expected an unary operator.".into(),
            )),
        }
    }
}

impl<'a> BinaryOperator {
    pub fn parse(value: &Token<'a>, span: &Span) -> Result<Self> {
        match value {
            Token::EqualTo => Ok(Self::Eq),
            Token::NotEqualTo => Ok(Self::NEq),
            Token::GreaterThan => Ok(Self::Gt),
            Token::LesserThan => Ok(Self::Lt),
            Token::GreaterOrEqualThan => Ok(Self::GtE),
            Token::LesserOrEqualThan => Ok(Self::LtE),
            Token::Matching => Ok(Self::Matches),
            Token::NotMatching => Ok(Self::MatchesNot),
            Token::Plus => Ok(Self::Add),
            Token::Minus => Ok(Self::Subtract),
            Token::Star => Ok(Self::Multiply),
            Token::Slash => Ok(Self::Divide),
            Token::Circumflex => Ok(Self::Raise),
            Token::BooleanAnd => Ok(Self::And),
            Token::BooleanOr => Ok(Self::Or),
            Token::Percent => Ok(Self::Modulo),
            t if t.is_expr_start() => Ok(Self::Concat),
            _ => Err(ParsingError::UnexpectedToken(
                span.clone(),
                "expected a binary operator.".into(),
            )),
        }
    }
}

impl UnaryPlaceOperator {
    pub fn parse_prefix(value: &Token<'_>, span: &Span) -> Result<Self> {
        match value {
            Token::Increment => Ok(Self::IncrementL),
            Token::Decrement => Ok(Self::DecrementL),
            _ => Err(ParsingError::OperatorExpectsVariable(span.clone())),
        }
    }

    pub fn parse_suffix(value: &Token<'_>, span: &Span) -> Result<Self> {
        match value {
            Token::Increment => Ok(Self::IncrementR),
            Token::Decrement => Ok(Self::DecrementR),
            _ => Err(ParsingError::OperatorExpectsVariable(span.clone())),
        }
    }
}

impl<'a> BinaryPlaceOperator {
    pub fn parse(value: &Token<'a>, span: &Span) -> Result<Self> {
        match value {
            Token::Assignment => Ok(Self::Assignment),
            Token::PlusAssign => Ok(Self::AddAssign),
            Token::MinusAssign => Ok(Self::SubAssign),
            Token::StarAssign => Ok(Self::MulAssign),
            Token::SlashAssign => Ok(Self::DivAssign),
            Token::CaretAssign => Ok(Self::PowAssign),
            Token::PercentAssign => Ok(Self::ModAssign),
            _ => Err(ParsingError::UnexpectedToken(
                span.clone(),
                "expected a place operator.".into(),
            )),
        }
    }
}

impl<'a> ArrayOperator {
    pub fn parse(value: &Token<'a>, span: &Span) -> Result<Self> {
        match value {
            Token::OpenBracket => Ok(Self::Index),
            Token::In => Ok(Self::In),
            _ => Err(ParsingError::UnexpectedToken(
                span.clone(),
                "expected an array operator.".into(),
            )),
        }
    }
}

impl<'a> Place<'a> {
    /// Attempts to lower an expression into a place; on error returns it back.
    pub fn lower_from(expr: Expr<'a>, span: Span) -> Result<Self, (Expr<'a>, ParsingError)> {
        match expr {
            Expr::Leaf(Atom::Variable(var)) => Ok(Self::Variable(var)),
            Expr::Node(node)
                if matches!(
                    &*node,
                    &ExprNode::UnaryOperation(UnaryOperator::Record, _)
                        | &ExprNode::ArrayOperation(ArrayOperator::Index, _, _)
                        | &ExprNode::NestedArray(_, _)
                ) =>
            {
                match Box::into_inner(node) {
                    ExprNode::UnaryOperation(_, index) => Ok(Self::Record(index)),
                    ExprNode::ArrayOperation(_, var, index) => Ok(Self::Index(var, index)),
                    ExprNode::NestedArray(arr, indices) => Ok(Self::ChainedIndex(arr, indices)),
                    _ => unreachable!("Box is magic; handled awkwardly in the match guard."),
                }
            }
            _ => Err((expr, ParsingError::OperatorExpectsVariable(span))),
        }
    }
}

impl Redirection {
    pub fn parse(value: &Token) -> Option<Self> {
        match value {
            Token::GreaterThan => Some(Self::WriteFile),
            Token::AppendPipe => Some(Self::AppendFile),
            Token::Pipe => Some(Self::PipeIn),
            Token::DoublePipe => Some(Self::CoprocessIn),
            _ => None,
        }
    }
}

impl WriteKind {
    pub fn parse(value: &Token) -> Option<Self> {
        match value {
            Token::Pipe => Some(Self::Pipe),
            Token::DoublePipe => Some(Self::Coprocess),
            _ => None,
        }
    }

    pub fn expr_getline<'a>(self, var: Option<Place<'a>>, expr: Expr<'a>) -> Getline<'a> {
        match self {
            Self::Pipe => Getline::PipeOut(var, expr),
            Self::Coprocess => Getline::CoprocessOut(var, expr),
        }
    }
}

pub struct Ternary;

mod binding_powers {
    pub const BP_GROUPING: u8 = 32; // [, (, ), ]
    pub const BP_RECORD: u8 = 30; // $
    pub const BP_INC_DEC: u8 = 28; // ++, --
    pub const BP_RAISE: (u8, u8) = (27, 26); // ^, **
    pub const BP_UNARY: u8 = 24; // +, -, !
    pub const BP_MULTI: (u8, u8) = (20, 21); // *, /, %
    pub const BP_ADDITION: (u8, u8) = (18, 19); // +, -
    pub const BP_CONCAT: (u8, u8) = (16, 17);
    pub const BP_CMP: (u8, u8) = (14, 15); // < <= == != > >= >> | |&
    pub const BP_MATCH: (u8, u8) = (12, 13); // ~, !~
    pub const BP_IN: (u8, u8) = (10, 11); // in
    pub const BP_AND: (u8, u8) = (8, 9); // &&
    pub const BP_OR: (u8, u8) = (6, 7); // ||
    pub const BP_TERNARY: (u8, u8) = (4, 3); // ?, :
    pub const BP_ASSIGN: (u8, u8) = (2, 1); // =, {+, -, *, /, %, ^, **}=
}

pub trait BindingPower {
    type Bp;
    fn binding_power(&self) -> Self::Bp;
}

impl BindingPower for BinaryOperator {
    type Bp = (u8, u8);
    fn binding_power(&self) -> Self::Bp {
        match self {
            Self::Concat => binding_powers::BP_CONCAT,
            Self::Eq | Self::NEq | Self::Gt | Self::Lt | Self::LtE | Self::GtE => {
                binding_powers::BP_CMP
            }
            Self::And => binding_powers::BP_AND,
            Self::Or => binding_powers::BP_OR,
            Self::Matches | Self::MatchesNot => binding_powers::BP_MATCH,
            Self::Add => binding_powers::BP_ADDITION,
            Self::Subtract => binding_powers::BP_ADDITION,
            Self::Multiply => binding_powers::BP_MULTI,
            Self::Divide => binding_powers::BP_MULTI,
            Self::Raise => binding_powers::BP_RAISE,
            Self::Modulo => binding_powers::BP_MULTI,
        }
    }
}

impl BindingPower for UnaryPlaceOperator {
    type Bp = u8;
    fn binding_power(&self) -> Self::Bp {
        binding_powers::BP_INC_DEC
    }
}

impl BindingPower for BinaryPlaceOperator {
    type Bp = (u8, u8);
    fn binding_power(&self) -> Self::Bp {
        binding_powers::BP_ASSIGN
    }
}

impl BindingPower for ArrayOperator {
    type Bp = (u8, u8);
    fn binding_power(&self) -> Self::Bp {
        match self {
            Self::Index => (binding_powers::BP_GROUPING, 0),
            Self::In => binding_powers::BP_IN,
        }
    }
}

impl BindingPower for UnaryOperator {
    type Bp = u8;
    fn binding_power(&self) -> Self::Bp {
        match self {
            Self::Record => binding_powers::BP_RECORD,
            Self::Negation => binding_powers::BP_UNARY,
            Self::ToInt => binding_powers::BP_UNARY,
            Self::Negative => binding_powers::BP_UNARY,
        }
    }
}

impl BindingPower for Ternary {
    type Bp = (u8, u8);
    fn binding_power(&self) -> Self::Bp {
        binding_powers::BP_TERNARY
    }
}

impl<'a> From<Vec<'a, Statement<'a>>> for Body<'a> {
    fn from(value: Vec<'a, Statement<'a>>) -> Self {
        Self(value)
    }
}
