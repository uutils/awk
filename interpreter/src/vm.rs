// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{
    fmt::{self, Display},
    io::{self, Write},
    mem::replace,
    ops::Range,
    vec::Vec as StdVec,
};

use ahash::RandomState;
use bumpalo::{Bump, collections::Vec};
use hashbrown::HashMap;
use indexmap_allocator_api::{IndexMap, IndexSet};
use parser::{Command, Identifier, Redirection};

use crate::{
    ir::{
        Instruction, Label, NonLocal, Reg,
        lower::{Bytecode, Code},
    },
    types::Value,
};

#[derive(Debug)]
pub enum ExecMode {
    Uu,
    Gnu,
    Posix,
}

pub struct Interpreter<'a> {
    arena: &'a Bump,
    bc: Bytecode<'a>,
    program_counter: usize,
    registers: Registers<'a>,
    symbols: SymbolTable<'a>,
    consts: Consts<'a>,
    compat: ExecMode,
}

#[derive(Debug)]
pub struct Registers<'a>(Vec<'a, Value<'a>>);

#[derive(Debug)]
pub struct SymbolTable<'a> {
    user: IndexMap<Identifier<'a>, Value<'a>, RandomState, &'a Bump>,
    // separate table for cheap invalidation. It's an arena _visibly shrugs_.
    records: HashMap<usize, Value<'a>, RandomState, &'a Bump>,
    ofs: Value<'a>,
    rfs: Value<'a>,
    // etc
}

#[derive(Debug)]
pub struct Consts<'a>(pub IndexSet<Value<'a>, RandomState, &'a Bump>);

impl<'a> Interpreter<'a> {
    pub fn new(compat: ExecMode, code: Code<'a>) -> Self {
        let n_regs = code.reg_pointer as usize + 1;
        Self {
            arena: code.arena,
            bc: code.bc,
            program_counter: 0,
            registers: Registers(bumpalo::vec![in code.arena; Value::Untyped; n_regs]),
            symbols: code.symbols,
            consts: code.consts,
            compat,
        }
    }
}

impl<'a> SymbolTable<'a> {
    pub fn new_in(arena: &'a Bump) -> Self {
        Self {
            user: IndexMap::new_in(arena),
            records: HashMap::with_hasher_in(RandomState::new(), arena),
            ofs: Value::String(b" ".into()),
            rfs: Value::String(b"\n".into()),
        }
    }

