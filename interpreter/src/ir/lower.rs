// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{borrow::Cow, mem::forget, ops::Deref, vec::Vec as StdVec};

use bumpalo::{Bump, collections::Vec};
use either::Either;
use indexmap_allocator_api::IndexSet;
use parser::{
    ArrayOperator, Ast, Atom, BinaryOperator, BinaryPlaceOperator, Body, Command, Expr, ExprNode,
    Identifier, MetaId, Place, Rule, RulePattern, SimpleStatement, Statement, UnaryOperator,
    UnaryPlaceOperator, Variable,
};

use crate::{
    CodeRange,
    ir::{Arg, ArgTy, Instruction, IxWidth, Label, NonLocal, Reg, RegWidth},
    types::Value,
    vm::{Consts, SymbolTable},
};

pub struct CodeGen<'a> {
    pub(crate) arena: &'a Bump,
    pub(crate) bc: Bytecode<'a>,
    pub(crate) consts: Consts<'a>,
    pub(crate) symbols: SymbolTable<'a>,
    free_regs: Vec<'a, Reg>,
    pub(crate) reg_pointer: RegWidth,
    current_metadata: MetaId,
    break_exits: Option<StdVec<Label>>,
    continue_label: Option<Label>,
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

impl<'a> CodeGen<'a> {
    #[inline(always)]
    fn emit(&mut self, code: Instruction) -> Label {
        self.bc.code.push(code);
        self.bc.metadata.push(self.current_metadata);
        Label((self.bc.code.len() - 1) as IxWidth)
    }

    pub fn new(arena: &'a Bump) -> Self {
        Self {
            arena,
            bc: Bytecode::with_capacity_in(64, arena),
            consts: Consts(IndexSet::new_in(arena)),
            symbols: SymbolTable::new_in(arena),
            free_regs: Vec::new_in(arena),
            reg_pointer: 0,
            current_metadata: MetaId::default(),
            break_exits: None,
            continue_label: None,
        }
    }

    pub fn lower_ast(&mut self, ast: &Ast) {
        self.bc.begin_label = self.lower_special_rules(&ast.begin);
        self.bc.begin_file_label = self.lower_special_rules(&ast.begin_file);
        self.bc.end_file_label = self.lower_special_rules(&ast.end_file);
        self.bc.end_label = self.lower_special_rules(&ast.end);
        self.bc.rules_label = Label(self.bc.len());

        for rule in &ast.rules {
            self.lower_rule(rule);
        }
    }

    fn lower_special_rules(&mut self, rules: &[Body]) -> Label {
        let start_label = Label(self.bc.len());
        for body in rules {
            self.lower_body(body);
        }
        start_label
    }

    fn lower_rule(&mut self, Rule { pattern, actions }: &Rule) -> Label {
        let start_label = Label(self.bc.len());
        if let Some(pattern) = pattern {
            match pattern {
                RulePattern::Expression(expr) => {
                    self.emit_branch(expr, |this| this.lower_actions(actions.as_ref()));
                }
                RulePattern::Range(_, _) => todo!(),
            }
        } else {
            self.lower_actions(actions.as_ref());
        }
        start_label
    }

    fn lower_actions(&mut self, actions: Option<&Body>) {
        if let Some(actions) = actions {
            self.lower_body(actions);
        } else {
            let reg = self.alloc_reg();

            let TypedArg(arg, ty) = TypedArg::new_imm(0);
            self.emit(Instruction::Record { dest: *reg, arg, ty });

            self.emit(Instruction::OutputCall {
                start: *reg,
                end: Reg((*reg).0 + 1),
                cmd: Command::Print,
                redir: None,
            });

            self.free_reg(reg);
        }
    }

    pub fn set_value(&mut self, var: &Identifier<'_>, value: &str) {
        self.symbols.register_user_var_with(var, value, self.arena);
    }

    fn lower_body(&mut self, body: &Body) {
        for stmnt in &body.0 {
            self.lower_statement(stmnt);
        }
    }

