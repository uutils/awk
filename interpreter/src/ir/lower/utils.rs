// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{iter::repeat, mem::forget, ops::Deref};

use parser::{BuiltinFunction, Identifier, Variable};
use smallvec::SmallVec;

use crate::{
    CodeGen,
    ir::{Arg, ArgTy, BuiltInVar, PlaceTy, Reg, RegWidth},
    vm::types::Value,
};

#[derive(Clone, Debug, Default)]
pub struct RegAlloc {
    ranges: SmallVec<[(Reg, RegWidth); 8]>,
    pub(super) reg_pointer: RegWidth,
    pub hwm: RegWidth,
}

#[must_use]
#[derive(Debug)]
#[repr(transparent)]
pub struct LinearReg(Reg);

#[must_use]
#[derive(Debug)]
pub struct LinearRegRange(LinearReg, LinearReg);

#[derive(Clone, Copy)]
pub struct TypedArg(Arg, ArgTy);

#[derive(Clone, Copy)]
pub struct TypedPlace(Arg, PlaceTy);

#[derive(Clone, Copy)]
pub enum ResolvedPlace {
    Record(Reg),
    Variable,
    Index { arg: Arg, ty: PlaceTy, range: (Reg, Reg) },
}

#[must_use]
pub enum Operand {
    Imm(TypedArg),  // carries data inline
    Reg(LinearReg), // needs to be freed
}

impl LinearReg {
    pub const fn into_inner(self) -> Reg {
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
            code.regs.free(reg);
        }
    }
}

impl TypedArg {
    pub fn new_var(code: &mut CodeGen<'_>, var: &Variable<'_>) -> Self {
        TypedPlace::new_var(code, var).into()
    }

    pub const fn new_imm(imm: i32) -> Self {
        Self(Arg { imm }, ArgTy::Imm)
    }

    pub fn new_immf(code: &mut CodeGen<'_>, n: f64) -> Self {
        let cnt = code.register_const(Value::Float(n));
        Self(Arg { cnt }, ArgTy::Cnt)
    }

    pub fn new_cnt<'a>(code: &mut CodeGen<'a>, val: Value<'a>) -> Self {
        let cnt = code.register_const(val);
        Self(Arg { cnt }, ArgTy::Cnt)
    }

    pub fn new_reg(reg: impl Into<Reg>) -> Self {
        TypedPlace::new_reg(reg).into()
    }

    pub const fn as_reg(self) -> Option<Reg> {
        if matches!(self.1, ArgTy::Reg) {
            // SAFETY: has been type-checked.
            Some(unsafe { self.0.reg })
        } else {
            None
        }
    }
}

impl TypedPlace {
    pub fn new_var(code: &mut CodeGen<'_>, var: &Variable<'_>) -> Self {
        match BuiltInVar::try_from(var) {
            Ok(var) => Self::new_btin(var),
            Err(ident) => Self::new_user(code, ident),
        }
    }

    pub fn new_user(code: &mut CodeGen<'_>, ident: &Identifier<'_>) -> Self {
        let usr = code.symbols.register_user_var(ident, code.arena);
        if let Some(reg) = code.get_local_arg(usr) {
            Self(Arg { reg }, PlaceTy::Reg)
        } else {
            Self(Arg { usr }, PlaceTy::UserVal)
        }
    }

    pub const fn new_btin(var: BuiltInVar) -> Self {
        Self(Arg { sys: var }, PlaceTy::BtInVal)
    }

    pub fn new_reg(reg: impl Into<Reg>) -> Self {
        Self(Arg { reg: reg.into() }, PlaceTy::Reg)
    }

    pub fn as_reg(self) -> Option<Reg> {
        TypedArg::from(self).as_reg()
    }
}

impl RegAlloc {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates a single register.
    pub fn alloc(&mut self) -> LinearReg {
        if let Some((start, len)) = self.ranges.last_mut() {
            let reg = *start;
            if *len == 1 {
                self.ranges.pop();
            } else {
                start.0 += 1;
                *len -= 1;
            }
            return LinearReg(reg);
        }
        LinearReg(self.bump_alloc(1))
    }

    /// Allocates a contiguous register range. Mainly used for instructions that
    /// require passing a variable amount of values. Not to be used for user
    /// function calls for the reasons listed in [`Self::alloc_many_top`].
    pub fn alloc_many(&mut self, need: RegWidth) -> LinearRegRange {
        // Consider using `min_by_key` instead of `position` if there was enough
        // fragmentation caused by this to slow down the allocator.
        if let Some(i) = self.ranges.iter().position(|&(_, len)| len >= need) {
            let (start, len) = self.ranges[i];
            if len == need {
                self.ranges.remove(i);
            } else {
                self.ranges[i] = (Reg(start.0 + need), len - need);
            }
            return LinearRegRange(LinearReg(start), LinearReg(Reg(start.0 + need)));
        }
        let start = self.bump_alloc(need);
        LinearRegRange(LinearReg(start), LinearReg(Reg(start.0 + need)))
    }

