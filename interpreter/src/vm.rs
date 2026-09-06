// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! The cornucopia of `inline` attrs is on purpose. I have measured it to 2x
//! performance quite easily. If we switch to a direct, tail-call dispatch,
//! which we should at least _try_[^ref], reassess it during the refactoring.
//!
//! [^ref]: <https://lordgoati.us/blog/tail-call/>

#![allow(clippy::inline_always)]

pub mod io;
mod regex;
mod symbols;
pub mod types;

use std::{
    fmt::{self, Display},
    io::Result as IoResult,
    mem::MaybeUninit,
    ops::Range,
    vec::Vec as StdVec,
};

use bumpalo::{Bump, collections::Vec};
use parser::{AriadneSpan, Command, Identifier, MetaId, MetadataStore, Redirection};
use smallvec::SmallVec;
pub use symbols::SymbolTable;

use crate::{
    InterpreterError,
    ir::{
        Arg, ArgTy, BuiltInVar, Instruction, IxWidth, Label, PlaceTy, Reg, RegWidth, UserNonLocal,
        lower::{Bytecode, CodeGen},
    },
    vm::{
        io::{FilePath, IoRequest, IoResponse},
        symbols::Record,
        types::Value,
    },
};

pub type Result<T, E = InterpreterError> = std::result::Result<T, E>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecMode {
    Uu,
    Gnu,
    Posix,
}

// TODO struct ReentrantPoint that contains PC, code_end, frames and regs.
pub struct Interpreter<'a> {
    program_counter: IxWidth,
    code_end: IxWidth,
    registers: Registers<'a>,
    pub(crate) symbols: SymbolTable<'a>,
    pub(crate) record: Record,
    consts: Consts<'a>,
    mode: ExecMode,
    frames: StdVec<CallFrame>,
    metadata: MetadataStore<AriadneSpan>,
}

pub struct CallFrame {
    reg_offset: IxWidth,
    ret_addr: IxWidth,
    prev_code_end: IxWidth,
    ret_dest: Reg,
}

#[derive(Debug)]
pub enum Signal {
    Suspend(IoRequest),
    Terminal(CtrlSig),
}

#[derive(Debug)]
pub enum CtrlSig {
    End,
    Next,
    NextFile,
    Exit(i32),
}

#[derive(Debug)]
pub struct Registers<'a>(Vec<'a, Value<'a>>);

#[derive(Debug)]
pub struct Function {
    pub arity: RegWidth,
    pub hwm_regs: RegWidth,
    pub code: CodeRange,
}

#[derive(Debug)]
pub struct Consts<'a>(pub(crate) Vec<'a, Value<'a>>);

#[derive(Debug, Clone)]
pub struct CodeRange(pub(crate) Range<IxWidth>);

/// Newtype of `(Arg, PlaceTy)`.
#[derive(Clone, Copy)]
struct Place {
    arg: Arg,
    ty: PlaceTy,
}

impl<'a> Interpreter<'a> {
    pub fn new(mode: ExecMode, code: CodeGen<'a>, metadata: MetadataStore<AriadneSpan>) -> Self {
        let n_regs = code.regs.hwm as usize + 1;
        Self {
            program_counter: 0,
            code_end: 0,
            registers: Registers(bumpalo::vec![in code.arena; Value::Untyped; n_regs + 1]),
            symbols: code.symbols,
            record: Record::new(),
            consts: code.consts,
            mode,
            frames: StdVec::new(),
            metadata,
        }
    }
}

impl<'a> Consts<'a> {
    pub fn new_in(arena: &'a Bump) -> Self {
        Self(Vec::new_in(arena))
    }
}

impl<'a> Interpreter<'a> {
    #[inline(always)]
    pub fn run_code(&mut self, bc: &Bytecode, span: CodeRange) -> Result<Signal> {
        self.program_counter = span.0.start;
        self.code_end = span.0.end;

        self.run_chunk(&bc.code, &bc.metadata)
    }