    fn lookup_user_scalar(&mut self, var: NonLocal) -> &Value<'a> {
        let v = self.user.get_index_mut(var.0 as _).unwrap().1;
        v.scalar_context()
    }

    fn write_user_val(&mut self, var: NonLocal, value: Value<'a>) {
        *self.user.get_index_mut(var.0 as _).unwrap().1 = value;
    }

    pub fn register_user_var(&mut self, var: &Identifier, bump: &'a Bump) -> NonLocal {
        if let Some(index) = self.user.get_index_of(var) {
            NonLocal(index as _)
        } else {
            let ident = Identifier {
                namespace: bump.alloc_str(var.namespace),
                literal: bump.alloc_str(var.literal),
            };
            NonLocal(self.user.insert_full(ident, Value::Untyped).0 as _)
        }
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

impl Interpreter<'_> {
    pub fn run(&mut self) {
        macro_rules! rx {
            ($self:expr, $dest:expr, $src:ident: $ty:ident, $e:expr) => {{
                rx!($self, $src: $ty);
                $self.registers.write($dest, $e);
            }};
            ($self:expr, $dest:expr, $lhs:ident: $tyl:ident, $rhs:ident: $tyr:ident, $e:expr) => {{
                rx!($self, $lhs: $tyl, $rhs: $tyr);
                $self.registers.write($dest, $e);
            }};
            ($self:expr, $($src:ident: $ty:ident),+) => {
                use $crate::ir::ArgTy;
                $(let $src = match $ty {
                    ArgTy::Reg => $self.registers.get(unsafe { $src.reg }),
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
        while let Some(&instr) = self.bc.code.get(self.program_counter) {
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
                Instruction::And { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => todo!(),
                Instruction::Or { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => todo!(),
                Instruction::Matches { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => todo!(),
                Instruction::MatchesNot { dest: _, lhs: _, rhs: _, tyr: _, tyl: _ } => todo!(),
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
                    self.registers.write(dest, Value::String(buf.into()));
                }
                Instruction::LoadA { dest: _, ty_place: _, start: _, end: _, var: _ } => todo!(),
                Instruction::StoreS { dest, ty_place, var, arg, ty } => {
                    rx!(self, arg: ty);
                    match ty_place {
                        ArgTy::UsVal => self.symbols.write_user_val(var, arg.clone()),
                        ArgTy::IsVal => todo!(),
                        _ => unreachable!(),
                    }
                    self.registers.write(dest, arg.clone());
                }
                Instruction::StoreR { dest: _, src: _, arg: _, ty: _, tys: _ } => {
                    todo!()
                }
                Instruction::StoreA {
                    dest: _,
                    ty_place: _,
                    start: _,
                    end: _,
                    var: _,
                    arg: _,
                } => todo!(),
                Instruction::IntrinsicCall { dest: _, start: _, end: _, name: _ } => todo!(),
                Instruction::OutputCall { start, end, cmd, redir } => {
                    self.intrinsic_print(start, end, cmd, redir);
                }
                Instruction::UserCall { dest: _, start: _, end: _, name: _ } => todo!(),
                Instruction::IndirectCall { dest: _, start: _, end: _, name: _, ty: _ } => todo!(),
                Instruction::Jump { to: Label(label) } => {
                    self.program_counter = label as _;
                    continue;
                }
                Instruction::Return { arg: _, ty: _ } => todo!(),
                Instruction::Branch { then_label, else_label, condition } => {
                    if self.registers.get(condition).to_bool() {
                        self.program_counter = then_label.0 as _;
                    } else {
                        self.program_counter = else_label.0 as _;
                    }
                    continue;
                }
            }
            self.program_counter += 1;
        }
    }

    fn intrinsic_print(&mut self, start: Reg, end: Reg, fun: Command, redir: Option<Redirection>) {
        let Command::Print = fun else { todo!() };
        let None = redir else { todo!() };
        let out = &mut io::stdout().lock();
        let range = self.registers.get_range(start..end);

        if range.is_empty() {
            let record = self.symbols.record(Value::Float(0.));
            self.write_fmt(out, format_args!("{record}"));
        } else {
            for reg in range {
                self.write_fmt(out, format_args!("{ofs}{reg}", ofs = self.symbols.ofs));
            }
        }
        self.write_fmt(out, format_args!("{rfs}", rfs = self.symbols.rfs));
    }

    fn write_fmt(&self, out: &mut impl Write, args: fmt::Arguments<'_>) {
        if let Err(e) = out.write_fmt(args)
            && e.kind() != io::ErrorKind::BrokenPipe
        {
            let _ = write!(
                io::stderr().lock(),
                "awk: warning: error writing to standard output: {e}"
            );
        }
    }
}

impl<'a> Registers<'a> {
    fn replace(&mut self, src: Reg, f: impl FnOnce(Value<'a>) -> Value<'a>) {
        let val = replace(self.get_mut(src), Value::Untyped);
        self.write(src, f(val));
    }
    fn get(&self, src: Reg) -> &Value<'a> {
        &self.0[src.0 as usize]
    }
    fn get_mut(&mut self, src: Reg) -> &mut Value<'a> {
        &mut self.0[src.0 as usize]
    }
    fn write(&mut self, dest: Reg, src: Value<'a>) {
        self.0[dest.0 as usize] = src;
    }
    fn get_range(&self, regs: Range<Reg>) -> &[Value<'a>] {
        &self.0[regs.start.0 as usize..regs.end.0 as _]
    }
}

impl Display for Interpreter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}\n", self.bc)?;
        writeln!(f, "{}\n", self.registers)?;
        writeln!(f, "{}\n", self.symbols)?;
        write!(f, "{}", self.consts)
    }
}

impl Display for Code<'_> {
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
            write!(f, "user[{i}] @ {k} = {v}")
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