    /// User function calls, at the moment, can clobber registers past the end.
    /// Mainly, because we don't pass their high-water mark to the allocator
    /// during lowering (just to the VM to reserve more registers), and because
    /// recursion exists. So, it's either this or worse shenanigans in the VM.
    pub fn alloc_many_top(&mut self, need: RegWidth) -> LinearRegRange {
        debug_assert!(
            !matches!(self.ranges.last(), Some(&(start, len)) if start.0 + len == self.reg_pointer),
            "a free range touched the high-water mark; free_n should have \
             retracted it instead of leaving it in the free list",
        );
        let start = self.bump_alloc(need);
        LinearRegRange(LinearReg(start), LinearReg(Reg(start.0 + need)))
    }

    /// Marks one register as freed.
    pub fn free(&mut self, reg: LinearReg) {
        self.free_n(reg.into_inner(), 1);
    }

    /// Frees a register range.
    pub fn free_many(&mut self, range: LinearRegRange) {
        let LinearRegRange(LinearReg(start), LinearReg(end)) = range;
        self.free_n(start, end.0 - start.0);
        forget(range);
    }

    /// Marks a range of registers as freed.
    fn free_n(&mut self, start: Reg, len: RegWidth) {
        if start.0 + len == self.reg_pointer {
            self.reg_pointer = start.0;
            if let Some(&(s, l)) = self.ranges.last()
                && s.0 + l == self.reg_pointer
            {
                self.reg_pointer = s.0;
                self.ranges.pop();
            }
            return;
        }
        let i = self.ranges.partition_point(|&(s, _)| s.0 < start.0);
        let merge_left = i > 0 && self.ranges[i - 1].0.0 + self.ranges[i - 1].1 == start.0;
        let merge_right = i < self.ranges.len() && start.0 + len == self.ranges[i].0.0;
        match (merge_left, merge_right) {
            (true, true) => {
                self.ranges[i - 1].1 += len + self.ranges[i].1;
                self.ranges.remove(i);
            }
            (true, false) => self.ranges[i - 1].1 += len,
            (false, true) => {
                self.ranges[i].0 = start;
                self.ranges[i].1 += len;
            }
            (false, false) => self.ranges.insert(i, (start, len)),
        }
    }

    /// Reserves some known registers before starting to allocate. Used in
    /// function lowering, primarily, to reserve argument passing.
    pub fn reserve(&mut self, n: RegWidth) {
        debug_assert!(self.ranges.is_empty());
        self.bump_alloc(n);
    }

    /// Allocates the next available register in width.
    fn bump_alloc(&mut self, n: RegWidth) -> Reg {
        let start = self.reg_pointer;
        // TODO: nice errors.
        self.reg_pointer = self.reg_pointer.checked_add(n).expect("register overflow");
        self.hwm = self.hwm.max(self.reg_pointer);
        Reg(start)
    }

    /// Runs the closure and restores the state of the allocator to a previous
    /// point, while still tracking high-water mark usage.
    pub fn scope<T>(self, code: &mut CodeGen, f: impl FnOnce(&mut CodeGen) -> T) -> T {
        let ret = f(code);

        code.regs.reg_pointer = self.reg_pointer;
        code.regs.ranges = self.ranges;

        ret
    }
}

impl LinearRegRange {
    #[inline(always)]
    pub fn as_range(&self) -> (Reg, Reg) {
        (*self.0, *self.1)
    }
}

impl From<Reg> for LinearReg {
    #[inline(always)]
    fn from(reg: Reg) -> Self {
        Self(reg)
    }
}

impl TypedArg {
    #[inline(always)]
    pub const fn into_arg(self) -> (Arg, ArgTy) {
        (self.0, self.1)
    }
}

impl TypedPlace {
    #[inline(always)]
    pub fn into_arg(self) -> (Arg, ArgTy) {
        (self.0, *self.1)
    }

    #[inline(always)]
    pub const fn into_place(self) -> (Arg, PlaceTy) {
        (self.0, self.1)
    }
}

impl From<TypedPlace> for TypedArg {
    #[inline(always)]
    fn from(value: TypedPlace) -> Self {
        Self(value.0, *value.1)
    }
}

