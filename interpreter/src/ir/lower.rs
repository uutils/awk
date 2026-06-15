// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{borrow::Cow, mem::forget, ops::Deref};

use bumpalo::{Bump, collections::Vec};
use either::Either;
use parser::{
    Atom, BinaryOperator, BinaryPlaceOperator, Body, Expr, ExprNode, Identifier, Place,
    SimpleStatement, Statement, UnaryOperator, UnaryPlaceOperator, Variable,
};

use crate::{
    ir::{Arg, ArgTy, Instruction, IxWidth, Label, NonLocal, Reg, RegWidth},
    types::Value,
    vm::{Consts, ExecMode, Interpreter, SymbolTable},
};

pub struct Code<'arena> {
    pub arena: &'arena Bump,
    pub bc: Bytecode<'arena>,
    pub consts: Consts<'arena>,
    pub symbols: SymbolTable<'arena>,
    free_regs: Vec<'arena, Reg>,
    pub reg_pointer: RegWidth,
}

#[must_use]
#[derive(Debug)]
#[repr(transparent)]
struct LinearReg(Reg);

#[derive(Clone, Copy)]
struct TypedArg(Arg, ArgTy);

#[must_use]
enum Operand {
    Imm(TypedArg),
    Reg(LinearReg),
}

impl<'a> Code<'a> {
    fn lower_body(&mut self, body: &Body) {
        for stmnt in &body.0 {
            self.lower_statement(stmnt);
        }
    }