    fn run_chunk(&mut self, bytecode: &[Instruction], metadata: &[MetaId]) -> Result<Signal> {
        while let Some(&instr) = bytecode.get(self.program_counter as usize)
            && self.program_counter < self.code_end
        {
            match instr {
                Instruction::LoadF { dest, arg, ty } => {
                    let place = self.get_val(arg, ty, metadata, Value::to_int)?;
                    let place = usize::try_from(place).unwrap(); // TODO: error handle
                    let val = match self.record.get_val(place, &mut self.symbols, self.mode) {
                        Ok(val) => val,
                        Err(e) => return Err(InterpreterError::Regex(self.get_span(metadata), e)),
                    };
                    self.write_reg(dest, val);
                }
                Instruction::Negation { dest, arg, ty } => {
                    let val = self.get_val(arg, ty, metadata, Value::to_bool)?;
                    self.write_reg(dest, !val);
                }
                Instruction::ToInt { dest, arg, ty } => {
                    let val = self.get_val(arg, ty, metadata, Value::to_num)?;
                    self.write_reg(dest, val);
                }
                Instruction::Negative { dest, arg, ty } => {
                    let val = self.get_val(arg, ty, metadata, Value::to_num)?;
                    self.write_reg(dest, -val);
                }
                Instruction::IncrementPost { dest, arg, ty }
                | Instruction::IncrementPre { dest, arg, ty }
                | Instruction::DecrementPost { dest, arg, ty }
                | Instruction::DecrementPre { dest, arg, ty } => {
                    let rhs = &Value::Int(match instr {
                        Instruction::IncrementPost { .. } | Instruction::IncrementPre { .. } => 1,
                        _ => -1,
                    });
                    let is_post = matches!(
                        instr,
                        Instruction::IncrementPost { .. } | Instruction::DecrementPost { .. }
                    );

                    let (new_val, observed) = self.get_val(arg, *ty, metadata, |old_val| {
                        let new_val = rhs + old_val;
                        let observed = if is_post {
                            &Value::Int(0) + old_val
                        } else {
                            new_val.clone()
                        };
                        (new_val, observed)
                    })?;
                    Place::new(arg, ty).write(self, new_val);
                    self.write_reg(dest, observed);
                }
                Instruction::CopyS { dest, arg, ty } => {
                    let val = self.get_val(arg, ty, metadata, Value::clone)?;
                    self.write_reg(dest, val);
                }
                Instruction::CopyA { dest, arg, ty } => {
                    let val = self.get_array(arg, ty, metadata, Value::clone)?;
                    self.write_reg(dest, val);
                }
                Instruction::CopyP { dest, arg, ty } => {
                    let val = arg.get_pure(ty, self, &mut MaybeUninit::uninit()).clone();
                    self.write_reg(dest, val);
                }
                Instruction::Eq { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs == rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::NEq { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs != rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::Gt { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs > rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::Lt { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs < rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::LtE { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs <= rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::GtE { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs >= rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::Matches { dest, lhs, rhs, tyl, tyr } => {
                    let mode = self.mode;
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| {
                        lhs.matches_regex(rhs, mode)
                    })?;

                    self.write_reg(dest, val);
                }
                Instruction::MatchesNot { dest, lhs, rhs, tyl, tyr } => {
                    let mode = self.mode;
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| {
                        !lhs.matches_regex(rhs, mode)
                    })?;

                    self.write_reg(dest, val);
                }
                Instruction::Add { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs + rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::Subtract { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs - rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::Multiply { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs * rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::Divide { dest, lhs, rhs, tyl, tyr } => {
                    let Some(val) =
                        self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs / rhs)?
                    else {
                        return Err(InterpreterError::DivByZeroAttempted(
                            self.get_span(metadata),
                            self.get_val(lhs, tyl, metadata, Value::to_string)?,
                        ));
                    };

                    self.write_reg(dest, val);
                }
                Instruction::Raise { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs ^ rhs)?;
                    self.write_reg(dest, val);
                }
                Instruction::Modulo { dest, lhs, rhs, tyl, tyr } => {
                    let Some(val) =
                        self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| lhs % rhs)?
                    else {
                        return Err(InterpreterError::DivByZeroAttempted(
                            self.get_span(metadata),
                            self.get_val(lhs, tyl, metadata, Value::to_string)?,
                        ));
                    };

                    self.write_reg(dest, val);
                }
                Instruction::Concat { dest, lhs, rhs, tyl, tyr } => {
                    let val = self.get_val2(lhs, tyl, rhs, tyr, metadata, |lhs, rhs| {
                        let mut buf =
                            StdVec::with_capacity(lhs.string_size_hint() + rhs.string_size_hint());
                        lhs.write_string(&mut buf);
                        rhs.write_string(&mut buf);
                        buf
                    })?;

                    self.write_reg(dest, val);
                }
                Instruction::IndexS { dest, arg, start, end, ty } => {
                    let key = self.make_array_key(start, end);
                    let place = Place::new(arg, ty);
                    let val = self.array_op(place, metadata, |arr| arr.get_array_elem(&key))?;

                    self.write_reg(dest, val);
                }
                Instruction::IndexA { dest, arg, start, end, ty } => {
                    let key = self.make_array_key(start, end);
                    let place = Place::new(arg, ty);
                    let val = self.array_op(place, metadata, |arr| arr.array_elem_aoa(key))?;

                    self.write_reg(dest, val);
                }
                Instruction::StoreS { dest, ty_place, var, arg, ty } => {
                    debug_assert!(matches!(ty_place, PlaceTy::UserVal | PlaceTy::BtInVal));
                    let val = self.get_val(arg, ty, metadata, Value::clone)?;

                    Place::new(var, ty_place).write(self, val.clone());
                    self.write_reg(dest, val);
                }
                Instruction::StoreF { dest, src, arg, ty, tys } => {
                    let place = self.get_val(src, tys, metadata, Value::to_int)?;
                    let place = usize::try_from(place).unwrap(); // TODO: error handle
                    let val = self.get_val(arg, ty, metadata, Value::clone)?;

                    self.record
                        .write_field(val.clone(), place, &mut self.symbols, self.mode)
                        .map_err(|e| InterpreterError::Regex(self.get_span(metadata), e))?;

                    self.write_reg(dest, val);
                }
                Instruction::Insert { dest, lhs, rhs, start, end, tyl, tyr } => {
                    let key = self.make_array_key(start, end);
                    let val = self.get_val(rhs, tyr, metadata, Value::clone)?;
                    let place = Place::new(lhs, tyl);

                    self.array_op(place, metadata, |arr| arr.array_insert(key, val.clone()))?;
                    self.write_reg(dest, val);
                }
                Instruction::DeleteA { arg, ty } => {
                    // Remember typedness
                    self.array_op(Place::new(arg, ty), metadata, Value::reset_array)?;
                }
                Instruction::DeleteP { arg, ty, start, end } => {
                    let key = self.make_array_key(start, end);
                    // Forget typedness
                    self.array_op(Place::new(arg, ty), metadata, |arr| arr.array_remove(&key))?;
                }
                Instruction::In { dest, lhs, rhs, tyr, tyl } => {
                    let key = self.get_val(rhs, tyr, metadata, Value::to_string)?;
                    let place = Place::new(lhs, tyl);
                    let val = self.array_op(place, metadata, |arr| arr.has_array_elem(&key))?;

                    self.write_reg(dest, val);
                }
                Instruction::InA { dest, arg, start, end, ty } => {
                    let key = self.make_array_key(start, end);
                    let place = Place::new(arg, ty);
                    let val = self.array_op(place, metadata, |arr| arr.has_array_elem(&key))?;

                    self.write_reg(dest, val);
                }
                Instruction::ConcatMany { dest, start, end } => {
                    let offset = self.reg_offset();
                    let args = self.registers.get_range(start..end, offset);
                    let mut buf = StdVec::with_capacity(4 * args.len()); // Heuristic

                    for arg in args {
                        arg.write_string(&mut buf);
                    }
                    self.write_reg(dest, Value::String(buf.into()));
                }
                Instruction::IntrinsicCall { dest, start, end, fun } => {
                    let offset = self.reg_offset();
                    let args = self.registers.get_range(start..end, offset);

                    match self.call_builtin(fun, args) {
                        Ok(val) => self.write_reg(dest, val),
                        Err(err) => {
                            return Err(err.into_interpreter_error(self.get_span(metadata)));
                        }
                    }
                }
                Instruction::OutputCall { start, end, cmd, redir } => {
                    return Ok(Signal::Suspend(self.print_req(start, end, cmd, redir)));
                }
                Instruction::UserCall { dest, start, end, name } => {
                    self.user_call(dest, start, end, name, metadata)?;
                    continue;
                }
                Instruction::IndirectCall { dest, start, end, name, ty } => {
                    let name = self.get_val(name, ty, metadata, Value::to_string)?;
                    // TODO: Proper parsing, catch indirect calls to built-ins,
                    //       native funs.
                    let (namespace, literal) = name.split_once("::").unwrap_or(("awk", &name));
                    let Some((name, _)) = self
                        .symbols
                        .functions
                        .lookup(&Identifier { namespace, literal })
                    else {
                        return Err(InterpreterError::UnknownIndFunction(
                            self.get_span(metadata),
                            name,
                        ));
                    };

                    self.user_call(dest, start, end, name, metadata)?;
                    continue;
                }
                Instruction::Jump { to: Label(label) } => {
                    self.program_counter = label;
                    continue;
                }
                Instruction::Branch { then_label, else_label, condition } => {
                    if self.read_reg(condition).to_bool() {
                        self.program_counter = then_label.0;
                    } else {
                        self.program_counter = else_label.0;
                    }
                    continue;
                }
                Instruction::Exit { arg, ty } => {
                    let val = self.get_val(arg, ty, metadata, Value::to_int)?;
                    return Ok(Signal::Terminal(CtrlSig::Exit(val as i32)));
                }
                Instruction::Return { arg, ty } => {
                    let val = self.get_val(arg, ty, metadata, Value::clone)?;
                    self.ret(val);
                    continue;
                }
                Instruction::ReturnUnassigned => {
                    self.ret(Value::Unassigned);
                    continue;
                }
                Instruction::Next => return Ok(Signal::Terminal(CtrlSig::Next)),
                Instruction::NextFile => return Ok(Signal::Terminal(CtrlSig::NextFile)),
            }
            self.program_counter += 1;
        }
        Ok(Signal::Terminal(CtrlSig::End))
    }

    /// Convenience wrapper to add errors from context metadata.
    #[inline(always)]
    fn get_val<T>(
        &mut self,
        arg: Arg,
        ty: ArgTy,
        metadata: &[MetaId],
        f: impl FnOnce(&Value<'a>) -> T,
    ) -> Result<T> {
        match arg.get_val(ty, self, &mut MaybeUninit::uninit()) {
            Some(x) => Ok(f(x)),
            None => Err(InterpreterError::ScalarUseOfArrary(self.get_span(metadata))),
        }
    }

    /// Convenience wrapper to add errors from context metadata.
    #[inline(always)]
    fn get_val2<T>(
        &mut self,
        lhs: Arg,
        tyl: ArgTy,
        rhs: Arg,
        tyr: ArgTy,
        metadata: &[MetaId],
        f: impl FnOnce(&Value<'a>, &Value<'a>) -> T,
    ) -> Result<T> {
        match Arg::get_val2(lhs, tyl, rhs, tyr, self, f) {
            Some(x) => Ok(x),
            None => Err(InterpreterError::ScalarUseOfArrary(self.get_span(metadata))),
        }
    }

    /// Reads and triggers side-effects of built-in variables on read.
    pub fn sync_nf_read(&mut self) -> Value<'a> {
        // Materialize fields if record is unsplit
        // TODO: refactor so there are no errors at this point
        self.record.nf(&mut self.symbols, self.mode).unwrap()
    }

    /// Writes and triggers side-effects of built-in variables on write.
    pub fn sync_btin_write(&mut self, sys: BuiltInVar, val: Value<'a>) {
        // Apply side effects before writing the value
        match sys {
            BuiltInVar::Nf => {
                // TODO: errors
                let n = usize::try_from(val.to_int()).unwrap();
                let _ = self.record.resize(n, &mut self.symbols, self.mode);
                return; // auto-updated
            }
            BuiltInVar::Fs | BuiltInVar::Ofs => self.record.invalidate(),
            _ => {}
        }
        *self.symbols.get_btin_mut(sys) = val;
    }

    fn array_op<T>(
        &mut self,
        place: Place,
        metadata: &[MetaId],
        f: impl FnOnce(&mut Value<'a>) -> Option<T>,
    ) -> Result<T> {
        place
            .array(self)
            .and_then(f)
            .ok_or_else(|| InterpreterError::ArrayUseOfScalar(self.get_span(metadata)))
    }

    /// Convenience wrapper to add errors from context metadata.
    fn get_array<T>(
        &mut self,
        arg: Arg,
        ty: PlaceTy,
        metadata: &[MetaId],
        f: impl FnOnce(&Value<'a>) -> T,
    ) -> Result<T> {
        match Place::new(arg, ty).array(self) {
            Some(val) => Ok(f(val)),
            None => Err(InterpreterError::ArrayUseOfScalar(self.get_span(metadata))),
        }
    }

    /// Convenience wrapper to write a value at the current reg slice.
    #[inline(always)]
    fn write_reg(&mut self, dest: Reg, val: impl Into<Value<'a>>) {
        self.registers.write(dest, self.reg_offset(), val);
    }

    /// Convenience wrapper to read a value from the current reg slice.
    #[inline(always)]
    fn read_reg(&self, src: Reg) -> &Value<'a> {
        self.registers.get(src, self.reg_offset())
    }

    /// Convenience wrapper to read a value from the current reg slice.
    #[inline(always)]
    fn read_reg_mut(&mut self, src: Reg) -> &mut Value<'a> {
        self.registers.get_mut(src, self.reg_offset())
    }

    fn ret(&mut self, val: Value<'a>) {
        let Some(CallFrame { reg_offset: _, ret_addr, prev_code_end, ret_dest }) =
            self.frames.pop()
        else {
            unreachable!()
        };
        self.write_reg(ret_dest, val);
        self.program_counter = ret_addr;
        self.code_end = prev_code_end;
    }

    #[inline(always)]
    fn reg_offset(&self) -> IxWidth {
        self.frames.last().map_or(0, |frame| frame.reg_offset)
    }

    /// Resumes execution from a suspend/yield point. Receives the request
    /// since we might need to uniquely identify it (with pipes, for instance).
    /// Also takes the response in a [`io::Result`] because AWK has error
    /// recovery mechanisms (ERRNO variable, etc.).
    ///
    /// Allows us to trivially drive multiple code blocks concurrently.
    pub fn resume(
        &mut self,
        bytecode: &Bytecode,
        _req: IoRequest,
        _res: IoResult<IoResponse>,
    ) -> IoResult<Result<Signal>> {
        self.program_counter += 1;
        Ok(self.run_chunk(&bytecode.code, &bytecode.metadata))
    }

    fn print_req(
        &mut self,
        start: Reg,
        end: Reg,
        fun: Command,
        redir: Option<Redirection>,
    ) -> IoRequest {
        let Command::Print = fun else { todo!() };
        let None = redir else { todo!() };
        let mut buf = StdVec::with_capacity(64);
        let range = self.registers.get_range(start..end, self.reg_offset());

        if range.is_empty() {
            buf.extend_from_slice(self.record.raw());
        } else {
            let mut ofs = SmallVec::<[u8; 16]>::new_const();
            self.symbols.ofs.write_string(&mut ofs);

            let mut range = range.iter();
            if let Some(reg) = range.next() {
                reg.write_string(&mut buf);
            }
            for reg in range {
                buf.extend_from_slice(&ofs);
                reg.write_string(&mut buf);
            }
        }
        self.symbols.ors.write_string(&mut buf);

        IoRequest::FileWrite { buf, at: FilePath::Stdout }
    }

    /// Join index register values with `SUBSEP` into an array key (gawk-compatible).
    fn make_array_key(&mut self, start: Reg, end: Reg) -> String {
        let range = self.registers.get_range(start..end, self.reg_offset());
        let mut buf = StdVec::new();
        for (i, value) in range.iter().enumerate() {
            if i > 0 {
                self.symbols.subsep.write_string(&mut buf);
            }
            value.write_string(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn user_call(
        &mut self,
        dest: Reg,
        start: Reg,
        end: Reg,
        name: UserNonLocal,
        metadata: &[MetaId],
    ) -> Result<()> {
        let reg_offset = start.0 as IxWidth + self.reg_offset();
        let Some(&Some(Function { arity, hwm_regs, ref code })) =
            self.symbols.functions.get_index(name)
        else {
            return Err(InterpreterError::UnknownFunction(self.get_span(metadata)));
        };
        let call_arity = end.0 - start.0;

        if arity < call_arity {
            return Err(InterpreterError::ArityMismatch(
                self.get_span(metadata),
                arity,
                call_arity,
            ));
        }

        self.registers.reserve(reg_offset + hwm_regs as IxWidth);
        for reg in call_arity..arity {
            // Fill in remaining arguments / local variables.
            self.registers.write(Reg(reg), reg_offset, Value::Untyped);
        }

        // Avoid infinite recursion
        if self.frames.len() > 4096 {
            return Err(InterpreterError::RecursionDepth(self.get_span(metadata)));
        }

        self.frames.push(CallFrame {
            reg_offset,
            ret_addr: self.program_counter + 1,
            prev_code_end: self.code_end,
            ret_dest: dest,
        });
        self.code_end = code.0.end;
        self.program_counter = code.0.start;
        Ok(())
    }

    fn get_span(&self, metadata: &[MetaId]) -> AriadneSpan {
        self.metadata[metadata[self.program_counter as usize]].clone()
    }
}

impl<'a> Registers<'a> {
    #[inline(always)]
    fn reserve(&mut self, len: IxWidth) {
        let len = len as usize;
        if self.0.len() < len {
            self.0.resize(len, Value::Untyped);
        }
    }
    #[inline(always)]
    const fn index_of(reg: Reg, offset: IxWidth) -> usize {
        reg.0 as usize + offset as usize
    }
    #[inline(always)]
    fn get(&self, src: Reg, offset: IxWidth) -> &Value<'a> {
        let ix = Self::index_of(src, offset);
        &self.0[ix]
    }
    #[inline(always)]
    fn get_mut(&mut self, src: Reg, offset: IxWidth) -> &mut Value<'a> {
        let ix = Self::index_of(src, offset);
        &mut self.0[ix]
    }
    #[inline(always)]
    fn write(&mut self, dest: Reg, offset: IxWidth, src: impl Into<Value<'a>>) {
        self.0[dest.0 as usize + offset as usize] = src.into();
    }
    #[inline(always)]
    fn get_range(&self, regs: Range<Reg>, offset: IxWidth) -> &[Value<'a>] {
        let start = Self::index_of(regs.start, offset);
        let end = Self::index_of(regs.end, offset);
        &self.0[start..end]
    }
}

impl Display for Interpreter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}\n", self.registers)?;
        writeln!(f, "{}\n", self.symbols)?;
        write!(f, "{}", self.consts)
    }
}

impl Display for CodeGen<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}\n", self.bc)?;
        writeln!(f, "{}\n", self.symbols)?;
        write!(f, "{}", self.consts)
    }
}