    fn lower_statement(&mut self, stmnt: &Statement) {
        match stmnt {
            Statement::If { condition, then_body, else_body, metadata } => {
                self.with_metadata(*metadata, |this| {
                    let state = RegsState::new(this);
                    let (if_label, _) =
                        this.emit_branch(condition, |this| this.lower_body(then_body));

                    if let Some(else_body) = else_body {
                        this.bc.nth(if_label).push_end_label();
                        this.emit_jump(|this| {
                            state.scope_hwm(this, |this| this.lower_body(else_body));
                        });
                    }
                });
            }
            Statement::While { condition, then_body, metadata } => {
                self.with_metadata(*metadata, |this| {
                    let cond_label = this.following_instr(0);
                    this.emit_branch(condition, |this| {
                        // Wrap loop-back so `break` jumps past the entire loop.
                        this.with_break_scope(|this| {
                            this.with_continue_label(cond_label, |this| {
                                this.lower_body(then_body);
                            });
                            this.emit(Instruction::Jump { to: cond_label });
                        });
                    });
                });
            }
            Statement::DoWhile { then_body, condition, metadata } => {
                self.with_metadata(*metadata, |this| {
                    // Layout so the continue target (condition) is known before the body:
                    //   jmp body
                    // continue:
                    //   cond; brif body, end
                    // body:
                    //   ...; jmp continue
                    this.with_break_scope(|this| {
                        let to_body = this.emit(Instruction::Jump { to: Label(0) });
                        let continue_label = this.following_instr(0);

                        let cond_reg = this.alloc_reg();
                        this.lower_expr_into(condition, *cond_reg);
                        let branch = this.emit(Instruction::Branch {
                            condition: *cond_reg,
                            then_label: Label(0),
                            else_label: Label(0),
                        });
                        this.free_reg(cond_reg);

                        let body_label = this.following_instr(0);
                        this.bc.nth(to_body).set_label(body_label);
                        this.bc.nth(branch).set_then_label(body_label);

                        this.with_continue_label(continue_label, |this| {
                            this.lower_body(then_body);
                        });
                        this.emit(Instruction::Jump { to: continue_label });

                        let end_label = this.following_instr(0);
                        this.bc.nth(branch).set_label(end_label);
                    });
                });
            }
            Statement::For { init, condition, update, body, metadata } => {
                self.with_metadata(*metadata, |this| {
                    if let Some(SimpleStatement::Expression(expr, metadata)) = init {
                        this.with_metadata(*metadata, |this| this.lower_expr(expr).free(this));
                    }

                    // Layout so continue jumps to the update clause (gawk/POSIX):
                    //   init; jmp cond
                    // continue: update
                    // cond: brif body / end; body; jmp continue
                    let to_cond = this.emit(Instruction::Jump { to: Label(0) });
                    let continue_label = this.following_instr(0);
                    if let Some(SimpleStatement::Expression(expr, metadata)) = update {
                        this.with_metadata(*metadata, |this| {
                            this.lower_expr(expr).free(this);
                        });
                    }
                    let cond_label = this.following_instr(0);
                    this.bc.nth(to_cond).set_label(cond_label);

                    if let Some(condition) = condition {
                        this.emit_branch(condition, |this| {
                            this.with_break_scope(|this| {
                                this.with_continue_label(continue_label, |this| {
                                    this.lower_body(body);
                                });
                                this.emit(Instruction::Jump { to: continue_label });
                            });
                        });
                    } else {
                        this.with_break_scope(|this| {
                            this.with_continue_label(continue_label, |this| {
                                this.lower_body(body);
                            });
                            this.emit(Instruction::Jump { to: continue_label });
                        });
                    }
                });
            }
            Statement::Simple(SimpleStatement::Expression(expr, metadata)) => {
                self.with_metadata(*metadata, |this| {
                    this.lower_expr(expr).free(this);
                });
            }
            Statement::Simple(SimpleStatement::Command { name, args, redirection, metadata }) => {
                self.with_metadata(*metadata, |this| {
                    let (start, end, redir) = this.gen_call_convention(args, |this| {
                        redirection.as_ref().map(|(r, expr)| {
                            let redir_reg = this.alloc_reg();
                            this.lower_expr_into(expr, *redir_reg);
                            this.free_reg(redir_reg);
                            *r
                        })
                    });
                    this.emit(Instruction::OutputCall { start, end, cmd: *name, redir });
                });
            }
            Statement::Simple(SimpleStatement::Delete(..)) => todo!(),
            Statement::Switch { scrutinee, branches, default, metadata } => {
                self.with_metadata(*metadata, |this| {
                    this.lower_switch(scrutinee, branches, default.as_ref());
                });
            }
            Statement::ForEach { .. } => todo!(),
            Statement::Break(metadata) => {
                self.with_metadata(*metadata, |this| {
                    // Parser rejects `break` outside a loop or switch (#75).
                    let jump = this.emit(Instruction::Jump { to: Label(0) });
                    match this.break_exits.as_mut() {
                        Some(exits) => exits.push(jump),
                        None => unreachable!("break outside loop or switch"),
                    }
                });
            }
            Statement::Continue(metadata) => {
                self.with_metadata(*metadata, |this| {
                    // Parser rejects `continue` outside a loop (#75).
                    let Some(to) = this.continue_label else {
                        unreachable!("continue outside loop");
                    };
                    this.emit(Instruction::Jump { to });
                });
            }
            Statement::Exit(Some(expr), metadata) => {
                self.with_metadata(*metadata, |this| {
                    let dest = this.alloc_reg();
                    this.lower_expr_into(expr, *dest);

                    let TypedArg(arg, ty) = (*dest).into();
                    this.emit(Instruction::Exit { arg, ty });

                    this.free_reg(dest);
                });
            }
            Statement::Exit(None, metadata) => {
                self.with_metadata(*metadata, |this| {
                    let TypedArg(arg, ty) = TypedArg::new_imm(0);
                    this.emit(Instruction::Exit { arg, ty });
                });
            }
            Statement::Return(Some(expr), metadata) => {
                self.with_metadata(*metadata, |this| {
                    let dest = this.alloc_reg();
                    this.lower_expr_into(expr, *dest);

                    let TypedArg(arg, ty) = (*dest).into();
                    this.emit(Instruction::Return { arg, ty });

                    this.free_reg(dest);
                });
            }
            Statement::Return(None, metadata) => {
                self.with_metadata(*metadata, |this| {
                    this.emit(Instruction::ReturnUnassigned);
                });
            }
            Statement::Next(metadata) => {
                self.with_metadata(*metadata, |this| {
                    this.emit(Instruction::Next);
                });
            }
            Statement::NextFile(metadata) => {
                self.with_metadata(*metadata, |this| {
                    this.emit(Instruction::NextFile);
                });
            }
        }
    }

