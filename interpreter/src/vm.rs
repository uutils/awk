// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

pub mod types;

use std::{
    cell::RefCell,
    fmt::{self, Display},
    io::{self, Write},
    mem::MaybeUninit,
    ops::Range,
    rc::Rc,
    vec::Vec as StdVec,
};

use ahash::RandomState;
use bumpalo::{Bump, collections::Vec};
use hashbrown::HashMap;
use indexmap_allocator_api::{IndexMap, IndexSet};
use parser::{AriadneSpan, Command, Identifier, MetaId, MetadataStore, Redirection};

use crate::{
    InterpreterError,
    ir::{
        Arg, ArgTy, Instruction, IxWidth, Label, NonLocal, Reg, RegWidth,
        lower::{Bytecode, CodeGen},
    },
    vm::types::{ArrayMap, Value},
};

#[derive(Debug)]
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
    symbols: SymbolTable<'a>,
    consts: Consts<'a>,
    _compat: ExecMode,
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
pub enum IoRequest {
    WriteStdout(StdVec<u8>),
}

#[derive(Debug)]
pub enum IoResponse {
    Empty,
}

#[derive(Debug)]
pub struct Registers<'a>(Vec<'a, Value<'a>>);

#[derive(Debug)]
pub struct RawSymbolTable<'a, T>(IndexMap<Identifier<'a>, T, RandomState, &'a Bump>);

#[derive(Debug)]
pub struct SymbolTable<'a> {
    user: RawSymbolTable<'a, Value<'a>>,
    functions: RawSymbolTable<'a, Option<Function>>,
    // separate table for cheap invalidation. It's an arena _visibly shrugs_.
    records: HashMap<usize, Value<'a>, RandomState, &'a Bump>,
    ofs: Value<'a>,
    rfs: Value<'a>,
    /// Default AWK `SUBSEP` (`"\034"`).
    subsep: Value<'a>,
    // etc
}

#[derive(Debug)]
pub struct Function {
    pub arity: RegWidth,
    pub hwm_regs: RegWidth,
    pub code: CodeRange,
}

#[derive(Debug)]
pub struct Consts<'a>(pub IndexSet<Value<'a>, RandomState, &'a Bump>);

#[derive(Debug, Clone)]
pub struct CodeRange(pub(crate) Range<IxWidth>);

impl<'a> Interpreter<'a> {
    pub fn new(compat: ExecMode, code: CodeGen<'a>, metadata: MetadataStore<AriadneSpan>) -> Self {
        let n_regs = code.reg_pointer as usize + 1;
        Self {
            program_counter: 0,
            code_end: 0,
            registers: Registers(bumpalo::vec![in code.arena; Value::Untyped; n_regs]),
            symbols: code.symbols,
            consts: code.consts,
            _compat: compat,
            frames: StdVec::new(),
            metadata,
        }
    }
}

impl<'a, T> RawSymbolTable<'a, T> {
    pub fn new_in(arena: &'a Bump) -> Self {
        Self(IndexMap::new_in(arena))
    }

    pub fn register(&mut self, ident: &Identifier, value: T, bump: &'a Bump) -> NonLocal {
        if let Some(index) = self.0.get_index_of(ident) {
            NonLocal(index as _)
        } else {
            let ident = Identifier {
                namespace: bump.alloc_str(ident.namespace),
                literal: bump.alloc_str(ident.literal),
            };
            NonLocal(self.0.insert_full(ident, value).0 as _)
        }
    }

    pub fn get_index(&self, var: NonLocal) -> Option<&T> {
        self.0.get_index(var.0 as _).map(|x| x.1)
    }

    pub fn get_index_mut(&mut self, var: NonLocal) -> Option<&mut T> {
        self.0.get_index_mut(var.0 as _).map(|x| x.1)
    }

    pub fn insert(&mut self, ident: Identifier<'a>, value: T) -> Option<T> {
        self.0.insert(ident, value)
    }