impl Arg {
    #[inline(always)]
    fn get_val<'v, 'a>(
        self,
        ty: ArgTy,
        intrp: &'v mut Interpreter<'a>,
        // We have super let at home.
        stack_space: &'v mut MaybeUninit<Value<'a>>,
    ) -> Option<&'v Value<'a>> {
        self.prepare(ty, intrp, stack_space)?;

        // SAFETY: called `Arg::prepare` beforehand with the right args.
        Some(unsafe { self.read_already_prepared(ty, intrp, stack_space) })
    }

    #[inline(always)]
    fn get_val2<'a, T>(
        lhs: Self,
        tyl: ArgTy,
        rhs: Self,
        tyr: ArgTy,
        intrp: &mut Interpreter<'a>,
        f: impl FnOnce(&Value<'a>, &Value<'a>) -> T,
    ) -> Option<T> {
        let mut stack_space_lhs = MaybeUninit::uninit();
        let mut stack_space_rhs = MaybeUninit::uninit();
        lhs.prepare(tyl, intrp, &mut stack_space_lhs)?;
        rhs.prepare(tyr, intrp, &mut stack_space_rhs)?;

        // SAFETY: called `Arg::prepare` beforehand with the right args.
        let lhs = unsafe { lhs.read_already_prepared(tyl, intrp, &stack_space_lhs) };
        let rhs = unsafe { rhs.read_already_prepared(tyr, intrp, &stack_space_rhs) };

        Some(f(lhs, rhs))
    }

    /// Gets a value without scalar/array side-effects.
    #[inline(always)]
    fn get_pure<'v, 'a>(
        self,
        ty: ArgTy,
        intrp: &'v mut Interpreter<'a>,
        stack_space: &'v mut MaybeUninit<Value<'a>>,
    ) -> &'v Value<'a> {
        match ty {
            ArgTy::Reg => intrp.read_reg(unsafe { self.reg }),
            ArgTy::Imm => stack_space.write(Value::Int(unsafe { self.imm } as isize)),
            ArgTy::Cnt => &intrp.consts.0[unsafe { self.cnt.0 } as usize],
            ArgTy::UserVal => intrp.symbols.user(unsafe { self.usr }),
            ArgTy::BtInVal if unsafe { self.sys } == BuiltInVar::Nf => {
                stack_space.write(intrp.sync_nf_read())
            }
            ArgTy::BtInVal => intrp.symbols.get_btin(unsafe { self.sys }),
        }
    }

    /// Only exists to make the borrow checker happy. To be used in conjunction
    /// w/ [`Self::read_already_prepared`]. Please, don't use either of these if
    /// you can help it; use [`Self::get_val`] or [`Self::get_val2`] instead.
    #[inline(always)]
    fn prepare<'a>(
        self,
        ty: ArgTy,
        intrp: &mut Interpreter<'a>,
        stack_space: &mut MaybeUninit<Value<'a>>,
    ) -> Option<()> {
        match ty {
            ArgTy::Reg => intrp.read_reg_mut(unsafe { self.reg }),
            ArgTy::Imm => {
                stack_space.write(Value::Int(unsafe { self.imm } as isize));
                return Some(());
            }
            ArgTy::Cnt => return Some(()),
            ArgTy::UserVal => intrp.symbols.user_mut(unsafe { self.usr }),
            ArgTy::BtInVal if unsafe { self.sys } == BuiltInVar::Nf => {
                stack_space.write(intrp.sync_nf_read())
            }
            ArgTy::BtInVal => intrp.symbols.get_btin_mut(unsafe { self.sys }),
        }
        .scalar_context()
        .map(drop)
    }

    /// # SAFETY
    ///
    /// Must have called [`Self::prepare`] with the exact same arguments. Only
    /// exists to make the borrowck happy via two-phased initialization (yuck).
    #[inline(always)]
    unsafe fn read_already_prepared<'v, 'a>(
        self,
        ty: ArgTy,
        intrp: &'v Interpreter<'a>,
        stack_space: &'v MaybeUninit<Value<'a>>,
    ) -> &'v Value<'a> {
        match ty {
            ArgTy::Reg => intrp.read_reg(unsafe { self.reg }),
            ArgTy::Imm => unsafe { stack_space.assume_init_ref() },
            ArgTy::Cnt => &intrp.consts.0[unsafe { self.cnt.0 } as usize],
            ArgTy::UserVal => intrp.symbols.user(unsafe { self.usr }),
            ArgTy::BtInVal if unsafe { self.sys } == BuiltInVar::Nf => unsafe {
                stack_space.assume_init_ref()
            },
            ArgTy::BtInVal => intrp.symbols.get_btin(unsafe { self.sys }),
        }
    }
}

