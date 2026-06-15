// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! This module contains the bytecode description, designed to be compact
//! for cache efficiency and isomorphic w.r.t Cranelift IR. Also, our bytecode
//! _is_ our IR; we lower the AST into it and can execute it right away, or do
//! an optimization or JIT pass. We don't do the hack Lua 5's VM does of
//! emitting bytecode without an intermediate AST because AWK contextual
//! shenanigans; _even_ if it was possible, good luck maintaining that.

pub mod lower;

use std::fmt::{self, Debug, Display, Formatter};

pub use lower::test_interpreter;
use parser::{Command, Redirection};

pub type RegWidth = u8;
pub type IxWidth = u32;

#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct NonLocal(pub IxWidth);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Reg(pub RegWidth);

#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Label(pub IxWidth);

#[repr(u8, align(16))]
#[derive(Clone, Copy)]
pub enum Instruction {
    // Unary operations
    Record { dest: Reg, arg: Arg, ty: ArgTy },
    Negation { dest: Reg, arg: Arg, ty: ArgTy },
    ToInt { dest: Reg, arg: Arg, ty: ArgTy },
    Negative { dest: Reg, arg: Arg, ty: ArgTy },
    Copy { dest: Reg, arg: Arg, ty: ArgTy },

    // Binary operations
    Eq { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    NEq { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Gt { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Lt { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    LtE { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    GtE { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    And { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Or { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Matches { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    MatchesNot { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Add { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Subtract { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Multiply { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Divide { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Raise { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Modulo { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Concat { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },

    // Intrinsic operations
    StoreS { dest: Reg, ty_place: ArgTy, var: NonLocal, arg: Arg, ty: ArgTy },
    StoreR { dest: Reg, src: Arg, arg: Arg, ty: ArgTy, tys: ArgTy },
    StoreA { dest: Reg, ty_place: ArgTy, start: Reg, end: Reg, var: NonLocal, arg: Reg },
    LoadA { dest: Reg, ty_place: ArgTy, start: Reg, end: Reg, var: NonLocal },
    IntrinsicCall { dest: Reg, start: Reg, end: Reg, name: NonLocal },
    OutputCall { start: Reg, end: Reg, cmd: Command, redir: Option<Redirection> },
    UserCall { dest: Reg, start: Reg, end: Reg, name: NonLocal },
    IndirectCall { dest: Reg, start: Reg, end: Reg, name: Arg, ty: ArgTy },
    Jump { to: Label },
    Return { arg: Arg, ty: ArgTy },
    Branch { then_label: Label, else_label: Label, condition: Reg },
}

const _: () = const { assert!(size_of::<Instruction>() <= size_of::<u128>()) };

#[derive(Clone, Copy)]
pub union Arg {
    pub reg: Reg,
    pub imm: i32,
    pub sym: NonLocal,
}

#[derive(Clone, Copy)]
struct Imm(u32);

#[derive(Clone, Copy, Debug)]
pub enum ArgTy {
    Reg,
    Imm,
    ImmF,
    Rec,
    Cnt,
    UsVal,
    UaVal,
    UmVal,
    IsVal,
    IaVal,
    ImVal,
}

impl Instruction {
    fn set_label(&mut self, label: Label) {
        match self {
            Self::Jump { to } | Self::Branch { else_label: to, then_label: _, condition: _ } => {
                *to = label;
            }
            _ => debug_assert!(false, "Incorrect label set!"),
        }
    }

    fn push_end_label(&mut self) {
        if let Self::Branch { else_label, then_label: _, condition: _ } = self {
            else_label.0 += 1;
        } else {
            debug_assert!(false, "Incorrect label set!");
        }
    }

    fn br(condition: Reg, then_label: Label) -> Self {
        Self::Branch { then_label, else_label: Label(0), condition }
    }
}

impl Display for Instruction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let op = self.display_name();
        let fmt_arg = |f: &mut Formatter, arg: &Arg, ty: &ArgTy, sep| match ty {
            ArgTy::Reg => write!(f, "{sep}{}", unsafe { arg.reg }),
            ArgTy::Imm => write!(f, "{sep}{ty}({})", unsafe { arg.imm }),
            _ => write!(f, "{sep}{ty}({})", unsafe { arg.sym }),
        };
        match self {
            Self::Record { dest, arg, ty }
            | Self::Negation { dest, arg, ty }
            | Self::ToInt { dest, arg, ty }
            | Self::Negative { dest, arg, ty }
            | Self::Copy { dest, arg, ty } => {
                write!(f, "{dest} <- {op}")?;
                fmt_arg(f, arg, ty, " ")
            }
            Self::Eq { dest, lhs, rhs, tyl, tyr }
            | Self::NEq { dest, lhs, rhs, tyl, tyr }
            | Self::Gt { dest, lhs, rhs, tyl, tyr }
            | Self::Lt { dest, lhs, rhs, tyl, tyr }
            | Self::LtE { dest, lhs, rhs, tyl, tyr }
            | Self::GtE { dest, lhs, rhs, tyl, tyr }
            | Self::And { dest, lhs, rhs, tyl, tyr }
            | Self::Or { dest, lhs, rhs, tyl, tyr }
            | Self::Matches { dest, lhs, rhs, tyl, tyr }
            | Self::MatchesNot { dest, lhs, rhs, tyl, tyr }
            | Self::Add { dest, lhs, rhs, tyl, tyr }
            | Self::Subtract { dest, lhs, rhs, tyl, tyr }
            | Self::Multiply { dest, lhs, rhs, tyl, tyr }
            | Self::Divide { dest, lhs, rhs, tyl, tyr }
            | Self::Raise { dest, lhs, rhs, tyl, tyr }
            | Self::Concat { dest, lhs, rhs, tyl, tyr }
            | Self::Modulo { dest, lhs, rhs, tyl, tyr } => {
                write!(f, "{dest} <- {op}")?;
                fmt_arg(f, lhs, tyl, " ")?;
                fmt_arg(f, rhs, tyr, ", ")
            }
            Self::StoreS { dest, ty_place, var, arg, ty } => {
                write!(f, "{dest} <- {op} {ty_place}({var})")?;
                fmt_arg(f, arg, ty, ", ")
            }
            Self::StoreR { dest, src, arg, ty, tys } => {
                write!(f, "{dest} <- {op} $(")?;
                fmt_arg(f, src, tys, "")?;
                fmt_arg(f, arg, ty, "), ")
            }
            Self::StoreA { dest, ty_place, start, end, var, arg } => {
                write!(f, "{dest} <- {op} {ty_place}({var}), {start}..{end}, {arg}")
            }
            Self::LoadA { dest, ty_place, start, end, var } => {
                write!(f, "{dest} <- {op} {ty_place}({var}), {start}..{end})")
            }
            Self::Branch { condition, then_label, else_label } => {
                write!(f, "{op} {condition}, {then_label}, {else_label}")
            }
            Self::Jump { to } => {
                write!(f, "{op} {to}")
            }
            Self::Return { arg, ty } => {
                write!(f, "{op}")?;
                fmt_arg(f, arg, ty, " ")
            }
            Self::IntrinsicCall { dest, start, end, name } => {
                write!(f, "{dest} <- {op} {name}, {start}..{end}")
            }
            Self::IndirectCall { dest, start, end, name, ty } => {
                write!(f, "{dest} <- {op}")?;
                fmt_arg(f, name, ty, " ")?;
                write!(f, ", {start}..{end}")
            }
            Self::OutputCall { start, end, cmd, redir: Some(redir) } => {
                write!(f, "{cmd}{redir:?} {start}..{end}")
            }
            Self::OutputCall { start, end, cmd, redir: None } => {
                write!(f, "{cmd} {start}..{end}")
            }
            Self::UserCall { dest, start, end, name } => {
                write!(f, "{dest} <- {op} {name}, {start}..{end}")
            }
        }
    }
}

impl Instruction {
    fn display_name(self) -> &'static str {
        match self {
            Self::Record { dest: _, arg: _, ty: _ } => "rec",
            Self::Negation { dest: _, arg: _, ty: _ } => "not",
            Self::ToInt { dest: _, arg: _, ty: _ } => "int",
            Self::Negative { dest: _, arg: _, ty: _ } => "neg",
            Self::Concat { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "cat",
            Self::Eq { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "eq",
            Self::NEq { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "neq",
            Self::Gt { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "gt",
            Self::Lt { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "lt",
            Self::LtE { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "le",
            Self::GtE { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "ge",
            Self::And { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "and",
            Self::Or { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "or",
            Self::Matches { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "mtch",
            Self::MatchesNot { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "nmtch",
            Self::Add { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "add",
            Self::Subtract { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "sub",
            Self::Multiply { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "mul",
            Self::Divide { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "div",
            Self::Raise { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "pow",
            Self::Modulo { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => "mod",
            Self::StoreS { dest: _, ty_place: _, var: _, arg: _, ty: _ } => "sstore",
            Self::StoreR { dest: _, src: _, arg: _, ty: _, tys: _ } => "rstore",
            Self::StoreA {
                dest: _,
                ty_place: _,
                start: _,
                end: _,
                var: _,
                arg: _,
            } => "astore",
            Self::LoadA { dest: _, ty_place: _, start: _, end: _, var: _ } => "aload",
            Self::Copy { dest: _, arg: _, ty: _ } => "cpy",
            Self::IntrinsicCall { dest: _, start: _, end: _, name: _ } => "icall",
            Self::UserCall { dest: _, start: _, end: _, name: _ } => "ucall",
            Self::IndirectCall { dest: _, start: _, end: _, name: _, ty: _ } => "vcall",
            Self::OutputCall { start: _, end: _, cmd: _, redir: _ } => "out",
            Self::Jump { to: _ } => "jmp",
            Self::Return { arg: _, ty: _ } => "ret",
            Self::Branch { then_label: _, else_label: _, condition: _ } => "brif",
        }
    }
}

impl Display for Label {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        <_ as Display>::fmt(&self.0, f)
    }
}

impl Display for Reg {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

impl Display for NonLocal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        <_ as Display>::fmt(&self.0, f)
    }
}

impl Display for ArgTy {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reg => write!(f, "r"),
            Self::Imm => write!(f, "imm"),
            Self::ImmF => write!(f, "immf"),
            Self::Rec => write!(f, "$"),
            Self::Cnt => write!(f, "mem"),
            Self::UsVal => write!(f, "us"),
            Self::UaVal => write!(f, "ua"),
            Self::UmVal => write!(f, "um"),
            Self::IsVal => write!(f, "is"),
            Self::IaVal => write!(f, "ia"),
            Self::ImVal => write!(f, "im"),
        }
    }
}
