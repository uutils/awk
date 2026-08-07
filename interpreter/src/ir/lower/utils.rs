// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{mem::forget, ops::Deref};

use parser::{BinaryOperator, Identifier, UnaryOperator, Variable};

use crate::{
    CodeGen, Instruction,
    ir::{Arg, ArgTy, IxWidth, NonLocal, Reg, RegWidth},
    vm::types::Value,
};

#[must_use]
#[derive(Debug)]
#[repr(transparent)]
pub struct LinearReg(Reg);

#[derive(Clone, Copy)]
pub struct TypedArg(Arg, ArgTy);

#[must_use]
pub enum Operand {
    Imm(TypedArg),  // carries data inline
    Reg(LinearReg), // needs to be freed
}

#[derive(Clone, Debug)]
pub struct RegsState {
    pub(super) reg_pointer: RegWidth,
    n_free_regs: usize,
}

impl LinearReg {
    pub fn into_inner(self) -> Reg {
        let inner = self.0;
        forget(self);
        inner
    }
}

impl Operand {
    pub fn to_arg(&self) -> TypedArg {
        match self {
            &Self::Imm(imm) => imm,
            Self::Reg(reg) => TypedArg::new_reg(reg),
        }
    }

    pub fn free(self, code: &mut CodeGen) {
        if let Self::Reg(reg) = self {
            code.free_reg(reg);
        }
    }
}

impl TypedArg {
    pub fn new_us(code: &mut CodeGen<'_>, ident: &Identifier<'_>) -> Self {
        let sym = code.symbols.register_user_var(ident, code.arena);
        if let Some(reg) = code.get_local_arg(sym) {
            Self(Arg { reg }, ArgTy::Reg)
        } else {
            Self(Arg { sym }, ArgTy::UsVal)
        }
    }

    pub fn new_is(var: &Variable<'_>) -> Self {
        Self(Arg { sym: var_index(var) }, ArgTy::IsVal)
    }

    pub fn new_ia(var: &Variable<'_>) -> Self {
        let sym = var_index(var);
        Self(Arg { sym }, ArgTy::IaVal)
    }

    pub fn new_ua(code: &mut CodeGen<'_>, ident: &Identifier<'_>) -> Self {
        let sym = code.symbols.register_user_var(ident, code.arena);
        if let Some(reg) = code.get_local_arg(sym) {
            Self(Arg { reg }, ArgTy::Reg)
        } else {
            Self(Arg { sym }, ArgTy::UaVal)
        }
    }

    pub fn new_imm(imm: i32) -> Self {
        Self(Arg { imm }, ArgTy::Imm)
    }

    pub fn new_immf(code: &mut CodeGen<'_>, n: f64) -> Self {
        let sym = code.register_const(Value::Float(n));
        Self(Arg { sym }, ArgTy::ImmF)
    }

    pub fn new_cnt<'a>(code: &mut CodeGen<'a>, val: Value<'a>) -> Self {
        let sym = code.register_const(val);
        Self(Arg { sym }, ArgTy::Cnt)
    }

    pub fn new_reg(reg: impl Into<Reg>) -> Self {
        Self(Arg { reg: reg.into() }, ArgTy::Reg)
    }

    pub fn as_reg(self) -> Option<Reg> {
        if matches!(self.1, ArgTy::Reg) {
            // SAFETY: has been type-checked.
            Some(unsafe { self.0.reg })
        } else {
            None
        }
    }
}

impl RegsState {
    pub fn new(code: &CodeGen) -> Self {
        Self {
            reg_pointer: code.reg_pointer,
            n_free_regs: code.free_regs.len(),
        }
    }

    pub fn scope<T>(self, code: &mut CodeGen, f: impl FnOnce(&mut CodeGen) -> T) -> (Self, T) {
        let ret = f(code);
        let old = code.reg_pointer;
        code.reg_pointer = self.reg_pointer;
        code.free_regs.truncate(self.n_free_regs);
        (Self { reg_pointer: old, ..self }, ret)
    }

    pub fn scope_hwm<T>(self, code: &mut CodeGen, f: impl FnOnce(&mut CodeGen) -> T) {
        f(code);
        code.reg_pointer = code.reg_pointer.max(self.reg_pointer);
        code.free_regs.truncate(self.n_free_regs);
    }
}

impl Instruction {
    pub(super) fn from_unary(op: UnaryOperator, dest: Reg, arg: TypedArg) -> Self {
        let (arg, ty) = arg.into();
        match op {
            UnaryOperator::Record => Self::Record { dest, arg, ty },
            UnaryOperator::Negation => Self::Negation { dest, arg, ty },
            UnaryOperator::ToInt => Self::ToInt { dest, arg, ty },
            UnaryOperator::Negative => Self::Negative { dest, arg, ty },
        }
    }

    pub(super) fn from_binary(op: BinaryOperator, dest: Reg, lhs: TypedArg, rhs: TypedArg) -> Self {
        let ((lhs, tyl), (rhs, tyr)) = (lhs.into(), rhs.into());
        match op {
            BinaryOperator::Concat => Self::Concat { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Eq => Self::Eq { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::NEq => Self::NEq { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Gt => Self::Gt { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Lt => Self::Lt { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::LtE => Self::LtE { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::GtE => Self::GtE { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::And | BinaryOperator::Or => {
                unreachable!("&& and || are lowered with branches")
            }
            BinaryOperator::Matches => Self::Matches { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::MatchesNot => Self::MatchesNot { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Add => Self::Add { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Subtract => Self::Subtract { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Multiply => Self::Multiply { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Divide => Self::Divide { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Raise => Self::Raise { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Modulo => Self::Modulo { dest, lhs, rhs, tyl, tyr },
        }
    }
}

impl From<Reg> for LinearReg {
    fn from(reg: Reg) -> Self {
        Self(reg)
    }
}

impl From<TypedArg> for (Arg, ArgTy) {
    fn from(value: TypedArg) -> Self {
        (value.0, value.1)
    }
}

impl Deref for LinearReg {
    type Target = Reg;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub fn var_index(var: &Variable<'_>) -> NonLocal {
    const { assert!(size_of::<(IxWidth, Identifier<'_>)>() == size_of::<Variable>()) }
    const { assert!(align_of::<(IxWidth, Identifier<'_>)>() == align_of::<Variable>()) }

    // SAFETY: The discriminant is repr(IxWidth).
    let index = unsafe { *(&raw const *var).cast::<IxWidth>() };
    debug_assert_ne!(index, 0); // User variable.

    NonLocal(index)
}

#[cfg(debug_assertions)]
impl Drop for LinearReg {
    fn drop(&mut self) {
        debug_assert!(false, "Leaked register {}!", self.0);
    }
}

impl From<&LinearReg> for Reg {
    fn from(value: &LinearReg) -> Self {
        **value
    }
}