impl Place {
    #[inline(always)]
    const fn new(arg: Arg, ty: PlaceTy) -> Self {
        Self { arg, ty }
    }

    /// Forces array typeck and returns the resolved place.
    fn array<'v, 'a>(self, intrp: &'v mut Interpreter<'a>) -> Option<&'v mut Value<'a>> {
        match self.ty {
            PlaceTy::Reg => intrp.read_reg_mut(unsafe { self.arg.reg }),
            PlaceTy::UserVal => intrp.symbols.user_mut(unsafe { self.arg.usr }),
            PlaceTy::BtInVal if unsafe { self.arg.sys }.is_always_scalar() => return None,
            PlaceTy::BtInVal => intrp.symbols.get_btin_mut(unsafe { self.arg.sys }),
        }
        .array_context()
    }

    #[inline(always)]
    fn write<'a>(self, intrp: &mut Interpreter<'a>, val: Value<'a>) {
        match self.ty {
            PlaceTy::Reg => *intrp.read_reg_mut(unsafe { self.arg.reg }) = val,
            PlaceTy::UserVal => *intrp.symbols.user_mut(unsafe { self.arg.usr }) = val,
            PlaceTy::BtInVal => intrp.sync_btin_write(unsafe { self.arg.sys }, val),
        }
    }
}