    pub fn lookup(&mut self, ident: &Identifier) -> Option<(NonLocal, &mut T)> {
        self.0
            .get_index_of(ident)
            .map(|ix| (NonLocal(ix as _), &mut self.0[ix]))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Identifier<'a>, &T)> {
        self.0.iter()
    }
}

impl<'a> SymbolTable<'a> {
    pub fn new_in(arena: &'a Bump) -> Self {
        Self {
            user: RawSymbolTable::new_in(arena),
            functions: RawSymbolTable::new_in(arena),
            records: HashMap::with_hasher_in(RandomState::new(), arena),
            ofs: Value::String(b" ".into()),
            rfs: Value::String(b"\n".into()),
            subsep: Value::String(b"\x1c".into()),
        }
    }

    fn lookup_user_scalar(&mut self, var: NonLocal) -> &Value<'a> {
        let v = self.user.get_index_mut(var).unwrap();
        v.scalar_context()
    }

    // HACK: Please do not use this if you can help it.
    fn raw_user_lookup(&self, var: NonLocal) -> &Value<'a> {
        self.user.get_index(var).unwrap()
    }

    fn write_user_val(&mut self, var: NonLocal, value: Value<'a>) {
        *self.user.get_index_mut(var).unwrap() = value;
    }

    fn user_array(&mut self, var: NonLocal) -> Rc<RefCell<ArrayMap<'a>>> {
        let v = self.user.get_index_mut(var).unwrap();
        v.array_context();
        match v {
            Value::Array(arr) => Rc::clone(arr),
            _ => unreachable!("array_context must leave an Array"),
        }
    }

    fn load_user_array_elem(&mut self, var: NonLocal, key: &str) -> Value<'a> {
        self.user_array(var)
            .borrow()
            .get(key)
            .cloned()
            .unwrap_or(Value::Unassigned)
    }

    fn store_user_array_elem(&mut self, var: NonLocal, key: String, value: Value<'a>) {
        self.user_array(var).borrow_mut().insert(key, value);
    }

    pub fn register_user_var(&mut self, var: &Identifier, bump: &'a Bump) -> NonLocal {
        self.user.register(var, Value::Untyped, bump)
    }

    pub fn register_user_var_with(&mut self, var: &Identifier, val: &str, bump: &'a Bump) {
        let ident = Identifier {
            namespace: bump.alloc_str(var.namespace),
            literal: bump.alloc_str(var.literal),
        };
        self.user.insert(
            ident,
            if let Ok(n) = val.parse() {
                // TODO: use strnum
                Value::Float(n)
            } else {
                Value::String(bump.alloc_str(val).as_bytes().into())
            },
        );
    }

    pub fn register_user_fun(
        &mut self,
        name: &Identifier,
        fun: Function,
        bump: &'a Bump,
    ) -> NonLocal {
        if let Some((nl, f)) = self.functions.lookup(name) {
            *f = Some(fun);
            nl
        } else {
            self.functions.register(name, Some(fun), bump)
        }
    }

    pub fn get_user_fun(&mut self, name: &Identifier, bump: &'a Bump) -> NonLocal {
        self.functions.register(name, None, bump)
    }

    pub fn record(&self, value: Value<'a>) -> &Value<'a> {
        self.records
            .get(&(value.to_num() as usize))
            .unwrap_or(&Value::Unassigned)
    }
}

impl<'a> Consts<'a> {
    pub fn new_in(arena: &'a Bump) -> Self {
        Self(IndexSet::with_capacity_in(4, arena))
    }
}

impl<'a> Interpreter<'a> {
    pub fn run_code(&mut self, bc: &Bytecode, span: CodeRange) -> Result<Signal, InterpreterError> {
        self.program_counter = span.0.start;
        self.code_end = span.0.end;

        self.run_chunk(&bc.code, &bc.metadata)
    }

