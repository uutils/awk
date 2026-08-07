// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

mod utils;

use std::{borrow::Cow, vec::Vec as StdVec};

use bumpalo::{Bump, collections::Vec};
use either::Either;
use parser::{
    ArrayOperator, Ast, Atom, BinaryOperator, BinaryPlaceOperator, Body, Command, Expr, ExprNode,
    Function as AstFunction, FunctionTable, Identifier, MetaId, Place, Rule, RulePattern,
    SimpleStatement, Statement, UnaryPlaceOperator, Variable,
};

use crate::{
    CodeRange,
    ir::{
        ArgTy, Instruction, IxWidth, Label, NonLocal, Reg, RegWidth,
        lower::utils::{LinearReg, Operand, RegsState, TypedArg, var_index},
    },
    vm::{Consts, Function, SymbolTable, types::Value},
};

#[derive(Debug)]
pub struct Bytecode<'a> {
    pub code: Vec<'a, Instruction>,
    pub functions: SymbolTable<'a>,
    pub metadata: StdVec<MetaId>,
    pub(crate) funs_label: Label,
    pub(crate) begin_label: Label,
    pub(crate) begin_file_label: Label,
    pub(crate) end_file_label: Label,
    pub(crate) end_label: Label,
    pub(crate) rules_label: Label,
}

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
    local_args: StdVec<NonLocal>,
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
            consts: Consts::new_in(arena),
            symbols: SymbolTable::new_in(arena),
            free_regs: Vec::new_in(arena),
            reg_pointer: 0,
            current_metadata: MetaId::default(),
            break_exits: None,
            continue_label: None,
            local_args: StdVec::new(),
        }
    }

    pub fn lower_ast(&mut self, ast: &Ast) {
        self.bc.funs_label = Label(self.bc.len());
        self.lower_functions(&ast.functions);

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

            let (arg, ty) = TypedArg::new_imm(0).into();
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

    fn lower_functions(&mut self, functions: &FunctionTable) {
        for (name, AstFunction { args, body }) in functions {
            let start = self.bc.len();
            let (arity, hwm_regs) = self.lower_fun_body(args, body);

            if !matches!(
                self.bc.code.last(),
                Some(Instruction::Return { .. } | Instruction::ReturnUnassigned)
            ) {
                self.emit(Instruction::ReturnUnassigned);
            }

            let code = CodeRange(start..self.bc.len());

            self.symbols
                .register_user_fun(name, Function { arity, hwm_regs, code }, self.arena);
        }
    }

    fn lower_fun_body(&mut self, args: &[Identifier], body: &Body) -> (RegWidth, RegWidth) {
        debug_assert_eq!(self.reg_pointer, 0);
        debug_assert!(self.local_args.is_empty());

        // intern the arguments
        self.local_args.extend(
            args.iter()
                .map(|arg| self.symbols.register_user_var(arg, self.arena)),
        );

        let arity = RegWidth::try_from(args.len()).expect("Too many args!");
        let (state, ()) = RegsState::new(self).scope(self, |this| {
            this.reg_pointer += RegWidth::try_from(arity).expect("Too many args!");
            this.lower_body(body);
        });

        self.local_args.clear();
        (arity, state.reg_pointer)
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

                    let (arg, ty) = TypedArg::new_reg(*dest).into();
                    this.emit(Instruction::Exit { arg, ty });

                    this.free_reg(dest);
                });
            }
            Statement::Exit(None, metadata) => {
                self.with_metadata(*metadata, |this| {
                    let (arg, ty) = TypedArg::new_imm(0).into();
                    this.emit(Instruction::Exit { arg, ty });
                });
            }
            Statement::Return(Some(expr), metadata) => {
                self.with_metadata(*metadata, |this| {
                    let dest = this.alloc_reg();
                    this.lower_expr_into(expr, *dest);

                    let (arg, ty) = TypedArg::new_reg(*dest).into();
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
        let mut pending_branches = StdVec::new();
        for (i, (atom, _)) in branches.iter().enumerate() {
            pending_branches.push(self.emit_switch_case_match(*scr, *cmp, atom, i));
        }
        let no_match_jump = self.emit(Instruction::Jump { to: Label(0) });

        let mut case_labels = StdVec::with_capacity(branches.len());
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
        let (lhs, tyl) = TypedArg::new_reg(scr).into();

        match case {
            Atom::Regex(r) | Atom::TypedRegex(r) => {
                let buf = &*self.arena.alloc_slice_copy(r.as_ref());
                let (rhs, tyr) = TypedArg::new_cnt(self, Value::Regex(buf.into())).into();
                self.emit(Instruction::Matches { dest: cmp, lhs, rhs, tyl, tyr });
            }
            atom => {
                let case_val = self.lower_atom(atom);
                let (rhs, tyr) = case_val.to_arg().into();
                self.emit(Instruction::Eq { dest: cmp, lhs, rhs, tyl, tyr });
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
            arg if let Some(reg) = arg.as_reg() => {
                if reg == *dest {
                    Operand::Reg(dest)
                } else {
                    // If the atom is a function local, we get its register instead.
                    self.free_reg(dest);
                    Operand::Imm(TypedArg::new_reg(reg))
                }
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
                let (rhs, tyr) = TypedArg::new_cnt(self, Value::Regex(buf.into())).into();
                let (lhs, tyl) = TypedArg::new_imm(0).into();
                self.emit(Instruction::Matches { dest, rhs, lhs, tyr, tyl });
                TypedArg::new_reg(dest)
            }
            _ => todo!(),
        }
    }

    fn lower_atom_into(&mut self, atom: &Atom, dest: Reg) {
        let t_arg = self.lower_atom_arg(atom, dest);
        let (arg, ty) = t_arg.into();

        if t_arg.as_reg().is_none_or(|reg| reg != dest) {
            self.emit(Instruction::Copy { dest, arg, ty });
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
                            this.store_place(place, dest, TypedArg::new_reg(dest));

                            this.free_reg(lhs_reg);
                            val.free(this);
                        }
                        // Use optimized path for variables and records. Cannot
                        // be used for arrays because the stores aren't trivial.
                        ExprNode::UnaryPlaceOperation(op, place)
                            if matches!(place, Place::Record(_) | Place::Variable(_)) =>
                        {
                            let (arg, ty) = this.load_place(dest, place).into();
                            match op {
                                UnaryPlaceOperator::IncrementL => {
                                    this.emit(Instruction::IncrementPre { dest, arg, ty });
                                }
                                UnaryPlaceOperator::IncrementR => {
                                    this.emit(Instruction::IncrementPost { dest, arg, ty });
                                }
                                UnaryPlaceOperator::DecrementL => {
                                    this.emit(Instruction::DecrementPre { dest, arg, ty });
                                }
                                UnaryPlaceOperator::DecrementR => {
                                    this.emit(Instruction::DecrementPost { dest, arg, ty });
                                }
                            }
                        }
                        // Unoptimized path for values in arrays.
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
                                    this.store_place(place, dest, TypedArg::new_reg(dest));
                                }
                                UnaryPlaceOperator::DecrementL => {
                                    this.emit(Instruction::from_binary(
                                        BinaryOperator::Subtract,
                                        dest,
                                        lhs,
                                        one,
                                    ));
                                    this.store_place(place, dest, TypedArg::new_reg(dest));
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
                                    this.store_place(place, *tmp, TypedArg::new_reg(*tmp));
                                    this.free_reg(tmp);
                                }
                            }
                        }
                        ExprNode::Parenthesized(expr) => this.lower_expr_into(expr, dest),
                        ExprNode::ArrayOperation(ArrayOperator::Index, var, index) => {
                            this.load_index(dest, var, index);
                        }
                        ExprNode::FunctionCall(name, args) => {
                            let name = this.symbols.get_user_fun(name, self.arena);
                            let (start, end, ()) = this.gen_call_convention(args, |_| ());
                            this.emit(Instruction::UserCall { dest, start, end, name });
                        }
                        ExprNode::BuiltinCall(fun, args) => {
                            // Bypass regular variable lookups on type-info funs.
                            let (start, end, _) = this.gen_call_convention(args, |_| ());
                            this.emit(Instruction::IntrinsicCall { dest, start, end, fun: *fun });
                        }
                        ExprNode::IndirectCall(place, args) => {
                            let (start, end, ()) = this.gen_call_convention(args, |_| ());
                            let (name, ty) = this.load_place(dest, &Place::Variable(*place)).into();
                            this.emit(Instruction::IndirectCall { dest, start, end, name, ty });
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
        TypedArg::new_reg(dest)
    }

    fn store_place(&mut self, place: &Place<'_>, dest: Reg, src: TypedArg) {
        let (arg, ty) = src.into();
        match place {
            Place::Record(expr) => {
                let rec = self.lower_expr(expr);
                let (src, tys) = rec.to_arg().into();
                self.emit(Instruction::StoreR { dest, src, tys, arg, ty });
                rec.free(self);
            }
            Place::Variable(Variable::User(ident)) => {
                let t_arg = TypedArg::new_us(self, ident);
                let (var, ty_place) = t_arg.into();

                if t_arg.as_reg().is_some_and(|reg| reg == dest) {
                    return; // Value already on destination.
                }

                let store = match ty_place {
                    ArgTy::Reg => Instruction::Copy { dest, arg, ty },
                    ArgTy::UsVal => {
                        Instruction::StoreS { dest, ty_place, var: unsafe { var.sym }, arg, ty }
                    }
                    _ => unreachable!(),
                };
                self.emit(store);
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
            let (arg, ty) = TypedArg::new_imm(0).into();
            this.emit(Instruction::Copy { dest, arg, ty });
        });
    }

    fn lower_or_into(&mut self, lhs: &Expr<'_>, rhs: &Expr<'_>, dest: Reg) {
        let (if_label, _) = self.emit_branch(lhs, |this| {
            let (arg, ty) = TypedArg::new_imm(1).into();
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
        let (arg, ty) = TypedArg::new_reg(src).into();
        self.emit(Instruction::Negation { dest, arg, ty });

        let (arg, ty) = TypedArg::new_reg(dest).into();
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
        self.free_regs.pop().map_or_else(
            || {
                let current = self.reg_pointer;
                self.reg_pointer = self.reg_pointer.checked_add(1).expect("register overflow");
                Reg(current).into()
            },
            LinearReg::from,
        )
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
                let dest = Reg(call_start.checked_add(offset).expect("register overflow"));
                // Bypass Copy instruction for variables here, so we do not get
                // scalar context shenanigans in the VM.
                if let &Expr::Leaf(Atom::Variable(var), _) = arg {
                    let (arg, ty) = this.load_place(dest, &Place::Variable(var)).into();
                    this.emit(Instruction::PureCopy { dest, arg, ty });
                } else {
                    this.lower_expr_into(arg, dest);
                }
            }
            (Reg(call_start), Reg(call_end), extra(this))
        });
        self.reg_pointer = self.reg_pointer.max(state.reg_pointer);
        ret
    }

    fn spill_to_reg(&mut self, t_arg: TypedArg) -> Either<Reg, LinearReg> {
        if let Some(reg) = t_arg.as_reg() {
            Either::Left(reg)
        } else {
            let dest = self.alloc_reg();
            let (arg, ty) = t_arg.into();
            self.emit(Instruction::Copy { dest: *dest, arg, ty });
            Either::Right(dest)
        }
    }

    fn free_reg(&mut self, reg: LinearReg) {
        self.free_regs.push(reg.into_inner());
    }

    fn register_const(&mut self, value: Value<'a>) -> NonLocal {
        let nl = NonLocal(self.consts.0.len() as IxWidth);
        self.consts.0.push(value);
        nl
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

    fn get_local_arg(&self, sym: NonLocal) -> Option<Reg> {
        self.local_args
            .iter()
            .enumerate()
            .find_map(|(i, &nl)| (nl == sym).then_some(Reg(i as RegWidth)))
    }
}

impl<'a> Bytecode<'a> {
    fn with_capacity_in(cap: usize, arena: &'a Bump) -> Self {
        Self {
            code: Vec::with_capacity_in(cap, arena),
            functions: SymbolTable::new_in(arena),
            metadata: StdVec::with_capacity(cap),
            begin_label: Label(0),
            begin_file_label: Label(0),
            end_file_label: Label(0),
            end_label: Label(0),
            rules_label: Label(0),
            funs_label: Label(0),
        }
    }

    pub(crate) fn len(&self) -> IxWidth {
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

    pub fn funs_code(&self) -> CodeRange {
        CodeRange(self.funs_label.0..self.begin_label.0)
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
