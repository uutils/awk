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

use derive_more::Display;
use parser::{BuiltinFunction, Command, Identifier, Redirection, Variable};

pub type RegWidth = u8;
pub type IxWidth = u32;

#[derive(Clone, Copy, Debug, Display, PartialEq, Eq)]
#[repr(transparent)]
pub struct UserNonLocal(pub IxWidth);

#[derive(Clone, Copy, Debug, Display, PartialEq, Eq)]
#[repr(transparent)]
pub struct ConstNonLocal(pub IxWidth);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Reg(pub RegWidth);

#[derive(Clone, Copy, Display, Debug)]
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
    StoreS { dest: Reg, ty_place: PlaceTy, var: Arg, arg: Arg, ty: ArgTy },
    StoreF { dest: Reg, src: Arg, arg: Arg, ty: ArgTy, tys: ArgTy },
    Insert { dest: Reg, lhs: Arg, rhs: Arg, start: Reg, end: Reg, tyl: PlaceTy, tyr: ArgTy },
    IndexS { dest: Reg, arg: Arg, start: Reg, end: Reg, ty: PlaceTy },
    InA { dest: Reg, arg: Arg, start: Reg, end: Reg, ty: PlaceTy },
    IndexA { dest: Reg, arg: Arg, start: Reg, end: Reg, ty: PlaceTy },
    DeleteP { arg: Arg, start: Reg, end: Reg, ty: PlaceTy },
    ConcatMany { dest: Reg, start: Reg, end: Reg },
    IntrinsicCall { dest: Reg, start: Reg, end: Reg, fun: BuiltinFunction },
    OutputCall { start: Reg, end: Reg, cmd: Command, redir: Option<Redirection> },
    UserCall { dest: Reg, start: Reg, end: Reg, name: UserNonLocal },
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

/// And you might ask, why are we doing a tagged union instead of an enum?
/// It's actually quite trivial, but a bit unfortunate. The problem is that the
/// packing in [`Instruction`] is otherwise not ideal; because the compiler has
/// to ensure we can _always_ take a reference to one of the fields, it can't
/// split the discriminants to pack it tightly, and we end up with big payloads
/// full of padding. Might be solved in the future with move-only fields.
#[derive(Clone, Copy)]
pub union Arg {
    pub reg: Reg,
    pub imm: i32,
    pub cnt: ConstNonLocal,
    pub usr: UserNonLocal,
    pub sys: BuiltInVar,
}

/// The discriminant of an [`Arg`] union. The fields are ordered the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ArgTy {
    Reg,
    Imm,
    Cnt,
    UserVal,
    BtInVal,
}

/// A subset of [`Arg`] values.
#[derive(Clone, Copy)]
#[repr(u8)]
pub enum PlaceTy {
    Reg = ArgTy::Reg as u8,
    UserVal = ArgTy::UserVal as u8,
    BtInVal = ArgTy::BtInVal as u8,
}

const _: () = {
    assert!(size_of::<PlaceTy>() == size_of::<ArgTy>());
    assert!(align_of::<PlaceTy>() == align_of::<ArgTy>());
};

/// A bit dirty, but helps LLVM in not generating a ton of garbage.
const fn var_n(variable: Variable) -> u32 {
    // SAFETY: `Variable` is repr(u32).
    unsafe { std::ptr::read((&raw const variable).cast()) }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Display, PartialEq, Eq)]
#[display(rename_all = "UPPERCASE")]
pub enum BuiltInVar {
    Nr = var_n(Variable::Nr),
    Nf = var_n(Variable::Nf),
    Fs = var_n(Variable::Fs),
    Rs = var_n(Variable::Rs),
    Ofs = var_n(Variable::Ofs),
    Ors = var_n(Variable::Ors),
    Filename = var_n(Variable::Filename),
    Argc = var_n(Variable::Argc),
    Argv = var_n(Variable::Argv),
    Subsep = var_n(Variable::Subsep),
    Fnr = var_n(Variable::Fnr),
    Argind = var_n(Variable::Argind),
    Ofmt = var_n(Variable::Ofmt),
    Rstart = var_n(Variable::Rstart),
    Rlength = var_n(Variable::Rlength),
    Environ = var_n(Variable::Environ),
    Symtab = var_n(Variable::Symtab),
    Functab = var_n(Variable::Functab),
    Procinfo = var_n(Variable::Procinfo),
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
            PlaceTy::UserVal => Self::UserVal,
            PlaceTy::BtInVal => Self::BtInVal,
        }
    }
}

impl<'a, 'r> TryFrom<&'r Variable<'a>> for BuiltInVar {
    type Error = &'r Identifier<'a>;