    fn run_chunk(
        &mut self,
        bytecode: &[Instruction],
        metadata: &[MetaId],
    ) -> Result<Signal, InterpreterError> {
        while let Some(&instr) = bytecode.get(self.program_counter as usize)
            && self.program_counter < self.code_end
        {
            match instr {
                Instruction::Record { dest: _, arg: _, ty: _ } => todo!(),
                Instruction::Negation { dest, arg, ty } => {
                    let val = arg.get_val(ty, self, &mut MaybeUninit::uninit()).to_bool();
                    self.write_reg(dest, !val);
                }
                Instruction::ToInt { dest, arg, ty } => {
                    let val = arg.get_val(ty, self, &mut MaybeUninit::uninit()).to_num();
                    self.write_reg(dest, val);
                }
                Instruction::Negative { dest, arg, ty } => {
                    let val = arg.get_val(ty, self, &mut MaybeUninit::uninit()).to_num();
                    self.write_reg(dest, -val);
                }
                Instruction::IncrementPost { dest, arg, ty }
                | Instruction::IncrementPre { dest, arg, ty }
                | Instruction::DecrementPost { dest, arg, ty }
                | Instruction::DecrementPre { dest, arg, ty } => {
                    let raw_arg = arg;
                    let rhs = &Value::Int(match instr {
                        Instruction::IncrementPost { .. } | Instruction::IncrementPre { .. } => 1,
                        _ => -1,
                    });
                    let is_post = matches!(
                        instr,
                        Instruction::IncrementPost { .. } | Instruction::DecrementPost { .. }
                    );

                    let (added, res) = {
                        let mut stack_space = MaybeUninit::uninit();
                        let val = arg.get_val(ty, self, &mut stack_space);
                        (val + rhs, val + if is_post { &Value::Int(0) } else { rhs })
                    };
                    self.write_reg(dest, res);

                    // TODO: refactor generic writes into a helper
                    match ty {
                        ArgTy::Reg => {
                            self.write_reg(unsafe { raw_arg.reg }, added);
                        }
                        ArgTy::UsVal => self.symbols.write_user_val(unsafe { raw_arg.sym }, added),
                        ArgTy::IsVal => todo!(),
                        _ => unreachable!(),
                    }
                }
                Instruction::Copy { dest, arg, ty } => {
                    let val = arg.get_val(ty, self, &mut MaybeUninit::uninit()).clone();
                    self.write_reg(dest, val);
                }
                Instruction::Eq { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs == rhs);
                    self.write_reg(dest, val);
                }
                Instruction::NEq { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs != rhs);
                    self.write_reg(dest, val);
                }
                Instruction::Gt { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs > rhs);
                    self.write_reg(dest, val);
                }
                Instruction::Lt { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs < rhs);
                    self.write_reg(dest, val);
                }
                Instruction::LtE { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs <= rhs);
                    self.write_reg(dest, val);
                }
                Instruction::GtE { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs >= rhs);
                    self.write_reg(dest, val);
                }
                Instruction::Matches { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| match rhs {
                        Value::Regex(pat) => lhs.matches_regex(pat),
                        _ => false,
                    });
                    self.write_reg(dest, val);
                }
                Instruction::MatchesNot { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| match rhs {
                        Value::Regex(pat) => lhs.matches_regex(pat),
                        _ => false,
                    });
                    self.write_reg(dest, !val);
                }
                Instruction::Add { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs + rhs);
                    self.write_reg(dest, val);
                }
                Instruction::Subtract { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs - rhs);
                    self.write_reg(dest, val);
                }
                Instruction::Multiply { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs * rhs);
                    self.write_reg(dest, val);
                }
                Instruction::Divide { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs / rhs);
                    self.write_reg(dest, val);
                }
                Instruction::Raise { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs ^ rhs);
                    self.write_reg(dest, val);
                }
                Instruction::Modulo { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| lhs % rhs);
                    self.write_reg(dest, val);
                }
                Instruction::Concat { dest, lhs, rhs, tyl, tyr } => {
                    let val = Arg::get_val2(lhs, tyl, rhs, tyr, self, |lhs, rhs| {
                        let mut buf =
                            StdVec::with_capacity(lhs.string_size_hint() + rhs.string_size_hint());
                        lhs.write_string(&mut buf);
                        rhs.write_string(&mut buf);
                        buf
                    });
                    self.write_reg(dest, val);
                }
                Instruction::LoadA { dest, ty_place, start, end, var } => {
                    let key = self.make_array_key(start, end);
                    let val = match ty_place {
                        ArgTy::UaVal => self.symbols.load_user_array_elem(var, &key),
                        ArgTy::IaVal => todo!("intrinsic array load"),
                        _ => unreachable!(),
                    };
                    self.write_reg(dest, val);
                }
                Instruction::StoreS { dest, ty_place, var, arg, ty } => {
                    let val = arg.get_val(ty, self, &mut MaybeUninit::uninit()).clone();
                    match ty_place {
                        ArgTy::UsVal => self.symbols.write_user_val(var, val.clone()),
                        ArgTy::IsVal => todo!(),
                        _ => unreachable!(),
                    }
                    self.write_reg(dest, val);
                }
                Instruction::StoreR { dest: _, src: _, arg: _, ty: _, tys: _ } => {
                    todo!()
                }
                Instruction::StoreA { dest, ty_place, start, end, var, arg } => {
                    let key = self.make_array_key(start, end);
                    let val = self.read_reg(arg).clone();
                    match ty_place {
                        ArgTy::UaVal => {
                            self.symbols.store_user_array_elem(var, key, val.clone());
                        }
                        ArgTy::IaVal => todo!("intrinsic array store"),
                        _ => unreachable!(),
                    }
                    self.write_reg(dest, val);
                }
                Instruction::IntrinsicCall { dest: _, start: _, end: _, name: _ } => todo!(),
                Instruction::OutputCall { start, end, cmd, redir } => {
                    return Ok(Signal::Suspend(self.print_req(start, end, cmd, redir)));
                }
                Instruction::UserCall { dest, start, end, name } => {
                    self.user_call(dest, start, end, name, metadata)?;
                    continue;
                }
                Instruction::IndirectCall { dest, start, end, name, ty } => {
                    let name = name
                        .get_val(ty, self, &mut MaybeUninit::uninit())
                        .to_string();
                    // TODO: Proper parsing, catch indirect calls to built-ins,
                    //       native funs and namespace tracking in metadata.
                    let (namespace, literal) = name.split_once("::").unwrap_or(("", &name));
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
                    self.program_counter = label as _;
                    continue;
                }
                Instruction::Branch { then_label, else_label, condition } => {
                    if self.read_reg(condition).to_bool() {
                        self.program_counter = then_label.0 as _;
                    } else {
                        self.program_counter = else_label.0 as _;
                    }
                    continue;
                }
                Instruction::Exit { arg, ty } => {
                    let val = arg.get_val(ty, self, &mut MaybeUninit::uninit()).to_int();
                    return Ok(Signal::Terminal(CtrlSig::Exit(val as i32)));
                }
                Instruction::Return { arg, ty } => {
                    let val = arg.get_val(ty, self, &mut MaybeUninit::uninit()).clone();
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

    /// Convenience wrapper to write a value at the current reg slice.
    fn write_reg(&mut self, dest: Reg, val: impl Into<Value<'a>>) {
        self.registers.write(dest, self.reg_offset(), val);
    }

    /// Convenience wrapper to read a value from the current reg slice.
    fn read_reg(&self, src: Reg) -> &Value<'a> {
        self.registers.get(src, self.reg_offset())
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
        _res: io::Result<IoResponse>,
    ) -> io::Result<Result<Signal, InterpreterError>> {
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
            let record = self.symbols.record(Value::Float(0.));
            let _ = write!(buf, "{record}");
        } else {
            let mut range = range.iter();
            if let Some(reg) = range.next() {
                let _ = write!(buf, "{reg}");
            }
            for reg in range {
                let _ = write!(buf, "{ofs}{reg}", ofs = self.symbols.ofs);
            }
        }
        let _ = write!(buf, "{rfs}", rfs = self.symbols.rfs);

        IoRequest::WriteStdout(buf)
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
        name: NonLocal,
        metadata: &[MetaId],
    ) -> Result<(), InterpreterError> {
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
    fn reserve(&mut self, len: IxWidth) {
        let len = len as usize;
        if self.0.len() < len {
            self.0.resize(len, Value::Untyped);
        }
    }
    fn index_of(reg: Reg, offset: IxWidth) -> usize {
        reg.0 as usize + offset as usize
    }
    fn get(&self, src: Reg, offset: IxWidth) -> &Value<'a> {
        let ix = Self::index_of(src, offset);
        &self.0[ix]
    }
    fn write(&mut self, dest: Reg, offset: IxWidth, src: impl Into<Value<'a>>) {
        self.0[dest.0 as usize + offset as usize] = src.into();
    }
    fn get_range(&mut self, regs: Range<Reg>, offset: IxWidth) -> &[Value<'a>] {
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
    fn get_val<'v, 'a>(
        self,
        ty: ArgTy,
        intrp: &'v mut Interpreter<'a>,
        // We have super let at home.
        stack_space: &'v mut MaybeUninit<Value<'a>>,
    ) -> &'v Value<'a> {
        self.prepare(ty, intrp, stack_space);

        // SAFETY: called `Arg::prepare` beforehand with the right args.
        unsafe { self.read_already_prepared(ty, intrp, stack_space) }
    }

    fn get_val2<'a, T>(
        lhs: Self,
        tyl: ArgTy,
        rhs: Self,
        tyr: ArgTy,
        intrp: &mut Interpreter<'a>,
        f: impl FnOnce(&Value<'a>, &Value<'a>) -> T,
    ) -> T {
        let mut stack_space_lhs = MaybeUninit::uninit();
        let mut stack_space_rhs = MaybeUninit::uninit();
        lhs.prepare(tyl, intrp, &mut stack_space_lhs);
        rhs.prepare(tyr, intrp, &mut stack_space_rhs);

        // SAFETY: called `Arg::prepare` beforehand with the right args.
        let lhs = unsafe { lhs.read_already_prepared(tyl, intrp, &stack_space_lhs) };
        let rhs = unsafe { rhs.read_already_prepared(tyr, intrp, &stack_space_rhs) };

        f(lhs, rhs)
    }

    /// Only exists to make the borrow checker happy. To be used in conjunction
    /// w/ [`Self::read_already_prepared`]. Please, don't use either of these if
    /// you can help it; use [`Self::get_val`] or [`Self::get_val2`] instead.
    fn prepare<'a>(
        self,
        ty: ArgTy,
        intrp: &mut Interpreter<'a>,
        stack_space: &mut MaybeUninit<Value<'a>>,
    ) {
        match ty {
            ArgTy::Reg | ArgTy::Cnt => {}
            ArgTy::Rec => todo!(),
            ArgTy::Imm => {
                stack_space.write(Value::Int(unsafe { self.imm } as _));
            }
            ArgTy::UsVal => {
                // Forces it a scalar without reading the value yet.
                intrp.symbols.lookup_user_scalar(unsafe { self.sym });
            }
            _ => todo!(),
        }
    }

    /// # SAFETY
    ///
    /// Must have called [`Self::prepare`] with the exact same arguments. Only
    /// exists to make the borrowck happy via two-phased initialization (yuck).
    unsafe fn read_already_prepared<'v, 'a>(
        self,
        ty: ArgTy,
        intrp: &'v Interpreter<'a>,
        stack_space: &'v MaybeUninit<Value<'a>>,
    ) -> &'v Value<'a> {
        match ty {
            ArgTy::Reg => intrp.read_reg(unsafe { self.reg }),
            ArgTy::Rec => todo!(),
            ArgTy::Imm => unsafe { stack_space.assume_init_ref() },
            ArgTy::Cnt => intrp
                .consts
                .0
                .get_index(unsafe { self.sym.0 } as _)
                .unwrap(),
            ArgTy::UsVal => intrp.symbols.raw_user_lookup(unsafe { self.sym }),
            _ => todo!(),
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
        })
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
