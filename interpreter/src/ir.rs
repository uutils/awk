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
#[cfg(test)]
mod tests;

use std::{
    fmt::{self, Debug, Display, Formatter},
    ops::Deref,
};

use parser::{BuiltinFunction, Command, Redirection};

pub type RegWidth = u8;
pub type IxWidth = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    LoadF { dest: Reg, arg: Arg, ty: ArgTy },
    Negation { dest: Reg, arg: Arg, ty: ArgTy },
    ToInt { dest: Reg, arg: Arg, ty: ArgTy },
    Negative { dest: Reg, arg: Arg, ty: ArgTy },
    IncrementPost { dest: Reg, arg: Arg, ty: PlaceTy },
    DecrementPost { dest: Reg, arg: Arg, ty: PlaceTy },
    IncrementPre { dest: Reg, arg: Arg, ty: PlaceTy },
    DecrementPre { dest: Reg, arg: Arg, ty: PlaceTy },
    CopyP { dest: Reg, arg: Arg, ty: ArgTy },
    CopyS { dest: Reg, arg: Arg, ty: ArgTy },
    CopyA { dest: Reg, arg: Arg, ty: PlaceTy },
    DeleteA { arg: Arg, ty: PlaceTy },

    // Binary operations
    Eq { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    NEq { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Gt { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Lt { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    LtE { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    GtE { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Matches { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    MatchesNot { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Add { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Subtract { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Multiply { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Divide { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Raise { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Modulo { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    Concat { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: ArgTy },
    In { dest: Reg, lhs: Arg, rhs: Arg, tyr: ArgTy, tyl: PlaceTy },

    // Intrinsic operations
    StoreS { dest: Reg, ty_place: PlaceTy, var: NonLocal, arg: Arg, ty: ArgTy },
    StoreF { dest: Reg, src: Arg, arg: Arg, ty: ArgTy, tys: ArgTy },
    StoreA { dest: Reg, lhs: Arg, rhs: Arg, start: Reg, end: Reg, tyl: PlaceTy, tyr: ArgTy },
    LoadA { dest: Reg, arg: Arg, start: Reg, end: Reg, ty: PlaceTy },
    InA { dest: Reg, arg: Arg, start: Reg, end: Reg, ty: PlaceTy },
    LoadM { dest: Reg, arg: Arg, start: Reg, end: Reg, ty: PlaceTy },
    DeleteM { arg: Arg, start: Reg, end: Reg, ty: PlaceTy },
    IntrinsicCall { dest: Reg, start: Reg, end: Reg, fun: BuiltinFunction },
    OutputCall { start: Reg, end: Reg, cmd: Command, redir: Option<Redirection> },
    UserCall { dest: Reg, start: Reg, end: Reg, name: NonLocal },
    IndirectCall { dest: Reg, start: Reg, end: Reg, name: Arg, ty: ArgTy },
    Jump { to: Label },
    Branch { then_label: Label, else_label: Label, condition: Reg },

    // Traps
    Exit { arg: Arg, ty: ArgTy },
    Return { arg: Arg, ty: ArgTy },
    ReturnUnassigned,
    Next,
    NextFile,
}

/// Keeps the size bounded to reduce cache pressure. They can actually be halved
/// to an [`u64`]; this requires setting `imm`s and [`IxWidth`] to [`u16`], and
/// doing some fancy tricks with the `Store`s. However, this constrains a lot
/// more our active development, and if your hardware prefetcher is particularly
/// smart (like in my case), makes almost no difference LOL. So, might be done
/// in the future, but *only* if it won't meaningfully hurt instruction decode.
//
/// Other VM folks will point and laugh at our 16-bytes instrs, though, but our
/// performance bottlenecks hardly lie here; AWK code is generally very small.
const _: () = const { assert!(size_of::<Instruction>() <= size_of::<u128>()) };

#[derive(Clone, Copy)]
pub union Arg {
    pub reg: Reg,
    pub imm: i32,
    pub sym: NonLocal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ArgTy {
    Reg,
    Imm,
    Cnt,
    UsVal,
    UaVal,
    IsVal,
    IaVal,
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum PlaceTy {
    Reg = ArgTy::Reg as u8,
    UsVal = ArgTy::UsVal as u8,
    UaVal = ArgTy::UaVal as u8,
    IsVal = ArgTy::IsVal as u8,
    IaVal = ArgTy::IaVal as u8,
}

const _: () = {
    assert!(size_of::<PlaceTy>() == size_of::<ArgTy>());
    assert!(align_of::<PlaceTy>() == align_of::<ArgTy>());
};

impl Instruction {
    fn set_label(&mut self, label: Label) {
        match self {
            Self::Jump { to } | Self::Branch { else_label: to, then_label: _, condition: _ } => {
                *to = label;
            }
            _ => debug_assert!(false, "Incorrect label set!"),
        }
    }

    fn set_then_label(&mut self, label: Label) {
        if let Self::Branch { then_label, else_label: _, condition: _ } = self {
            *then_label = label;
        } else {
            debug_assert!(false, "Incorrect label set!");
        }
    }

    fn push_end_label(&mut self) {
        if let Self::Branch { else_label, then_label: _, condition: _ } = self {
            else_label.0 += 1;
        } else {
            debug_assert!(false, "Incorrect label set!");
        }
    }

    const fn br(condition: Reg, then_label: Label) -> Self {
        Self::Branch { then_label, else_label: Label(0), condition }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn to_bytes(self) -> u128 {
        use std::mem::{MaybeUninit, transmute};

        // miri does not support inline asm. Intentionally if-gated so it's
        // still compiled anyway.
        if cfg!(miri) {
            return Default::default();
        }

        /// Freezes the value at the LLVM level, essentially marks it as initialized,
        /// even if possibly invalid. In this case, taking T means it is valid, and we
        /// seek seek to make its whole bit pattern initialized but unspecified.
        ///
        /// Note that this relies on an LLVM hack; it's just a quite uncontroversial
        /// cop-out for the lack of a freeze intrinsic, whose RFC is on the works.
        #[inline(always)]
        fn freeze<T>(mut val: MaybeUninit<T>) -> MaybeUninit<T> {
            unsafe {
                std::arch::asm!(
                    "/* freeze {0} */",
                    in(reg) &raw mut val,
                    options(nostack, preserves_flags),
                );
            }
            val
        }

        // SAFETY: the return value is (partially) unspecified, because
        // it reads arbitrary padding bytes. However, it is frozen, and
        // therefore not UB.
        unsafe { freeze(transmute::<Self, MaybeUninit<u128>>(self)).assume_init() }
    }

    #[cfg(target_arch = "wasm32")]
    fn to_bytes(self) -> u128 {
        Default::default()
    }
}

impl From<PlaceTy> for ArgTy {
    #[inline(always)]
    fn from(value: PlaceTy) -> Self {
        match value {
            PlaceTy::Reg => Self::Reg,
            PlaceTy::UsVal => Self::UsVal,
            PlaceTy::UaVal => Self::UaVal,
            PlaceTy::IsVal => Self::IsVal,
            PlaceTy::IaVal => Self::IaVal,
        }
    }
}

/// Deref polymorphism is not _that_ bad.
impl Deref for PlaceTy {
    type Target = ArgTy;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        // SAFETY: The target is declared as a strict superset with equal repr.
        unsafe { &*(&raw const *self).cast::<Self::Target>() }
    }
}

impl TryFrom<ArgTy> for PlaceTy {
    type Error = ();

    #[inline(always)]
    fn try_from(value: ArgTy) -> Result<Self, Self::Error> {
        match value {
            ArgTy::Reg => Ok(Self::Reg),
            ArgTy::UsVal => Ok(Self::UsVal),
            ArgTy::UaVal => Ok(Self::UaVal),
            ArgTy::IsVal => Ok(Self::IsVal),
            ArgTy::IaVal => Ok(Self::IaVal),
            _ => Err(()),
        }
    }
}

impl Debug for Instruction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:032x}", self.to_bytes())
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
            Self::LoadF { dest, arg, ty }
            | Self::Negation { dest, arg, ty }
            | Self::ToInt { dest, arg, ty }
            | Self::Negative { dest, arg, ty }
            | Self::CopyS { dest, arg, ty }
            | Self::CopyP { dest, arg, ty } => {
                write!(f, "{dest} <- {op}")?;
                fmt_arg(f, arg, ty, " ")
            }
            Self::CopyA { dest, arg, ty } => {
                write!(f, "{dest} <- {op}")?;
                fmt_arg(f, arg, ty, " ")
            }
            Self::DeleteA { arg, ty } => {
                write!(f, "{op}")?;
                fmt_arg(f, arg, ty, " ")
            }
            Self::Eq { dest, lhs, rhs, tyl, tyr }
            | Self::NEq { dest, lhs, rhs, tyl, tyr }
            | Self::Gt { dest, lhs, rhs, tyl, tyr }
            | Self::Lt { dest, lhs, rhs, tyl, tyr }
            | Self::LtE { dest, lhs, rhs, tyl, tyr }
            | Self::GtE { dest, lhs, rhs, tyl, tyr }
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
            Self::In { dest, lhs, rhs, tyr, tyl } => {
                write!(f, "{dest} <- {op}")?;
                fmt_arg(f, lhs, tyl, " ")?;
                fmt_arg(f, rhs, tyr, ", ")
            }
            Self::StoreS { dest, ty_place, var, arg, ty } => {
                let ty_place = ArgTy::from(*ty_place);
                write!(f, "{dest} <- {op} {ty_place}({var})")?;
                fmt_arg(f, arg, ty, ", ")
            }
            Self::StoreF { dest, src, arg, ty, tys } => {
                write!(f, "{dest} <- {op} $(")?;
                fmt_arg(f, src, tys, "")?;
                fmt_arg(f, arg, ty, "), ")
            }
            Self::StoreA { dest, lhs, rhs, start, end, tyl, tyr } => {
                write!(f, "{dest} <- {op}")?;
                fmt_arg(f, lhs, tyl, " ")?;
                write!(f, ", {start}..{end}")?;
                fmt_arg(f, rhs, tyr, ", ")
            }
            Self::LoadA { dest, arg, start, end, ty }
            | Self::LoadM { dest, arg, start, end, ty }
            | Self::InA { dest, arg, start, end, ty } => {
                write!(f, "{dest} <- {op}")?;
                fmt_arg(f, arg, ty, " ")?;
                write!(f, ", {start}..{end}")
            }
            Self::DeleteM { arg, start, end, ty } => {
                write!(f, "{op}")?;
                fmt_arg(f, arg, ty, " ")?;
                write!(f, ", {start}..{end}")
            }
            Self::IncrementPost { dest, arg, ty }
            | Self::IncrementPre { dest, arg, ty }
            | Self::DecrementPost { dest, arg, ty }
            | Self::DecrementPre { dest, arg, ty } => {
                write!(f, "{dest} <- {op}")?;
                fmt_arg(f, arg, ty, " ")
            }
            Self::Branch { condition, then_label, else_label } => {
                write!(f, "{op} {condition}, {then_label}, {else_label}")
            }
            Self::Jump { to } => {
                write!(f, "{op} {to}")
            }
            Self::Return { arg, ty } | Self::Exit { arg, ty } => {
                write!(f, "{op}")?;
                fmt_arg(f, arg, ty, " ")
            }
            Self::IntrinsicCall { dest, start, end, fun } => {
                write!(f, "{dest} <- {op} {fun}, {start}..{end}")
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
            Self::Next | Self::NextFile | Self::ReturnUnassigned => {
                write!(f, "{op}")
            }
        }
    }
}

impl Instruction {
    const fn display_name(self) -> &'static str {
        match self {
            Self::LoadF { .. } => "fload",
            Self::Negation { .. } => "not",
            Self::ToInt { .. } => "int",
            Self::Negative { .. } => "neg",
            Self::Concat { .. } => "cat",
            Self::IncrementPost { .. } => "incpst",
            Self::IncrementPre { .. } => "incpre",
            Self::DecrementPost { .. } => "decpst",
            Self::DecrementPre { .. } => "decpre",
            Self::Eq { .. } => "eq",
            Self::NEq { .. } => "neq",
            Self::Gt { .. } => "gt",
            Self::Lt { .. } => "lt",
            Self::LtE { .. } => "le",
            Self::GtE { .. } => "ge",
            Self::Matches { .. } => "mtch",
            Self::MatchesNot { .. } => "nmtch",
            Self::Add { .. } => "add",
            Self::Subtract { .. } => "sub",
            Self::Multiply { .. } => "mul",
            Self::Divide { .. } => "div",
            Self::Raise { .. } => "pow",
            Self::Modulo { .. } => "mod",
            Self::StoreS { .. } => "sstore",
            Self::StoreF { .. } => "fstore",
            Self::StoreA { .. } => "astore",
            Self::LoadA { .. } => "aload",
            Self::LoadM { .. } => "mload",
            Self::In { .. } => "in",
            Self::InA { .. } => "ain",
            Self::CopyS { .. } => "scpy",
            Self::CopyA { .. } => "acpy",
            Self::CopyP { .. } => "pcpy",
            Self::DeleteA { .. } => "adel",
            Self::DeleteM { .. } => "mdel",
            Self::IntrinsicCall { .. } => "icall",
            Self::UserCall { .. } => "ucall",
            Self::IndirectCall { .. } => "vcall",
            Self::OutputCall { .. } => "out",
            Self::Jump { .. } => "jmp",
            Self::Return { .. } | Self::ReturnUnassigned => "ret",
            Self::Branch { .. } => "brif",
            Self::Exit { .. } => "exit",
            Self::Next => "next",
            Self::NextFile => "nextf",
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
            Self::Cnt => write!(f, "mem"),
            Self::UsVal => write!(f, "us"),
            Self::UaVal => write!(f, "ua"),
            Self::IsVal => write!(f, "is"),
            Self::IaVal => write!(f, "ia"),
        }
    }
}