    fn lower_statement(&mut self, stmnt: &Statement) {
        match stmnt {
            Statement::If { condition, then_body, else_body } => {
                let state = RegsState::new(self);
                let (if_label, _) = self.emit_branch(condition, |this| this.lower_body(then_body));

                if let Some(else_body) = else_body {
                    self.bc.nth(if_label).push_end_label();
                    self.emit_jump(|this| state.scope_hwm(this, |this| this.lower_body(else_body)));
                }
            }
            Statement::While { condition, then_body } => {
                let cond_label = self.following_instr(0);
                self.emit_branch(condition, |this| {
                    this.lower_body(then_body);
                    this.bc.emit(Instruction::Jump { to: cond_label });
                });
            }
            Statement::DoWhile { then_body, condition } => {
                let then_label = self.following_instr(0);
                self.lower_body(then_body);
                let cond_reg = self.alloc_reg();
                self.lower_expr_into(condition, *cond_reg);

                self.bc.emit(Instruction::Branch {
                    condition: *cond_reg,
                    then_label,
                    else_label: self.following_instr(1),
                });
                self.free_reg(cond_reg);
            }
            Statement::For { init, condition, update, body } => {
                if let Some(SimpleStatement::Expression(expr)) = init {
                    self.lower_expr(expr).free(self);
                }

                let cond_label = self.following_instr(0);
                if let Some(condition) = condition {
                    self.emit_branch(condition, |this| {
                        this.lower_body(body);
                        if let Some(SimpleStatement::Expression(expr)) = update {
                            this.lower_expr(expr).free(this);
                        }
                        this.bc.emit(Instruction::Jump { to: cond_label });
                    });
                } else {
                    self.lower_body(body);
                    if let Some(SimpleStatement::Expression(expr)) = update {
                        self.lower_expr(expr).free(self);
                    }
                    self.bc.emit(Instruction::Jump { to: cond_label });
                }
            }
            Statement::Simple(SimpleStatement::Expression(expr)) => {
                self.lower_expr(expr).free(self);
            }
            Statement::Simple(SimpleStatement::Command { name, args, redirection }) => {
                let (start, end, redir) = self.gen_call_convention(args, |this| {
                    redirection.as_ref().map(|(r, expr)| {
                        let redir_reg = this.alloc_reg();
                        this.lower_expr_into(expr, *redir_reg);
                        this.free_reg(redir_reg);
                        *r
                    })
                });
                self.bc
                    .emit(Instruction::OutputCall { start, end, cmd: *name, redir });
            }
            _ => todo!(),
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> Operand {
        match expr {
            Expr::Leaf(atom) => self.lower_atom(atom),
            Expr::Node(_) => {
                let dest = self.alloc_reg();
                self.lower_expr_into(expr, *dest);
                Operand::Reg(dest)
            }
        }
    }

    fn lower_atom(&mut self, atom: &Atom) -> Operand {
        let dest = self.alloc_reg();
        match self.lower_atom_arg(atom, *dest) {
            TypedArg(arg, ArgTy::Reg) => {
                let reg = unsafe { arg.reg };
                debug_assert_eq!(reg, *dest);
                Operand::Reg(dest)
            }
            imm => {
                self.free_reg(dest);
                Operand::Imm(imm)
            }
        }
    }

    fn lower_atom_arg(&mut self, atom: &Atom, dest: Reg) -> TypedArg {
        match atom {
            Atom::Variable(Variable::User(ident)) => TypedArg::new_us(self, ident),
            Atom::Variable(var) => TypedArg::new_is(var),
            &Atom::Integer(n) => TypedArg::new_imm(n),
            &Atom::Number(n) => TypedArg::new_immf(self, n),
            atom @ (Atom::String(s) | Atom::TypedRegex(s)) => {
                let val = if matches!(atom, Atom::String(_)) {
                    Value::String
                } else {
                    Value::Regex
                };
                let buf = self.arena.alloc_slice_copy(s.as_ref());
                TypedArg::new_cnt(self, val(Cow::Borrowed(buf)))
            }
            Atom::Regex(r) => {
                let buf = &*self.arena.alloc_slice_copy(r.as_ref());
                let TypedArg(rhs, tyr) = TypedArg::new_cnt(self, Value::Regex(buf.into()));
                let TypedArg(lhs, tyl) = TypedArg(Arg { imm: 0 }, ArgTy::Rec);
                self.bc
                    .emit(Instruction::Matches { dest, rhs, lhs, tyr, tyl });
                dest.into()
            }
            _ => todo!(),
        }
    }

    fn lower_atom_into(&mut self, atom: &Atom, dest: Reg) {
        let arg = self.lower_atom_arg(atom, dest);
        match arg {
            TypedArg(arg, ArgTy::Reg) if unsafe { arg.reg } == dest => {}
            TypedArg(arg, ty) => {
                self.bc.emit(Instruction::Copy { dest, arg, ty });
            }
        }
    }

    fn lower_expr_into(&mut self, expr: &Expr, dest: Reg) {
        match expr {
            Expr::Leaf(atom) => self.lower_atom_into(atom, dest),
            Expr::Node(node) => match node.as_ref() {
                ExprNode::UnaryOperation(op, expr) => {
                    let src = self.lower_expr(expr);
                    self.bc
                        .emit(Instruction::from_unary(*op, dest, src.to_arg()));
                    src.free(self);
                }
                ExprNode::BinaryOperation(op, lhs, rhs) => {
                    let lhs = self.lower_expr(lhs);
                    let rhs = self.lower_expr(rhs);
                    self.bc.emit(Instruction::from_binary(
                        *op,
                        dest,
                        lhs.to_arg(),
                        rhs.to_arg(),
                    ));
                    lhs.free(self);
                    rhs.free(self);
                }
                ExprNode::Ternary(condition, true_then, false_then) => {
                    let (if_label, state) = self.emit_branch(condition, |this| {
                        RegsState::new(this)
                            .scope(this, |this| this.lower_expr_into(true_then, dest))
                            .0
                    });
                    self.bc.nth(if_label).push_end_label();
                    self.emit_jump(|this| {
                        state.scope_hwm(this, |this| this.lower_expr_into(false_then, dest));
                    });
                }
                ExprNode::BinaryPlaceOperation(op, place, expr) => {
                    let val = self.lower_expr(expr);

                    let Some(bin_op) = lower_assign_ops(*op) else {
                        self.store_place(place, dest, val.to_arg());
                        val.free(self);
                        return;
                    };

                    let lhs_reg = self.alloc_reg();
                    let lhs = self.load_place(*lhs_reg, place);
                    let rhs = val.to_arg();

                    self.bc
                        .emit(Instruction::from_binary(bin_op, dest, lhs, rhs));
                    self.store_place(place, dest, dest.into());

                    self.free_reg(lhs_reg);
                    val.free(self);
                }
                ExprNode::UnaryPlaceOperation(op, place) => {
                    // Note: val may alias with dest.
                    let lhs = self.load_place(dest, place);
                    let one = TypedArg::new_imm(1);

                    match op {
                        UnaryPlaceOperator::IncrementL => {
                            self.bc.emit(Instruction::from_binary(
                                BinaryOperator::Add,
                                dest,
                                lhs,
                                one,
                            ));
                            self.store_place(place, dest, dest.into());
                        }
                        UnaryPlaceOperator::DecrementL => {
                            self.bc.emit(Instruction::from_binary(
                                BinaryOperator::Subtract,
                                dest,
                                lhs,
                                one,
                            ));
                            self.store_place(place, dest, dest.into());
                        }
                        UnaryPlaceOperator::IncrementR | UnaryPlaceOperator::DecrementR => {
                            self.bc.emit(Instruction::from_binary(
                                BinaryOperator::Add,
                                dest,
                                lhs,
                                TypedArg::new_imm(0),
                            ));
                            let tmp = self.alloc_reg();
                            let update_op = match op {
                                UnaryPlaceOperator::IncrementR => BinaryOperator::Add,
                                UnaryPlaceOperator::DecrementR => BinaryOperator::Subtract,
                                _ => unreachable!(),
                            };
                            self.bc
                                .emit(Instruction::from_binary(update_op, *tmp, lhs, one));
                            self.store_place(place, *tmp, (*tmp).into());
                            self.free_reg(tmp);
                        }
                    }
                }
                _ => todo!(),
            },
        }
    }

    fn load_place(&mut self, dest: Reg, place: &Place<'_>) -> TypedArg {
        match place {
            Place::Record(_) => {
                todo!()
            }
            Place::Variable(Variable::User(ident)) => TypedArg::new_us(self, ident),
            Place::Variable(var) => TypedArg::new_is(var),
            Place::Index(Variable::User(ident), index) => {
                let var = self.symbols.register_user_var(ident, self.arena);
                let (start, end, _) = self.gen_call_convention(index, |_| ());
                self.bc
                    .emit(Instruction::LoadA { dest, ty_place: ArgTy::UaVal, start, end, var });
                TypedArg(Arg { sym: var }, ArgTy::UaVal)
            }
            Place::Index(var, index) => {
                let (start, end, _) = self.gen_call_convention(index, |_| ());
                let var = var_index(var);
                self.bc
                    .emit(Instruction::LoadA { dest, ty_place: ArgTy::UaVal, start, end, var });
                TypedArg(Arg { sym: var }, ArgTy::IaVal)
            }
            Place::ChainedIndex(_, _) => todo!(),
        }
    }

    fn store_place(&mut self, place: &Place<'_>, dest: Reg, src: TypedArg) {
        let TypedArg(arg, ty) = src;
        match place {
            Place::Record(expr) => {
                let rec = self.lower_expr(expr);
                let TypedArg(src, tys) = rec.to_arg();
                self.bc
                    .emit(Instruction::StoreR { dest, src, tys, arg, ty });
                rec.free(self);
            }
            Place::Variable(Variable::User(ident)) => {
                let var = self.symbols.register_user_var(ident, self.arena);
                self.bc
                    .emit(Instruction::StoreS { dest, ty_place: ArgTy::UsVal, var, arg, ty });
            }
            Place::Variable(var) => {
                let var = var_index(var);
                self.bc
                    .emit(Instruction::StoreS { dest, ty_place: ArgTy::IsVal, var, arg, ty });
            }
            Place::Index(Variable::User(ident), index) => {
                let var = self.symbols.register_user_var(ident, self.arena);
                let (start, end, _) = self.gen_call_convention(index, |_| ());
                let arg = self.spill_to_reg(src);
                self.bc.emit(Instruction::StoreA {
                    dest,
                    start,
                    end,
                    var,
                    ty_place: ArgTy::UaVal,
                    arg: arg.as_ref().either_into(),
                });
                arg.map_right(|r| self.free_reg(r));
            }
            Place::Index(var, index) => {
                let (start, end, _) = self.gen_call_convention(index, |_| ());
                let var = var_index(var);
                let arg = self.spill_to_reg(src);
                self.bc.emit(Instruction::StoreA {
                    dest,
                    start,
                    end,
                    var,
                    ty_place: ArgTy::IaVal,
                    arg: arg.as_ref().either_into(),
                });
                arg.map_right(|r| self.free_reg(r));
            }
            Place::ChainedIndex(_, _) => todo!(),
        }
    }

    fn emit_branch<T>(
        &mut self,
        condition_expr: &Expr<'_>,
        cb: impl FnOnce(&mut Self) -> T,
    ) -> (Label, T) {
        let condition = self.alloc_reg();
        self.lower_expr_into(condition_expr, *condition);
        let then_label = self.following_instr(1);
        let if_label = self.bc.emit(Instruction::br(*condition, then_label));
        self.free_reg(condition);

        let res = cb(self);
        let next = self.following_instr(0);
        self.bc.nth(if_label).set_label(next);

        (if_label, res)
    }

    fn emit_jump<T>(&mut self, cb: impl FnOnce(&mut Self) -> T) -> T {
        let label = self.bc.emit(Instruction::Jump { to: Label(0) });
        let res = cb(self);
        let next = self.following_instr(0);
        self.bc.nth(label).set_label(next);
        res
    }

    fn alloc_reg(&mut self) -> LinearReg {
        self.free_regs.pop().map(LinearReg).unwrap_or_else(|| {
            let current = self.reg_pointer;
            self.reg_pointer = self.reg_pointer.checked_add(1).expect("register overflow");
            LinearReg(Reg(current))
        })
    }

    fn gen_call_convention<T>(
        &mut self,
        args: &[Expr<'_>],
        extra: impl FnOnce(&mut Code) -> T,
    ) -> (Reg, Reg, T) {
        RegsState::new(self)
            .scope(self, |this| {
                let call_start = this.reg_pointer;
                // TODO: Nicer error reporting.
                let args_len = RegWidth::try_from(args.len()).expect("too many call args");
                let call_end = call_start.checked_add(args_len).expect("register overflow");

                this.reg_pointer = call_end;
                for (i, arg) in args.iter().enumerate() {
                    let offset = i as RegWidth;
                    let reg = Reg(call_start.checked_add(offset).expect("register overflow"));
                    this.lower_expr_into(arg, reg);
                }
                (Reg(call_start), Reg(call_end), extra(this))
            })
            .1
    }

    fn spill_to_reg(&mut self, TypedArg(arg, ty): TypedArg) -> Either<Reg, LinearReg> {
        if matches!(ty, ArgTy::Reg) {
            Either::Left(unsafe { arg.reg })
        } else {
            let dest = self.alloc_reg();
            self.bc.emit(Instruction::Copy { dest: *dest, arg, ty });
            Either::Right(dest)
        }
    }

    fn free_reg(&mut self, reg: LinearReg) {
        self.free_regs.push(reg.into_inner());
    }

    fn register_const(&mut self, value: Value<'a>) -> NonLocal {
        NonLocal(self.consts.0.insert_full(value).0 as IxWidth)
    }

    fn following_instr(&self, nth: IxWidth) -> Label {
        Label(self.bc.len() + nth)
    }
}

#[derive(Clone)]
pub struct Bytecode<'a> {
    pub code: Vec<'a, Instruction>,
}

#[derive(Clone, Debug)]
struct RegsState {
    reg_pointer: RegWidth,
    n_free_regs: usize,
}

impl<'a> Bytecode<'a> {
    fn new_in(bump: &'a Bump) -> Self {
        Self { code: Vec::with_capacity_in(64, bump) }
    }

    #[inline(always)]
    fn emit(&mut self, code: Instruction) -> Label {
        self.code.push(code);
        Label((self.code.len() - 1) as IxWidth)
    }

    fn len(&self) -> IxWidth {
        self.code.len() as IxWidth
    }

    fn nth(&mut self, label: Label) -> &mut Instruction {
        &mut self.code[label.0 as usize]
    }
}

impl RegsState {
    fn new(code: &Code) -> Self {
        Self {
            reg_pointer: code.reg_pointer,
            n_free_regs: code.free_regs.len(),
        }
    }

    fn scope<T>(self, code: &mut Code, f: impl FnOnce(&mut Code) -> T) -> (Self, T) {
        let ret = f(code);
        let old = code.reg_pointer;
        code.reg_pointer = self.reg_pointer;
        code.free_regs.truncate(self.n_free_regs);
        (Self { reg_pointer: old, ..self }, ret)
    }

    fn scope_hwm<T>(self, code: &mut Code, f: impl FnOnce(&mut Code) -> T) {
        f(code);
        code.reg_pointer = code.reg_pointer.max(self.reg_pointer);
        code.free_regs.truncate(self.n_free_regs);
    }
}

pub fn test_interpreter(stmnt: &Body<'_>) -> String {
    let bump = Bump::with_capacity(16384);
    let mut c = Code {
        arena: &bump,
        bc: Bytecode::new_in(&bump),
        consts: Consts::new_in(&bump),
        symbols: SymbolTable::new_in(&bump),
        reg_pointer: 0,
        free_regs: Vec::new_in(&bump),
    };
    c.lower_body(stmnt);
    let mut vm = Interpreter::new(ExecMode::Uu, c);
    vm.run();
    vm.to_string()
}

fn lower_assign_ops(op: BinaryPlaceOperator) -> Option<BinaryOperator> {
    match op {
        BinaryPlaceOperator::Assignment => None,
        BinaryPlaceOperator::AddAssign => Some(BinaryOperator::Add),
        BinaryPlaceOperator::SubAssign => Some(BinaryOperator::Subtract),
        BinaryPlaceOperator::MulAssign => Some(BinaryOperator::Multiply),
        BinaryPlaceOperator::DivAssign => Some(BinaryOperator::Divide),
        BinaryPlaceOperator::PowAssign => Some(BinaryOperator::Raise),
        BinaryPlaceOperator::ModAssign => Some(BinaryOperator::Modulo),
    }
}

impl Instruction {
    fn from_unary(op: UnaryOperator, dest: Reg, TypedArg(arg, ty): TypedArg) -> Self {
        match op {
            UnaryOperator::Record => Self::Record { dest, arg, ty },
            UnaryOperator::Negation => Self::Negation { dest, arg, ty },
            UnaryOperator::ToInt => Self::ToInt { dest, arg, ty },
            UnaryOperator::Negative => Self::Negative { dest, arg, ty },
        }
    }

    fn from_binary(
        op: BinaryOperator,
        dest: Reg,
        TypedArg(lhs, tyl): TypedArg,
        TypedArg(rhs, tyr): TypedArg,
    ) -> Self {
        match op {
            BinaryOperator::Concat => Self::Concat { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Eq => Self::Eq { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::NEq => Self::NEq { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Gt => Self::Gt { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Lt => Self::Lt { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::LtE => Self::LtE { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::GtE => Self::GtE { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::And => Self::And { dest, lhs, rhs, tyl, tyr },
            BinaryOperator::Or => Self::Or { dest, lhs, rhs, tyl, tyr },
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

impl LinearReg {
    fn into_inner(self) -> Reg {
        let inner = self.0;
        forget(self);
        inner
    }
}

impl Operand {
    fn to_arg(&self) -> TypedArg {
        match self {
            &Self::Imm(imm) => imm,
            Self::Reg(reg) => reg.0.into(),
        }
    }

    fn free(self, code: &mut Code) {
        if let Self::Reg(reg) = self {
            code.free_reg(reg);
        }
    }
}

impl TypedArg {
    fn new_us(code: &mut Code<'_>, ident: &Identifier<'_>) -> Self {
        let sym = code.symbols.register_user_var(ident, code.arena);
        Self(Arg { sym }, ArgTy::UsVal)
    }

    fn new_is(var: &Variable<'_>) -> Self {
        Self(Arg { sym: var_index(var) }, ArgTy::IsVal)
    }

    fn new_imm(imm: i32) -> Self {
        Self(Arg { imm }, ArgTy::Imm)
    }

    fn new_immf(code: &mut Code<'_>, n: f64) -> Self {
        let sym = code.register_const(Value::Float(n));
        Self(Arg { sym }, ArgTy::ImmF)
    }

    fn new_cnt<'a>(code: &mut Code<'a>, val: Value<'a>) -> Self {
        let sym = code.register_const(val);
        Self(Arg { sym }, ArgTy::Cnt)
    }
}

impl From<Reg> for TypedArg {
    fn from(reg: Reg) -> Self {
        Self(Arg { reg }, ArgTy::Reg)
    }
}

impl Deref for LinearReg {
    type Target = Reg;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn var_index(var: &Variable<'_>) -> NonLocal {
    // SAFETY: it is repr(RegWidth).
    unsafe { *<*const Variable>::from(var).cast::<NonLocal>() }
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

// HACK: Either::either_into from a ref.
impl From<&Self> for Reg {
    fn from(value: &Self) -> Self {
        *value
    }
}