    fn lower_switch(
        &mut self,
        scrutinee: &Expr<'_>,
        branches: &[(Atom<'_>, Body<'_>)],
        default: Option<&(Body<'_>, usize)>,
    ) {
        let scr = self.alloc_reg();
        self.lower_expr_into(scrutinee, *scr);
        let cmp = self.alloc_reg();

        let default_pos = default.map_or(branches.len(), |(_, pos)| *pos);
        let mut pending_branches = Vec::new_in(self.arena);
        for (i, (atom, _)) in branches.iter().enumerate() {
            pending_branches.push(self.emit_switch_case_match(*scr, *cmp, atom, i));
        }
        let no_match_jump = self.emit(Instruction::Jump { to: Label(0) });

        let mut case_labels = Vec::with_capacity_in(branches.len(), self.arena);
        case_labels.resize(branches.len(), Label(0));
        let mut default_label = None;

        // One break scope for all cases so `break` skips fall-through (gawk/C).
        self.with_break_scope(|this| {
            for (i, (_, body)) in branches.iter().enumerate().take(default_pos) {
                case_labels[i] = Label(this.bc.len());
                this.lower_body(body);
            }

            if let Some((body, _)) = default {
                default_label = Some(Label(this.bc.len()));
                this.lower_body(body);
            }

            for (i, (_, body)) in branches.iter().enumerate().skip(default_pos) {
                case_labels[i] = Label(this.bc.len());
                this.lower_body(body);
            }
        });

        let end_switch = self.following_instr(0);

        for (br_label, case_ix) in pending_branches {
            self.bc.nth(br_label).set_then_label(case_labels[case_ix]);
        }

        let no_match_target = default_label.unwrap_or(end_switch);
        self.bc.nth(no_match_jump).set_label(no_match_target);
        self.free_reg(cmp);
        self.free_reg(scr);
    }

    fn emit_switch_case_match(
        &mut self,
        scr: Reg,
        cmp: Reg,
        case: &Atom<'_>,
        case_ix: usize,
    ) -> (Label, usize) {
        let lhs = TypedArg::from(scr);
        let tyl = ArgTy::Reg;

        match case {
            Atom::Regex(r) | Atom::TypedRegex(r) => {
                let buf = &*self.arena.alloc_slice_copy(r.as_ref());
                let TypedArg(rhs, tyr) = TypedArg::new_cnt(self, Value::Regex(buf.into()));
                self.emit(Instruction::Matches { dest: cmp, lhs: lhs.0, rhs, tyl, tyr });
            }
            atom => {
                let case_val = self.lower_atom(atom);
                let TypedArg(rhs, tyr) = case_val.to_arg();
                self.emit(Instruction::Eq { dest: cmp, lhs: lhs.0, rhs, tyl, tyr });
                case_val.free(self);
            }
        }

        let br_label = self.emit(Instruction::Branch {
            condition: cmp,
            then_label: Label(0),
            else_label: self.following_instr(1),
        });
        (br_label, case_ix)
    }

    fn lower_expr(&mut self, expr: &Expr) -> Operand {
        match expr {
            Expr::Leaf(atom, metadata) => {
                self.with_metadata(*metadata, |this| this.lower_atom(atom))
            }
            Expr::Node(_, _) => {
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
                self.emit(Instruction::Matches { dest, rhs, lhs, tyr, tyl });
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
                self.emit(Instruction::Copy { dest, arg, ty });
            }
        }
    }

    fn lower_expr_into(&mut self, expr: &Expr, dest: Reg) {
        match expr {
            Expr::Leaf(atom, metadata) => {
                self.with_metadata(*metadata, |this| this.lower_atom_into(atom, dest));
            }
            Expr::Node(node, metadata) => {
                self.with_metadata(*metadata, |this| {
                    match node.as_ref() {
                        ExprNode::UnaryOperation(op, expr) => {
                            let src = this.lower_expr(expr);
                            this.emit(Instruction::from_unary(*op, dest, src.to_arg()));
                            src.free(this);
                        }
                        ExprNode::BinaryOperation(op, lhs, rhs) => match op {
                            BinaryOperator::And => this.lower_and_into(lhs, rhs, dest),
                            BinaryOperator::Or => this.lower_or_into(lhs, rhs, dest),
                            _ => {
                                let lhs = this.lower_expr(lhs);
                                let rhs = this.lower_expr(rhs);
                                this.emit(Instruction::from_binary(
                                    *op,
                                    dest,
                                    lhs.to_arg(),
                                    rhs.to_arg(),
                                ));
                                lhs.free(this);
                                rhs.free(this);
                            }
                        },
                        ExprNode::Ternary(condition, true_then, false_then) => {
                            let (if_label, state) = this.emit_branch(condition, |this| {
                                RegsState::new(this)
                                    .scope(this, |this| this.lower_expr_into(true_then, dest))
                                    .0
                            });
                            this.bc.nth(if_label).push_end_label();
                            this.emit_jump(|this| {
                                state
                                    .scope_hwm(this, |this| this.lower_expr_into(false_then, dest));
                            });
                        }
                        ExprNode::BinaryPlaceOperation(op, place, expr) => {
                            let val = this.lower_expr(expr);

                            let Some(bin_op) = lower_assign_ops(*op) else {
                                this.store_place(place, dest, val.to_arg());
                                val.free(this);
                                return;
                            };

                            let lhs_reg = this.alloc_reg();
                            let lhs = this.load_place(*lhs_reg, place);
                            let rhs = val.to_arg();

                            this.emit(Instruction::from_binary(bin_op, dest, lhs, rhs));
                            this.store_place(place, dest, dest.into());

                            this.free_reg(lhs_reg);
                            val.free(this);
                        }
                        ExprNode::UnaryPlaceOperation(op, place) => {
                            // Note: val may alias with dest.
                            let lhs = this.load_place(dest, place);
                            let one = TypedArg::new_imm(1);

                            match op {
                                UnaryPlaceOperator::IncrementL => {
                                    this.emit(Instruction::from_binary(
                                        BinaryOperator::Add,
                                        dest,
                                        lhs,
                                        one,
                                    ));
                                    this.store_place(place, dest, dest.into());
                                }
                                UnaryPlaceOperator::DecrementL => {
                                    this.emit(Instruction::from_binary(
                                        BinaryOperator::Subtract,
                                        dest,
                                        lhs,
                                        one,
                                    ));
                                    this.store_place(place, dest, dest.into());
                                }
                                UnaryPlaceOperator::IncrementR | UnaryPlaceOperator::DecrementR => {
                                    this.emit(Instruction::from_binary(
                                        BinaryOperator::Add,
                                        dest,
                                        lhs,
                                        TypedArg::new_imm(0),
                                    ));
                                    let tmp = this.alloc_reg();
                                    let update_op = match op {
                                        UnaryPlaceOperator::IncrementR => BinaryOperator::Add,
                                        UnaryPlaceOperator::DecrementR => BinaryOperator::Subtract,
                                        _ => unreachable!(),
                                    };
                                    this.emit(Instruction::from_binary(update_op, *tmp, lhs, one));
                                    this.store_place(place, *tmp, (*tmp).into());
                                    this.free_reg(tmp);
                                }
                            }
                        }
                        ExprNode::Parenthesized(expr) => this.lower_expr_into(expr, dest),
                        ExprNode::ArrayOperation(ArrayOperator::Index, var, index) => {
                            this.load_index(dest, var, index);
                        }
                        _ => todo!(),
                    }
                });
            }
        }
    }

    fn load_place(&mut self, dest: Reg, place: &Place<'_>) -> TypedArg {
        match place {
            Place::Record(_) => {
                todo!()
            }
            Place::Variable(Variable::User(ident)) => TypedArg::new_us(self, ident),
            Place::Variable(var) => TypedArg::new_is(var),
            Place::Index(var, index) => self.load_index(dest, var, index),
            Place::ChainedIndex(_, _) => todo!(),
        }
    }

    fn load_index(&mut self, dest: Reg, var: &Variable<'_>, index: &[Expr<'_>]) -> TypedArg {
        if let Variable::User(ident) = var {
            let var = self.symbols.register_user_var(ident, self.arena);
            let (start, end, _) = self.gen_call_convention(index, |_| ());
            self.emit(Instruction::LoadA { dest, ty_place: ArgTy::UaVal, start, end, var });
        } else {
            let (start, end, _) = self.gen_call_convention(index, |_| ());
            let var = var_index(var);
            self.emit(Instruction::LoadA { dest, ty_place: ArgTy::IaVal, start, end, var });
        }
        // Element value was written to `dest`; subsequent ops must use the register.
        dest.into()
    }