impl Display for Bytecode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bytecode:")?;
        let n = self.code.len().checked_ilog10().unwrap_or(0) as usize + 1;
        fmt_list(f, self.code.iter(), |f, i, e| write!(f, "{i:0n$}: {e}"))
    }
}

impl Display for Registers<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Registers:")?;
        let n = self.0.len().checked_ilog10().unwrap_or(0) as usize + 1;
        fmt_list(f, self.0.iter(), |f, i, e| write!(f, "r{i:0n$} = {e}"))
    }
}

impl Display for SymbolTable<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Symbols:")?;
        fmt_list(f, self.user.iter(), |f, i, (k, v)| {
            write!(f, "user[{i}] @ {k:?} = {v}")
        })?;
        for (name, val) in [
            ("ARGC", &self.argc),
            ("ARGIND", &self.argind),
            ("ARGV", &self.argv),
            ("BINMODE", &self.binmode),
            ("CONVFMT", &self.convfmt),
            ("ERRNO", &self.errno),
            ("FIELDWIDTHS", &self.fieldwidths),
            ("FILENAME", &self.filename),
            ("FNR", &self.fnr),
            ("FPAT", &self.fpat),
            ("FS", &self.fs),
            ("IGNORECASE", &self.ignorecase),
            ("LINT", &self.lint),
            ("NR", &self.nr),
            ("OFMT", &self.ofmt),
            ("OFS", &self.ofs),
            ("ORS", &self.ors),
            ("PREC", &self.prec),
            ("ROUNDMODE", &self.roundmode),
            ("RS", &self.rs),
            ("RT", &self.rt),
            ("RSTART", &self.rstart),
            ("RLENGTH", &self.rlength),
            ("SUBSEP", &self.subsep),
            ("TEXTDOMAIN", &self.textdomain),
        ] {
            write!(f, "\n  builtin {name} = {val}")?;
        }
        Ok(())
    }
}

impl Display for Consts<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Consts:")?;
        fmt_list(f, self.0.iter(), |f, i, e| write!(f, "mem[{i}] = {e}"))
    }
}

fn fmt_list<'a, T: Copy>(
    f: &mut fmt::Formatter<'a>,
    iter: impl Iterator<Item = T>,
    cb: impl Fn(&mut fmt::Formatter<'a>, usize, T) -> fmt::Result,
) -> fmt::Result {
    for (i, e) in iter.enumerate() {
        write!(f, "\n  ")?;
        cb(f, i, e)?;
    }
    Ok(())
}