impl Deref for LinearReg {
    type Target = Reg;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Poor man's linear types. The const fallback is a bit better, but has the
/// trade-off that you're effectively at the compiler's will.
#[cfg(debug_assertions)]
impl Drop for LinearReg {
    fn drop(&mut self) {
        debug_assert!(false, "Leaked register {}!", self.0);
    }
}

/// On release builds, we can rely on post-monomorphization errors to assert
/// linearity. Kinda neat. Fully cursed. Remove it if getting false positives,
/// but assert _they are_; the rt fallback not catching them is not an excuse.
#[cfg(not(debug_assertions))]
impl Drop for LinearReg {
    fn drop(&mut self) {
        fn evil<T>() {
            let _ = std::marker::PhantomData::<T>;
            const { panic!("Leaked register!") }
        }
        evil::<()>();
    }
}

impl From<&LinearReg> for Reg {
    fn from(value: &LinearReg) -> Self {
        **value
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub(super) enum RtType {
    Scalar,
    Array,
    Any,
}

// Thin newtype wrapper is dependency-injected an will only implement IntoIterator.
pub(super) struct CallConvGen(SmallVec<[RtType; 16]>);

pub(super) trait CallConv {
    fn convention(self, argc: RegWidth) -> impl Iterator<Item = RtType>;
}

impl CallConvGen {
    fn new(fun: BuiltinFunction, argc: RegWidth) -> Self {
        let argc = argc.into();
        match fun {
            BuiltinFunction::Length => Self(SmallVec::from_elem(RtType::Any, 1)),
            BuiltinFunction::Substr => Self(SmallVec::from_elem(RtType::Scalar, 3)),
            BuiltinFunction::Split => Self(SmallVec::from_slice(&[
                RtType::Scalar,
                RtType::Array,
                RtType::Scalar,
                RtType::Array,
            ])),
            BuiltinFunction::Sub => Self(SmallVec::from_elem(RtType::Scalar, 3)),
            BuiltinFunction::Gsub => Self(SmallVec::from_elem(RtType::Scalar, 3)),
            BuiltinFunction::Match => Self(SmallVec::from_slice(&[
                RtType::Scalar,
                RtType::Scalar,
                RtType::Array,
            ])),
            BuiltinFunction::Index => Self(SmallVec::from_elem(RtType::Scalar, 2)),
            BuiltinFunction::Sprintf => Self(SmallVec::from_elem(RtType::Scalar, argc)),
            BuiltinFunction::Toupper => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Tolower => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Gensub => Self(SmallVec::from_elem(RtType::Scalar, 4)),
            BuiltinFunction::Patsplit => Self(SmallVec::from_slice(&[
                RtType::Scalar,
                RtType::Array,
                RtType::Scalar,
                RtType::Array,
            ])),
            BuiltinFunction::Strtonum => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Close => Self(SmallVec::from_elem(RtType::Scalar, 2)),
            BuiltinFunction::Fflush => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::System => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Int => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Sqrt => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Exp => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Log => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Sin => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Cos => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Atan2 => Self(SmallVec::from_elem(RtType::Scalar, 2)),
            BuiltinFunction::Rand => Self(SmallVec::new()),
            BuiltinFunction::Srand => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Systime => Self(SmallVec::new()),
            BuiltinFunction::Mktime => Self(SmallVec::from_elem(RtType::Scalar, 2)),
            BuiltinFunction::Strftime => Self(SmallVec::from_elem(RtType::Scalar, 3)),
            BuiltinFunction::Typeof => Self(SmallVec::from_elem(RtType::Any, 1)),
            BuiltinFunction::Isarray => Self(SmallVec::from_elem(RtType::Any, 1)),
            BuiltinFunction::Asort => Self(SmallVec::from_slice(&[
                RtType::Array,
                RtType::Array,
                RtType::Scalar,
            ])),
            BuiltinFunction::Asorti => Self(SmallVec::from_slice(&[
                RtType::Array,
                RtType::Array,
                RtType::Scalar,
            ])),
            BuiltinFunction::And => Self(SmallVec::from_elem(RtType::Scalar, argc)),
            BuiltinFunction::Or => Self(SmallVec::from_elem(RtType::Scalar, argc)),
            BuiltinFunction::Xor => Self(SmallVec::from_elem(RtType::Scalar, argc)),
            BuiltinFunction::Compl => Self(SmallVec::from_elem(RtType::Scalar, 1)),
            BuiltinFunction::Lshift => Self(SmallVec::from_elem(RtType::Scalar, 2)),
            BuiltinFunction::Rshift => Self(SmallVec::from_elem(RtType::Scalar, 2)),
        }
    }
}

impl CallConv for BuiltinFunction {
    fn convention(self, argc: RegWidth) -> impl Iterator<Item = RtType> {
        // Unexpected args are passed as-is so the error that triggers is
        // always the nice one about arity, not unexpected typeck bs.
        CallConvGen::new(self, argc)
            .0
            .into_iter()
            .chain(repeat(RtType::Any))
    }
}

impl CallConv for RtType {
    fn convention(self, _argc: RegWidth) -> impl Iterator<Item = Self> {
        repeat(self)
    }
}