    fn store_place(&mut self, place: &Place<'_>, dest: Reg, src: TypedArg) {
        let TypedArg(arg, ty) = src;
        match place {
            Place::Record(expr) => {
                let rec = self.lower_expr(expr);
                let TypedArg(src, tys) = rec.to_arg();
                self.emit(Instruction::StoreR { dest, src, tys, arg, ty });
                rec.free(self);
            }
            Place::Variable(Variable::User(ident)) => {
                let var = self.symbols.register_user_var(ident, self.arena);
                self.emit(Instruction::StoreS { dest, ty_place: ArgTy::UsVal, var, arg, ty });
            }
            Place::Variable(var) => {
                let var = var_index(var);
                self.emit(Instruction::StoreS { dest, ty_place: ArgTy::IsVal, var, arg, ty });
            }
            Place::Index(Variable::User(ident), index) => {
                let var = self.symbols.register_user_var(ident, self.arena);
                let (start, end, _) = self.gen_call_convention(index, |_| ());
                let arg = self.spill_to_reg(src);
                self.emit(Instruction::StoreA {
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
                self.emit(Instruction::StoreA {
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

    fn lower_and_into(&mut self, lhs: &Expr<'_>, rhs: &Expr<'_>, dest: Reg) {
        let (if_label, _) = self.emit_branch(lhs, |this| {
            let rhs_reg = this.alloc_reg();
            this.lower_expr_into(rhs, *rhs_reg);
            this.truthify(dest, *rhs_reg);
            this.free_reg(rhs_reg);
        });
        self.bc.nth(if_label).push_end_label();
        self.emit_jump(|this| {
            let TypedArg(arg, ty) = TypedArg::new_imm(0);
            this.emit(Instruction::Copy { dest, arg, ty });
        });
    }

    fn lower_or_into(&mut self, lhs: &Expr<'_>, rhs: &Expr<'_>, dest: Reg) {
        let (if_label, _) = self.emit_branch(lhs, |this| {
            let TypedArg(arg, ty) = TypedArg::new_imm(1);
            this.emit(Instruction::Copy { dest, arg, ty });
        });
        self.bc.nth(if_label).push_end_label();
        self.emit_jump(|this| {
            let rhs_reg = this.alloc_reg();
            this.lower_expr_into(rhs, *rhs_reg);
            this.truthify(dest, *rhs_reg);
            this.free_reg(rhs_reg);
        });
    }

    /// Coerce `src` to an integer truth value (0 or 1), as gawk does via `mkbool()`.
    fn truthify(&mut self, dest: Reg, src: Reg) {
        let TypedArg(arg, ty) = src.into();
        self.emit(Instruction::Negation { dest, arg, ty });

        let TypedArg(arg, ty) = dest.into();
        self.emit(Instruction::Negation { dest, arg, ty });
    }

    fn emit_branch<T>(
        &mut self,
        condition_expr: &Expr<'_>,
        cb: impl FnOnce(&mut Self) -> T,
    ) -> (Label, T) {
        let condition = self.alloc_reg();
        self.lower_expr_into(condition_expr, *condition);
        let then_label = self.following_instr(1);
        let if_label = self.emit(Instruction::br(*condition, then_label));
        self.free_reg(condition);

        let res = cb(self);
        let next = self.following_instr(0);
        self.bc.nth(if_label).set_label(next);

        (if_label, res)
    }

    fn emit_jump<T>(&mut self, cb: impl FnOnce(&mut Self) -> T) -> T {
        let label = self.emit(Instruction::Jump { to: Label(0) });
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
        extra: impl FnOnce(&mut CodeGen) -> T,
    ) -> (Reg, Reg, T) {
        let (state, ret) = RegsState::new(self).scope(self, |this| {
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
        });
        self.reg_pointer = self.reg_pointer.max(state.reg_pointer);
        ret
    }

    fn spill_to_reg(&mut self, TypedArg(arg, ty): TypedArg) -> Either<Reg, LinearReg> {
        if matches!(ty, ArgTy::Reg) {
            Either::Left(unsafe { arg.reg })
        } else {
            let dest = self.alloc_reg();
            self.emit(Instruction::Copy { dest: *dest, arg, ty });
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

    pub fn bytecode(&mut self) -> Bytecode<'a> {
        std::mem::replace(&mut self.bc, Bytecode::with_capacity_in(0, self.arena))
    }

    fn with_metadata<R>(&mut self, metadata: MetaId, f: impl FnOnce(&mut Self) -> R) -> R {
        let old = self.current_metadata;
        self.current_metadata = metadata;
        let ret = f(self);
        self.current_metadata = old;
        ret
    }

    /// Run `f` as the body of a loop/`switch`, patching `break` jumps to the end.
    fn with_break_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.break_exits.replace(StdVec::new());
        let ret = f(self);
        let end = self.following_instr(0);
        if let Some(exits) = self.break_exits.take() {
            for jump in exits {
                self.bc.nth(jump).set_label(end);
            }
        }
        self.break_exits = prev;
        ret
    }

    /// Set the continue target for the duration of `f`, restoring the previous value after.
    fn with_continue_label<R>(&mut self, label: Label, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.continue_label.replace(label);
        let ret = f(self);
        self.continue_label = prev;
        ret
    }
}

#[derive(Debug)]
pub struct Bytecode<'a> {
    pub code: Vec<'a, Instruction>,
    pub metadata: StdVec<MetaId>,
    pub(crate) begin_label: Label,
    pub(crate) begin_file_label: Label,
    pub(crate) end_file_label: Label,
    pub(crate) end_label: Label,
    pub(crate) rules_label: Label,
}

#[derive(Clone, Debug)]
struct RegsState {
    reg_pointer: RegWidth,
    n_free_regs: usize,
}

impl<'a> Bytecode<'a> {
    fn with_capacity_in(cap: usize, arena: &'a Bump) -> Self {
        Self {
            code: Vec::with_capacity_in(cap, arena),
            metadata: StdVec::with_capacity(cap),
            begin_label: Label(0),
            begin_file_label: Label(0),
            end_file_label: Label(0),
            end_label: Label(0),
            rules_label: Label(0),
        }
    }

    fn len(&self) -> IxWidth {
        self.code.len() as IxWidth
    }

    fn nth(&mut self, label: Label) -> &mut Instruction {
        &mut self.code[label.0 as usize]
    }

    pub fn begin_code(&self) -> CodeRange {
        CodeRange(self.begin_label.0..self.begin_file_label.0)
    }

    pub fn begin_file_code(&self) -> CodeRange {
        CodeRange(self.begin_file_label.0..self.end_file_label.0)
    }

    pub fn end_file_code(&self) -> CodeRange {
        CodeRange(self.end_file_label.0..self.end_label.0)
    }

    pub fn end_code(&self) -> CodeRange {
        CodeRange(self.end_label.0..self.rules_label.0)
    }

    pub fn rules_code(&self) -> CodeRange {
        CodeRange(self.rules_label.0..self.len())
    }
}

impl RegsState {
    fn new(code: &CodeGen) -> Self {
        Self {
            reg_pointer: code.reg_pointer,
            n_free_regs: code.free_regs.len(),
        }
    }

    fn scope<T>(self, code: &mut CodeGen, f: impl FnOnce(&mut CodeGen) -> T) -> (Self, T) {
        let ret = f(code);
        let old = code.reg_pointer;
        code.reg_pointer = self.reg_pointer;
        code.free_regs.truncate(self.n_free_regs);
        (Self { reg_pointer: old, ..self }, ret)
    }

    fn scope_hwm<T>(self, code: &mut CodeGen, f: impl FnOnce(&mut CodeGen) -> T) {
        f(code);
        code.reg_pointer = code.reg_pointer.max(self.reg_pointer);
        code.free_regs.truncate(self.n_free_regs);
    }
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

    fn free(self, code: &mut CodeGen) {
        if let Self::Reg(reg) = self {
            code.free_reg(reg);
        }
    }
}

impl TypedArg {
    fn new_us(code: &mut CodeGen<'_>, ident: &Identifier<'_>) -> Self {
        let sym = code.symbols.register_user_var(ident, code.arena);
        Self(Arg { sym }, ArgTy::UsVal)
    }

    fn new_is(var: &Variable<'_>) -> Self {
        Self(Arg { sym: var_index(var) }, ArgTy::IsVal)
    }

    fn new_imm(imm: i32) -> Self {
        Self(Arg { imm }, ArgTy::Imm)
    }

    fn new_immf(code: &mut CodeGen<'_>, n: f64) -> Self {
        let sym = code.register_const(Value::Float(n));
        Self(Arg { sym }, ArgTy::ImmF)
    }

    fn new_cnt<'a>(code: &mut CodeGen<'a>, val: Value<'a>) -> Self {
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

// HACK: Either::either_into from a ref.
impl From<&Self> for Reg {
    fn from(value: &Self) -> Self {
        *value
    }
}

#[cfg(test)]
mod tests {
    use bumpalo::Bump;
    use parser::Parser;

    use super::*;

    fn with_lower(source: &str, f: impl FnOnce(&CodeGen<'_>)) {
        let arena = Bump::new();
        let mut parser = Parser::new(&arena, false);
        let ast = parser.parse(None, source.as_bytes()).expect("parse");
        let mut cg = CodeGen::new(&arena);
        cg.lower_ast(ast);
        f(&cg);
    }

    #[test]
    fn switch_lowers_case_comparisons() {
        with_lower(
            "BEGIN { switch (x) { case 1: print; case \"a\": print 2; default: print 3 } }",
            |cg| {
                let bc = format!("{}", cg.bc);
                assert!(
                    bc.contains(" <- eq "),
                    "expected Eq for literal cases:\n{bc}"
                );
                assert!(bc.contains("brif"), "expected case branches:\n{bc}");
                assert!(bc.contains("jmp"), "expected jumps to end of switch:\n{bc}");
            },
        );
    }

    #[test]
    fn switch_lowers_regex_case_with_matches() {
        with_lower("BEGIN { switch (x) { case /pat/: print } }", |cg| {
            let bc = format!("{}", cg.bc);
            assert!(
                bc.contains(" <- mtch "),
                "expected Matches for regex case:\n{bc}"
            );
            assert!(
                !bc.contains(" <- eq "),
                "regex case should not use Eq:\n{bc}"
            );
        });
    }

    fn brif_count(cg: &CodeGen<'_>) -> usize {
        cg.bc
            .code
            .iter()
            .filter(|i| matches!(i, Instruction::Branch { .. }))
            .count()
    }

    #[test]
    fn and_or_use_branches_not_dedicated_ops() {
        with_lower("BEGIN { print (0 && 1); print (1 || 0) }", |cg| {
            let bc = format!("{}", cg.bc);
            assert!(
                !bc.contains(" <- and "),
                "unexpected And instruction:\n{bc}"
            );
            assert!(!bc.contains(" <- or "), "unexpected Or instruction:\n{bc}");
            assert!(
                bc.contains("brif"),
                "expected short-circuit branches:\n{bc}"
            );
        });
    }

    #[test]
    fn switch_default_in_middle_uses_no_match_jump_only() {
        with_lower(
            "BEGIN { switch (x) { case 1: print 1; default: print 2; case 3: print 3 } }",
            |cg| {
                let jmp_count = cg
                    .bc
                    .code
                    .iter()
                    .filter(|i| matches!(i, Instruction::Jump { .. }))
                    .count();
                assert_eq!(
                    jmp_count, 1,
                    "expected a single no-match jump, not per-case exits:\n{}",
                    cg.bc
                );
            },
        );
    }

    #[test]
    fn switch_typed_regex_case_uses_matches() {
        with_lower("BEGIN { switch (x) { case @/pat/: print } }", |cg| {
            let bc = format!("{}", cg.bc);
            assert!(
                bc.contains(" <- mtch "),
                "expected Matches for typed regex case:\n{bc}"
            );
        });
    }

    #[test]
    fn chained_and_lowers_one_branch_per_operator() {
        let mut single = 0;
        with_lower("BEGIN { print (0 && 1) }", |cg| single = brif_count(cg));
        with_lower("BEGIN { print (0 && 1 && 2) }", |cg| {
            assert_eq!(brif_count(cg), single + 1, "each && should add one branch");
        });
    }

    #[test]
    fn switch_no_exit_jumps_between_case_bodies() {
        with_lower(
            "BEGIN { switch (1) { case 1: print; default: print 2 } }",
            |cg| {
                let jmp_count = cg
                    .bc
                    .code
                    .iter()
                    .filter(|i| matches!(i, Instruction::Jump { .. }))
                    .count();
                assert_eq!(
                    jmp_count, 1,
                    "gawk switch fallthrough should not emit per-case exit jumps:\n{}",
                    cg.bc
                );
            },
        );
    }

    #[test]
    fn switch_scrutinee_expression_is_evaluated_once() {
        with_lower("BEGIN { switch (1 + 1) { case 2: print } }", |cg| {
            let add_count = cg
                .bc
                .code
                .iter()
                .filter(|i| matches!(i, Instruction::Add { .. }))
                .count();
            assert_eq!(
                add_count, 1,
                "scrutinee should be evaluated once:\n{}",
                cg.bc
            );
        });
    }

    #[test]
    fn chained_or_lowers_one_branch_per_operator() {
        let mut single = 0;
        with_lower("BEGIN { print (1 || 0) }", |cg| single = brif_count(cg));
        with_lower("BEGIN { print (1 || 0 || 2) }", |cg| {
            assert_eq!(brif_count(cg), single + 1, "each || should add one branch");
        });
    }

    fn jump_targets(cg: &CodeGen<'_>) -> StdVec<IxWidth> {
        cg.bc
            .code
            .iter()
            .filter_map(|i| match i {
                Instruction::Jump { to } => Some(to.0),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn break_in_while_jumps_past_loop() {
        with_lower("BEGIN { while (1) { break } }", |cg| {
            let targets = jump_targets(cg);
            assert!(!targets.is_empty(), "expected break jump:\n{}", cg.bc);
            let max = *targets.iter().max().expect("jumps");
            // Break must land at/after the last emitted instruction index (past loop-back).
            assert!(
                targets.iter().any(|&t| t == max),
                "break should jump to the loop exit:\n{}",
                cg.bc
            );
            assert!(
                max >= cg.bc.len().saturating_sub(1),
                "break target should be at the end of the loop:\n{}",
                cg.bc
            );
        });
    }

    #[test]
    fn break_in_switch_jumps_to_end() {
        with_lower(
            "BEGIN { switch (1) { case 1: break; case 2: print 2 } }",
            |cg| {
                let targets = jump_targets(cg);
                assert!(
                    targets.iter().any(|&t| t == cg.bc.len()),
                    "break should jump to end of switch:\n{}",
                    cg.bc
                );
            },
        );
    }

    #[test]
    fn nested_break_targets_innermost() {
        with_lower("BEGIN { while (1) { for (;;) { break } } }", |cg| {
            let targets = jump_targets(cg);
            // Inner for break + outer while loop-back (and possibly more) produce jumps;
            // the earliest jump target should be the inner for exit (before outer loop-back).
            assert!(
                targets.len() >= 2,
                "expected inner break and outer loop jumps:\n{}",
                cg.bc
            );
            let min = *targets.iter().min().expect("jumps");
            let max = *targets.iter().max().expect("jumps");
            assert!(
                min < max,
                "innermost break exit should precede outer loop targets:\n{}",
                cg.bc
            );
        });
    }

    #[test]
    fn continue_in_while_jumps_to_condition() {
        with_lower("BEGIN { while (1) { continue } }", |cg| {
            let targets = jump_targets(cg);
            assert!(
                targets.len() >= 2,
                "expected continue + loop-back jumps:\n{}",
                cg.bc
            );
            // Both continue and the end-of-body jump re-enter at the condition.
            assert!(
                targets.windows(2).any(|w| w[0] == w[1]),
                "continue should share the while condition label with the loop-back:\n{}",
                cg.bc
            );
        });
    }

    #[test]
    fn continue_in_for_jumps_to_update() {
        with_lower("BEGIN { for (i = 0; i < 3; i++) { continue } }", |cg| {
            let targets = jump_targets(cg);
            let min = *targets.iter().min().expect("jumps");
            let max = *targets.iter().max().expect("jumps");
            assert!(
                min < max,
                "update (continue) label should precede condition:\n{}",
                cg.bc
            );
            // Initial jump skips update → condition; continue + loop-back → update.
            assert_eq!(
                targets.iter().filter(|&&t| t == min).count(),
                2,
                "continue and loop-back should jump to update:\n{}",
                cg.bc
            );
            assert_eq!(
                targets.iter().filter(|&&t| t == max).count(),
                1,
                "one jump should skip update into the condition:\n{}",
                cg.bc
            );
        });
    }

    #[test]
    fn continue_in_do_while_jumps_to_condition() {
        with_lower("BEGIN { do { continue } while (0) }", |cg| {
            let targets = jump_targets(cg);
            let min = *targets.iter().min().expect("jumps");
            let max = *targets.iter().max().expect("jumps");
            assert!(
                min < max,
                "condition (continue) label should precede body:\n{}",
                cg.bc
            );
            assert_eq!(
                targets.iter().filter(|&&t| t == min).count(),
                2,
                "continue and loop-back should jump to the condition:\n{}",
                cg.bc
            );
            assert_eq!(
                targets.iter().filter(|&&t| t == max).count(),
                1,
                "one jump should enter the body on first iteration:\n{}",
                cg.bc
            );
        });
    }

    #[test]
    fn nested_continue_targets_innermost_loop() {
        // Outer while + inner for: continue must use the for update label, not the while condition.
        with_lower(
            "BEGIN { while (1) { for (i = 0; i < 3; i++) { continue } } }",
            |cg| {
                let targets = jump_targets(cg);
                // Among jumps, the innermost continue/loop-back share the smallest label that
                // is hit twice (the for update). The while loop-back is a distinct third target.
                let mut counts = std::collections::BTreeMap::<IxWidth, usize>::new();
                for t in &targets {
                    *counts.entry(*t).or_default() += 1;
                }
                let twice = counts.iter().filter(|(_, c)| **c == 2).count();
                assert!(
                    twice >= 1,
                    "inner for continue+loop-back should share a label:\n{}",
                    cg.bc
                );
                assert!(
                    counts.len() >= 3,
                    "nested loops should produce distinct continue/loop targets:\n{}",
                    cg.bc
                );
            },
        );
    }

    #[test]
    fn array_index_assignment_lowers_storea() {
        with_lower("BEGIN { a[1] = 2 }", |cg| {
            let bc = format!("{}", cg.bc);
            assert!(bc.contains("astore"), "expected StoreA:\n{bc}");
            assert!(bc.contains("ua("), "expected user-array place:\n{bc}");
        });
    }

    #[test]
    fn array_index_read_lowers_loada() {
        with_lower("BEGIN { print a[1] }", |cg| {
            let bc = format!("{}", cg.bc);
            assert!(bc.contains("aload"), "expected LoadA:\n{bc}");
            assert!(bc.contains("ua("), "expected user-array place:\n{bc}");
        });
    }

    #[test]
    fn array_index_increment_lowers_load_and_store() {
        with_lower("BEGIN { ++a[1] }", |cg| {
            let bc = format!("{}", cg.bc);
            assert!(bc.contains("aload"), "expected LoadA:\n{bc}");
            assert!(bc.contains("astore"), "expected StoreA:\n{bc}");
        });
    }

    #[test]
    fn array_multi_index_assignment_lowers_storea() {
        with_lower("BEGIN { a[1, 2] = 3 }", |cg| {
            let bc = format!("{}", cg.bc);
            assert!(bc.contains("astore"), "expected StoreA:\n{bc}");
        });
    }
}
