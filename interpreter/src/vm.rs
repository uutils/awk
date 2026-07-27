// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

pub mod types;

use std::{
    cell::RefCell,
    fmt::{self, Display},
    io::{self, Write},
    mem::replace,
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
        ArgTy, Instruction, IxWidth, Label, NonLocal, Reg, RegWidth,
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
    arena: &'a Bump,
    program_counter: IxWidth,
    code_end: IxWidth,
    registers: Registers<'a>,
    symbols: SymbolTable<'a>,
    consts: Consts<'a>,
    compat: ExecMode,
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
    Error(InterpreterError),
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
            arena: code.arena,
            program_counter: 0,
            code_end: 0,
            registers: Registers(bumpalo::vec![in code.arena; Value::Untyped; n_regs]),
            symbols: code.symbols,
            consts: code.consts,
            compat,
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
    pub fn run_code(&mut self, bytecode: &Bytecode, range: CodeRange) -> io::Result<Signal> {
        self.program_counter = range.0.start;
        self.code_end = range.0.end;

        match self.run_chunk(&bytecode.code, &bytecode.metadata) {
            Ok(s) => Ok(s),
            Err(err) => Ok(Signal::Error(err)),
        }
    }

    fn run_chunk(
        &mut self,
        bytecode: &[Instruction],
        metadata: &[MetaId],
    ) -> Result<Signal, InterpreterError> {
        macro_rules! rx {
            ($self:expr, $dest:expr, $src:ident: $ty:ident, $e:expr) => {{
                rx!($self, $src: $ty);
                $self.registers.write($dest, $self.reg_offset(), $e);
            }};
            ($self:expr, $dest:expr, $lhs:ident: $tyl:ident, $rhs:ident: $tyr:ident, $e:expr) => {{
                rx!($self, $lhs: $tyl, $rhs: $tyr);
                $self.registers.write($dest, $self.reg_offset(), $e);
            }};
            ($self:expr, $($src:ident: $ty:ident),+) => {
                use $crate::ir::ArgTy;
                $(let $src = match $ty {
                    ArgTy::Reg => $self.registers.get(unsafe { $src.reg }, $self.reg_offset()),
                    ArgTy::Rec => todo!(),
                    ArgTy::Imm => &Value::Int(unsafe { $src.imm } as _),
                    ArgTy::Cnt => &$self.consts.0.get_index(unsafe { $src.sym.0 } as _).unwrap().clone(),
                    ArgTy::UsVal => {
                        &$self.symbols.lookup_user_scalar(unsafe { $src.sym }).clone()
                    }
                    _ => todo!()
                };)+
            };
            ($self:expr, $dest:expr, $lhs:ident, $rhs:ident, $e:expr) => {{
                rx!($self, $lhs, $rhs);
                $self.registers.write($dest, $e);
            }};
        }
        while let Some(&instr) = bytecode.get(self.program_counter as usize)
            && self.program_counter < self.code_end
        {
            match instr {
                Instruction::Record { dest: _, arg: _, ty: _ } => todo!(),
                Instruction::Negation { dest, arg, ty } => {
                    rx!(self, dest, arg: ty, Value::b2f(!arg.to_bool()));
                }
                Instruction::ToInt { dest, arg, ty } => {
                    rx!(self, dest, arg: ty, Value::Float(arg.to_num().trunc()));
                }
                Instruction::Negative { dest, arg, ty } => {
                    rx!(self, dest, arg: ty, Value::Float(-arg.to_num()));
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

                    rx!(self, arg: ty);
                    let added = arg + rhs;
                    let res = arg + if is_post { &Value::Int(0) } else { rhs };
                    self.registers.write(dest, self.reg_offset(), res);

                    // TODO: refactor generic writes into a helper
                    match ty {
                        ArgTy::Reg => {
                            self.registers
                                .write(unsafe { raw_arg.reg }, self.reg_offset(), added);
                        }
                        ArgTy::UsVal => self.symbols.write_user_val(unsafe { raw_arg.sym }, added),
                        ArgTy::IsVal => todo!(),
                        _ => unreachable!(),
                    }
                }
                Instruction::Copy { dest, arg, ty } => rx!(self, dest, arg: ty, arg.clone()),
                Instruction::Eq { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, Value::b2f(lhs == rhs));
                }
                Instruction::NEq { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, Value::b2f(lhs != rhs));
                }
                Instruction::Gt { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, Value::b2f(lhs > rhs));
                }
                Instruction::Lt { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, Value::b2f(lhs < rhs));
                }
                Instruction::LtE { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, Value::b2f(lhs <= rhs));
                }
                Instruction::GtE { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, Value::b2f(lhs >= rhs));
                }
                Instruction::Matches { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, lhs: tyl, rhs: tyr);
                    let matched = match rhs {
                        Value::Regex(pat) => lhs.matches_regex(pat),
                        _ => false,
                    };
                    self.registers
                        .write(dest, self.reg_offset(), Value::b2f(matched));
                }
                Instruction::MatchesNot { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, lhs: tyl, rhs: tyr);
                    let matched = match rhs {
                        Value::Regex(pat) => lhs.matches_regex(pat),
                        _ => false,
                    };
                    self.registers
                        .write(dest, self.reg_offset(), Value::b2f(!matched));
                }
                Instruction::Add { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, lhs + rhs);
                }
                Instruction::Subtract { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, lhs - rhs);
                }
                Instruction::Multiply { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, lhs * rhs);
                }
                Instruction::Divide { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, lhs / rhs);
                }
                Instruction::Raise { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, lhs ^ rhs);
                }
                Instruction::Modulo { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, dest, lhs: tyl, rhs: tyr, lhs % rhs);
                }
                Instruction::Concat { dest, lhs, rhs, tyl, tyr } => {
                    rx!(self, lhs: tyl, rhs: tyr);
                    let mut buf =
                        StdVec::with_capacity(lhs.string_size_hint() + rhs.string_size_hint());
                    lhs.write_string(&mut buf);
                    rhs.write_string(&mut buf);
                    self.registers
                        .write(dest, self.reg_offset(), Value::String(buf.into()));
                }
                Instruction::LoadA { dest, ty_place, start, end, var } => {
                    let key = self.make_array_key(start, end);
                    let value = match ty_place {
                        ArgTy::UaVal => self.symbols.load_user_array_elem(var, &key),
                        ArgTy::IaVal => todo!("intrinsic array load"),
                        _ => unreachable!(),
                    };
                    self.registers.write(dest, self.reg_offset(), value);
                }
                Instruction::StoreS { dest, ty_place, var, arg, ty } => {
                    rx!(self, arg: ty);
                    match ty_place {
                        ArgTy::UsVal => self.symbols.write_user_val(var, arg.clone()),
                        ArgTy::IsVal => todo!(),
                        _ => unreachable!(),
                    }
                    self.registers.write(dest, self.reg_offset(), arg.clone());
                }
                Instruction::StoreR { dest: _, src: _, arg: _, ty: _, tys: _ } => {
                    todo!()
                }
                Instruction::StoreA { dest, ty_place, start, end, var, arg } => {
                    let key = self.make_array_key(start, end);
                    let value = self.registers.get(arg, self.reg_offset()).clone();
                    match ty_place {
                        ArgTy::UaVal => {
                            self.symbols.store_user_array_elem(var, key, value.clone());
                        }
                        ArgTy::IaVal => todo!("intrinsic array store"),
                        _ => unreachable!(),
                    }
                    self.registers.write(dest, self.reg_offset(), value);
                }
                Instruction::IntrinsicCall { dest: _, start: _, end: _, name: _ } => todo!(),
                Instruction::OutputCall { start, end, cmd, redir } => {
                    return Ok(Signal::Suspend(self.print_req(start, end, cmd, redir)));
                }
                Instruction::UserCall { dest, start, end, name } => {
                    let reg_offset = start.0 as IxWidth + self.reg_offset();
                    let Some(&Some(Function { arity, hwm_regs, ref code })) =
                        self.symbols.functions.get_index(name)
                    else {
                        return Err(InterpreterError::UnknownFunction(
                            self.metadata[metadata[self.program_counter as usize]].clone(),
                        ));
                    };

                    self.registers.reserve(reg_offset + hwm_regs as IxWidth);
                    for reg in (end.0 - start.0)..arity {
                        self.registers.write(Reg(reg), reg_offset, Value::Untyped);
                    }

                    // TODO: add a recursion depth check.
                    self.frames.push(CallFrame {
                        reg_offset,
                        ret_addr: self.program_counter + 1,
                        prev_code_end: self.code_end,
                        ret_dest: dest,
                    });
                    self.code_end = code.0.end;
                    self.program_counter = code.0.start;
                    continue;
                }
                Instruction::IndirectCall { dest: _, start: _, end: _, name: _, ty: _ } => todo!(),
                Instruction::Jump { to: Label(label) } => {
                    self.program_counter = label as _;
                    continue;
                }
                Instruction::Branch { then_label, else_label, condition } => {
                    if self.registers.get(condition, self.reg_offset()).to_bool() {
                        self.program_counter = then_label.0 as _;
                    } else {
                        self.program_counter = else_label.0 as _;
                    }
                    continue;
                }
                Instruction::Exit { arg, ty } => {
                    rx!(self, arg: ty);
                    return Ok(Signal::Terminal(CtrlSig::Exit(arg.to_int() as i32)));
                }
                Instruction::Return { arg, ty } => {
                    rx!(self, arg: ty);
                    self.ret(arg.clone());
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

    fn ret(&mut self, val: Value<'a>) {
        let Some(CallFrame { reg_offset: _, ret_addr, prev_code_end, ret_dest }) =
            self.frames.pop()
        else {
            unreachable!()
        };
        self.registers.write(ret_dest, self.reg_offset(), val);
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
    ) -> io::Result<Signal> {
        self.program_counter += 1;
        match self.run_chunk(&bytecode.code, &bytecode.metadata) {
            Ok(s) => Ok(s),
            Err(e) => Ok(Signal::Error(e)),
        }
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
}

impl<'a> Registers<'a> {
    fn replace(&mut self, src: Reg, offset: IxWidth, f: impl FnOnce(Value<'a>) -> Value<'a>) {
        let val = replace(self.get_mut(src, offset), Value::Untyped);
        self.write(src, offset, f(val));
    }
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
    fn get_mut(&mut self, src: Reg, offset: IxWidth) -> &mut Value<'a> {
        let ix = Self::index_of(src, offset);
        &mut self.0[ix]
    }
    fn write(&mut self, dest: Reg, offset: IxWidth, src: Value<'a>) {
        self.0[dest.0 as usize + offset as usize] = src;
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