    fn try_from(value: &'r Variable<'a>) -> Result<Self, Self::Error> {
        match value {
            Variable::User(ident) => Err(ident),
            Variable::Nr => Ok(Self::Nr),
            Variable::Nf => Ok(Self::Nf),
            Variable::Fs => Ok(Self::Fs),
            Variable::Rs => Ok(Self::Rs),
            Variable::Ofs => Ok(Self::Ofs),
            Variable::Ors => Ok(Self::Ors),
            Variable::Filename => Ok(Self::Filename),
            Variable::Argc => Ok(Self::Argc),
            Variable::Argv => Ok(Self::Argv),
            Variable::Subsep => Ok(Self::Subsep),
            Variable::Fnr => Ok(Self::Fnr),
            Variable::Argind => Ok(Self::Argind),
            Variable::Ofmt => Ok(Self::Ofmt),
            Variable::Rstart => Ok(Self::Rstart),
            Variable::Rlength => Ok(Self::Rlength),
            Variable::Environ => Ok(Self::Environ),
            Variable::Symtab => Ok(Self::Symtab),
            Variable::Functab => Ok(Self::Functab),
            Variable::Procinfo => Ok(Self::Procinfo),
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
            ArgTy::UserVal => Ok(Self::UserVal),
            ArgTy::BtInVal => Ok(Self::BtInVal),
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
            ArgTy::Cnt => write!(f, "{sep}{ty}({})", unsafe { arg.cnt }),
            ArgTy::UserVal => write!(f, "{sep}{ty}({})", unsafe { arg.usr }),
            ArgTy::BtInVal => write!(f, "{sep}{ty}({})", unsafe { arg.sys }),
        };
        match self {
            Self::LoadF { dest, arg, ty }
            | Self::Negation { dest, arg, ty }
            | Self::ToInt { dest, arg, ty }
            | Self::Negative { dest, arg, ty }
            | Self::CopyS { dest, arg, ty }
            | Self::CopyP { dest, arg, ty } => {
                write!(f, "{op} {dest}")?;
                fmt_arg(f, arg, ty, ", ")
            }
            Self::CopyA { dest, arg, ty } => {
                write!(f, "{op} {dest}")?;
                fmt_arg(f, arg, ty, ", ")
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
                write!(f, "{op} {dest}")?;
                fmt_arg(f, lhs, tyl, ", ")?;
                fmt_arg(f, rhs, tyr, ", ")
            }
            Self::In { dest, lhs, rhs, tyr, tyl } => {
                write!(f, "{op} {dest}")?;
                fmt_arg(f, lhs, tyl, ", ")?;
                fmt_arg(f, rhs, tyr, ", ")
            }
            Self::StoreS { dest, ty_place, var, arg, ty } => {
                let ty_place = ArgTy::from(*ty_place);
                write!(f, "{op} {dest}, {ty_place}(")?;
                fmt_arg(f, var, ty, "")?;
                fmt_arg(f, arg, ty, "), ")
            }
            Self::StoreF { dest, src, arg, ty, tys } => {
                write!(f, "{op} {dest}, $(")?;
                fmt_arg(f, src, tys, "")?;
                fmt_arg(f, arg, ty, "), ")
            }
            Self::Insert { dest, lhs, rhs, start, end, tyl, tyr } => {
                write!(f, "{op} {dest}")?;
                fmt_arg(f, lhs, tyl, ", ")?;
                write!(f, ", {start}..{end}")?;
                fmt_arg(f, rhs, tyr, ", ")
            }
            Self::IndexS { dest, arg, start, end, ty }
            | Self::IndexA { dest, arg, start, end, ty }
            | Self::InA { dest, arg, start, end, ty } => {
                write!(f, "{op} {dest}")?;
                fmt_arg(f, arg, ty, ", ")?;
                write!(f, ", {start}..{end}")
            }
            Self::DeleteP { arg, start, end, ty } => {
                write!(f, "{op}")?;
                fmt_arg(f, arg, ty, ", ")?;
                write!(f, ", {start}..{end}")
            }
            Self::IncrementPost { dest, arg, ty }
            | Self::IncrementPre { dest, arg, ty }
            | Self::DecrementPost { dest, arg, ty }
            | Self::DecrementPre { dest, arg, ty } => {
                write!(f, "{op} {dest}")?;
                fmt_arg(f, arg, ty, ", ")
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
            Self::ConcatMany { dest, start, end } => {
                write!(f, "{op} {dest}, {start}..{end}")
            }
            Self::IntrinsicCall { dest, start, end, fun } => {
                write!(f, "{op} {dest}, {fun}, {start}..{end}")
            }
            Self::IndirectCall { dest, start, end, name, ty } => {
                write!(f, "{op} {dest}")?;
                fmt_arg(f, name, ty, ", ")?;
                write!(f, ", {start}..{end}")
            }
            Self::OutputCall { start, end, cmd, redir: Some(redir) } => {
                write!(f, "{cmd}{redir:?} {start}..{end}")
            }
            Self::OutputCall { start, end, cmd, redir: None } => {
                write!(f, "{cmd} {start}..{end}")
            }
            Self::UserCall { dest, start, end, name } => {
                write!(f, "{op} {dest}, {name}, {start}..{end}")
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
            Self::LtE { .. } => "lte",
            Self::GtE { .. } => "gte",
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
            Self::Insert { .. } => "insert",
            Self::IndexS { .. } => "sindex",
            Self::IndexA { .. } => "aindex",
            Self::In { .. } => "in",
            Self::InA { .. } => "ain",
            Self::CopyS { .. } => "scpy",
            Self::CopyA { .. } => "acpy",
            Self::CopyP { .. } => "pcpy",
            Self::DeleteA { .. } => "adel",
            Self::DeleteP { .. } => "pdel",
            Self::ConcatMany { .. } => "catv",
            Self::IntrinsicCall { .. } => "bcall",
            Self::UserCall { .. } => "ucall",
            Self::IndirectCall { .. } => "icall",
            Self::OutputCall { .. } => "out",
            Self::Jump { .. } => "jmp",
            Self::Return { .. } => "retval",
            Self::ReturnUnassigned => "ret",
            Self::Branch { .. } => "brif",
            Self::Exit { .. } => "exit",
            Self::Next => "next",
            Self::NextFile => "nextf",
        }
    }
}

impl Display for Reg {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

impl Display for ArgTy {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reg => write!(f, "r"),
            Self::Imm => write!(f, "imm"),
            Self::Cnt => write!(f, "cnt"),
            Self::UserVal => write!(f, "user"),
            Self::BtInVal => write!(f, "btin"),
        }
    }
}
