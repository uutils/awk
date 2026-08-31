// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

mod utils;

use std::{borrow::Cow, mem::replace, vec::Vec as StdVec};

use bumpalo::{Bump, collections::Vec};
use parser::{
    ArrayOperator, Ast, Atom, BinaryOperator, BinaryPlaceOperator, Body, Command, Expr, ExprNode,
    Function as AstFunction, FunctionTable, Identifier, MetaId, Place, Rule, RulePattern,
    SimpleStatement, Statement, UnaryOperator, UnaryPlaceOperator, Variable,
};
use smallvec::SmallVec;

use crate::{
    CodeRange,
    ir::{
        Arg, Instruction, IxWidth, Label, NonLocal, PlaceTy, Reg, RegWidth,
        lower::utils::{
            CallConv, LinearRegRange, Operand, RegAlloc, ResolvedPlace, RtType, TypedArg,
            TypedPlace, var_index,
        },
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
    pub(crate) regs: RegAlloc,
    current_metadata: MetaId,
    break_exits: Option<SmallVec<[Label; 4]>>,
    continue_label: Option<Label>,
    local_args: SmallVec<[NonLocal; 4]>,
}

impl<'a> CodeGen<'a> {
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
            regs: RegAlloc::new(),
            current_metadata: MetaId::default(),
            break_exits: None,
            continue_label: None,
            local_args: SmallVec::new(),
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
            self.scoped_reg(|this, reg| {
                let (arg, ty) = TypedArg::new_imm(0).into_arg();
                this.emit(Instruction::LoadF { dest: reg, arg, ty });

                this.emit(Instruction::OutputCall {
                    start: reg,
                    end: Reg(reg.0 + 1),
                    cmd: Command::Print,
                    redir: None,
                });
            });
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
        debug_assert_eq!(self.regs.reg_pointer, 0);
        debug_assert!(self.local_args.is_empty());
        // The VM dynamically allocates new regs in each stack frame, so we can
        // restore the hwm mark at the end and save on registers.
        let old_hwm = self.regs.hwm;

        // intern the arguments
        self.local_args.extend(
            args.iter()
                .map(|arg| self.symbols.register_user_var(arg, self.arena)),
        );

        let arity = RegWidth::try_from(args.len()).expect("Too many args!");
        self.regs.clone().scope(self, |this| {
            this.regs.reserve(arity);
            this.lower_body(body);
        });
        self.local_args.clear();

        (arity, replace(&mut self.regs.hwm, old_hwm))
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
                    let state = this.regs.clone();
                    let (if_label, ()) =
                        this.emit_branch(condition, |this| this.lower_body(then_body));

                    if let Some(else_body) = else_body {
                        this.bc.nth(if_label).push_end_label();
                        this.emit_jump(|this| {
                            state.scope(this, |this| this.lower_body(else_body));
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
                        let (branch, continue_label) = this.emit_jump(|this| {
                            let continue_label = this.following_instr(0);
                            this.scoped_reg(|this, cond_reg| {
                                this.lower_expr_into(condition, cond_reg);
                                let then_label = this.following_instr(1);
                                let branch = this.emit(Instruction::br(cond_reg, then_label));
                                (branch, continue_label)
                            })
                        });

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
                    let continue_label = this.emit_jump(|this| {
                        let continue_label = this.following_instr(0);
                        if let Some(SimpleStatement::Expression(expr, metadata)) = update {
                            this.with_metadata(*metadata, |this| {
                                this.lower_expr(expr).free(this);
                            });
                        }
                        continue_label
                    });

                    let lower_for_body = |this: &mut Self| {
                        this.with_break_scope(|this| {
                            this.with_continue_label(continue_label, |this| {
                                this.lower_body(body);
                            });
                            this.emit(Instruction::Jump { to: continue_label });
                        });
                    };

                    match condition {
                        Some(condition) => {
                            this.emit_branch(condition, lower_for_body);
                        }
                        None => lower_for_body(this),
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
                    let (range, redir) =
                        this.alloc_reg_range_patched::<false, _>(RtType::Scalar, args, |this| {
                            redirection.as_ref().map(|(r, expr)| {
                                this.scoped_reg(|this, redir_reg| {
                                    this.lower_expr_into(expr, redir_reg);
                                });
                                *r
                            })
                        });
                    let (start, end) = range.as_range();
                    this.emit(Instruction::OutputCall { start, end, cmd: *name, redir });
                    this.regs.free_many(range);
                });
            }
            Statement::Simple(SimpleStatement::Delete(var, Some(indices), metadata)) => self
                .with_metadata(*metadata, |this| {
                    let (arg, ty) = this.load_var(var).into_place();
                    this.scoped_reg_range(RtType::Scalar, indices, |this, (start, end)| {
                        this.emit(Instruction::DeleteP { arg, ty, start, end });
                    });
                }),
            Statement::Simple(SimpleStatement::Delete(var, None, metadata)) => {
                self.with_metadata(*metadata, |this| {
                    let (arg, ty) = this.load_var(var).into_place();
                    this.emit(Instruction::DeleteA { arg, ty });
                });
            }
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
                    this.scoped_reg(|this, dest| {
                        this.lower_expr_into(expr, dest);

                        let (arg, ty) = TypedArg::new_reg(dest).into_arg();
                        this.emit(Instruction::Exit { arg, ty });
                    });
                });
            }
            Statement::Exit(None, metadata) => {
                self.with_metadata(*metadata, |this| {
                    let (arg, ty) = TypedArg::new_imm(0).into_arg();
                    this.emit(Instruction::Exit { arg, ty });
                });
            }
            Statement::Return(Some(expr), metadata) => {
                self.with_metadata(*metadata, |this| {
                    this.scoped_reg(|this, dest| {
                        this.lower_expr_into(expr, dest);

                        let (arg, ty) = TypedArg::new_reg(dest).into_arg();
                        this.emit(Instruction::Return { arg, ty });
                    });
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
        let scr = self.regs.alloc();
        self.lower_expr_into(scrutinee, *scr);
        let cmp = self.regs.alloc();

        let default_pos = default.map_or(branches.len(), |(_, pos)| *pos);
        let mut pending_branches = SmallVec::<[_; 8]>::new();
        for (i, (atom, _)) in branches.iter().enumerate() {
            pending_branches.push(self.emit_switch_case_match(*scr, *cmp, atom, i));
        }
        let no_match_jump = self.emit(Instruction::Jump { to: Label(0) });

        let mut case_labels = SmallVec::<[_; 8]>::new();
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
        self.regs.free(cmp);
        self.regs.free(scr);
    }

    fn emit_switch_case_match(
        &mut self,
        scr: Reg,
        cmp: Reg,
        case: &Atom<'_>,
        case_ix: usize,
    ) -> (Label, usize) {
        let (lhs, tyl) = TypedArg::new_reg(scr).into_arg();

        match case {
            Atom::Regex(r) | Atom::TypedRegex(r) => {
                let buf = &*self.arena.alloc_slice_copy(r.as_ref());
                let (rhs, tyr) = TypedArg::new_cnt(self, Value::Regex(buf.into())).into_arg();
                self.emit(Instruction::Matches { dest: cmp, lhs, rhs, tyl, tyr });
            }
            atom => {
                let case_val = self.lower_atom(atom);
                let (rhs, tyr) = case_val.to_arg().into_arg();
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
                let dest = self.regs.alloc();
                self.lower_expr_into(expr, *dest);
                Operand::Reg(dest)
            }
        }
    }

    fn lower_atom(&mut self, atom: &Atom) -> Operand {
        let dest = self.regs.alloc();
        match self.lower_atom_arg(atom, *dest) {
            arg if let Some(reg) = arg.as_reg() => {
                if reg == *dest {
                    Operand::Reg(dest)
                } else {
                    // If the atom is a function local, we get its register instead.
                    self.regs.free(dest);
                    Operand::Imm(TypedArg::new_reg(reg))
                }
            }
            imm => {
                self.regs.free(dest);
                Operand::Imm(imm)
            }
        }
    }

    fn lower_atom_arg(&mut self, atom: &Atom, dest: Reg) -> TypedArg {
        match atom {
            Atom::Variable(Variable::User(ident)) => TypedArg::new_user(self, ident),
            Atom::Variable(var) => TypedArg::new_btin(var),
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
                let (rhs, tyr) = TypedArg::new_cnt(self, Value::Regex(buf.into())).into_arg();
                let (lhs, tyl) = TypedArg::new_imm(0).into_arg();
                self.emit(Instruction::Matches { dest, rhs, lhs, tyr, tyl });
                TypedArg::new_reg(dest)
            }
            Atom::BigInt() => todo!(),
            Atom::BigFloat() => todo!(),
        }
    }

    fn lower_atom_into(&mut self, atom: &Atom, dest: Reg) {
        let t_arg = self.lower_atom_arg(atom, dest);
        let (arg, ty) = t_arg.into_arg();

        if t_arg.as_reg().is_none_or(|reg| reg != dest) {
            self.emit(Instruction::CopyP { dest, arg, ty });
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
                                rhs.free(this);
                                lhs.free(this);
                            }
                        },
                        ExprNode::Ternary(condition, true_then, false_then) => {
                            let (if_label, state) = this.emit_branch(condition, |this| {
                                let state = this.regs.clone();
                                this.lower_expr_into(true_then, dest);
                                state
                            });
                            this.bc.nth(if_label).push_end_label();
                            this.emit_jump(|this| {
                                state.scope(this, |this| this.lower_expr_into(false_then, dest));
                            });
                        }
                        ExprNode::BinaryPlaceOperation(op, place, expr) => {
                            let val = this.lower_expr(expr);

                            let Some(bin_op) = lower_assign_ops(*op) else {
                                this.store_place(place, dest, val.to_arg());
                                val.free(this);
                                return;
                            };

                            this.with_resolved_place(place, |this, resolved| {
                                this.scoped_reg(|this, lhs_reg| {
                                    let lhs = this.load_resolved(resolved, place, lhs_reg).into();
                                    let rhs = val.to_arg();
                                    let arg = TypedArg::new_reg(dest);

                                    this.emit(Instruction::from_binary(bin_op, dest, lhs, rhs));
                                    this.store_resolved(resolved, place, dest, arg);
                                });
                            });
                            val.free(this);
                        }
                        // Use optimized path for variables and records. Cannot
                        // be used for arrays because the stores aren't trivial.
                        ExprNode::UnaryPlaceOperation(op, place)
                            if matches!(place, Place::Variable(_)) =>
                        {
                            let (arg, ty) = this.load_place(dest, place).into_place();
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
                            this.with_resolved_place(place, |this, resolved| {
                                let lhs = this.load_resolved(resolved, place, dest).into();
                                let (zero, one) = (TypedArg::new_imm(0), TypedArg::new_imm(1));
                                let add = BinaryOperator::Add;
                                let bn = unary_place_to_binop(*op);

                                match op {
                                    UnaryPlaceOperator::IncrementL
                                    | UnaryPlaceOperator::DecrementL => {
                                        let arg = TypedArg::new_reg(dest);
                                        this.emit(Instruction::from_binary(bn, dest, lhs, one));
                                        this.store_resolved(resolved, place, dest, arg);
                                    }
                                    UnaryPlaceOperator::IncrementR
                                    | UnaryPlaceOperator::DecrementR => {
                                        this.emit(Instruction::from_binary(add, dest, lhs, zero));
                                        this.scoped_reg(|this, tmp| {
                                            let arg = TypedArg::new_reg(tmp);
                                            this.emit(Instruction::from_binary(bn, tmp, lhs, one));
                                            this.store_resolved(resolved, place, tmp, arg);
                                        });
                                    }
                                }
                            });
                        }
                        ExprNode::Parenthesized(expr) => this.lower_expr_into(expr, dest),
                        ExprNode::ArrayOperation(ArrayOperator::Index, var, index) => {
                            this.load_index(dest, var, index);
                        }
                        ExprNode::ArrayOperation(ArrayOperator::In, var, index)
                            if let [expr] = index.as_slice() =>
                        {
                            let index = this.lower_expr(expr);
                            let (rhs, tyr) = index.to_arg().into_arg();
                            let (lhs, tyl) = this.load_var(var).into_place();
                            this.emit(Instruction::In { dest, lhs, rhs, tyr, tyl });
                            index.free(this);
                        }
                        ExprNode::ArrayOperation(ArrayOperator::In, var, indices) => {
                            let (arg, ty) = this.load_var(var).into_place();
                            this.scoped_reg_range(RtType::Scalar, indices, |this, (start, end)| {
                                this.emit(Instruction::InA { dest, arg, start, end, ty });
                            });
                        }
                        ExprNode::FunctionCall(name, args) => {
                            let name = this.symbols.get_user_fun(name, self.arena);
                            let range = this.gen_call_convention(RtType::Any, args);
                            let (start, end) = range.as_range();
                            this.emit(Instruction::UserCall { dest, start, end, name });
                            this.regs.free_many(range);
                        }
                        &ExprNode::BuiltinCall(fun, ref args) => {
                            // Bypass regular variable look-ups on type-info funs.
                            this.scoped_reg_range(fun, args, |this, (start, end)| {
                                this.emit(Instruction::IntrinsicCall { dest, start, end, fun });
                            });
                        }
                        ExprNode::IndirectCall(var, args) => {
                            let range = this.gen_call_convention(RtType::Any, args);
                            let (start, end) = range.as_range();
                            let (name, ty) = this.load_var(var).into_arg();
                            this.emit(Instruction::IndirectCall { dest, start, end, name, ty });
                            this.regs.free_many(range);
                        }
                        ExprNode::ChainedIndex(var, indices) => {
                            this.load_chained_index(dest, var, indices);
                        }
                        &ExprNode::Getline(_) => todo!(),
                    }
                });
            }
        }
    }

    /// Resolves a place and allows performing multiple operations on it.
    /// Mostly, useful for places whose resolving has side-effects, like
    /// `array[f(x)] += 1`, which would otherwise evaluate `f(x)` twice.
    fn with_resolved_place<T>(
        &mut self,
        place: &Place<'_>,
        f: impl FnOnce(&mut Self, ResolvedPlace) -> T,
    ) -> T {
        match place {
            Place::Record(expr) => self.scoped_reg(|this, reg| {
                this.lower_expr_into(expr, reg);
                f(this, ResolvedPlace::Record(reg))
            }),
            Place::Variable(_) => f(self, ResolvedPlace::Variable),
            Place::Index(var, indices) => {
                let (arg, ty) = self.load_var(var).into_place();
                self.scoped_reg_range(RtType::Scalar, indices, |this, range| {
                    f(this, ResolvedPlace::Index { arg, ty, range })
                })
            }
            Place::ChainedIndex(var, indices) => self.scoped_reg(|this, array_reg| {
                this.load_aoa_place(array_reg, var, indices, |this, arg, ty, range| {
                    f(this, ResolvedPlace::Index { arg, ty, range })
                })
            }),
        }
    }

    /// Loads the current value of an already-resolved place into `dest`.
    fn load_resolved(
        &mut self,
        resolved: ResolvedPlace,
        place: &Place<'_>,
        dest: Reg,
    ) -> TypedPlace {
        match resolved {
            ResolvedPlace::Record(reg) => {
                let (arg, ty) = TypedArg::new_reg(reg).into_arg();
                self.emit(Instruction::LoadF { dest, arg, ty });
                TypedPlace::new_reg(dest)
            }
            ResolvedPlace::Variable => self.load_place(dest, place),
            ResolvedPlace::Index { arg, ty, range: (start, end) } => {
                self.emit(Instruction::IndexS { dest, arg, ty, start, end });
                TypedPlace::new_reg(dest)
            }
        }
    }

    /// Stores `src` into an already-resolved place.
    fn store_resolved(
        &mut self,
        resolved: ResolvedPlace,
        place: &Place<'_>,
        dest: Reg,
        src: TypedArg,
    ) {
        match resolved {
            ResolvedPlace::Record(reg) => {
                let (arg, ty) = src.into_arg();
                let (src, tys) = TypedArg::new_reg(reg).into_arg();
                self.emit(Instruction::StoreF { dest, src, tys, arg, ty });
            }
            ResolvedPlace::Variable => self.store_place(place, dest, src),
            ResolvedPlace::Index { arg: lhs, ty: tyl, range: (start, end) } => {
                let (rhs, tyr) = src.into_arg();
                self.emit(Instruction::Insert { dest, lhs, rhs, start, end, tyl, tyr });
            }
        }
    }

    fn load_place(&mut self, dest: Reg, place: &Place<'_>) -> TypedPlace {
        match place {
            Place::Record(expr) => {
                self.lower_expr_into(expr, dest);
                let (arg, ty) = TypedArg::new_reg(dest).into_arg();
                self.emit(Instruction::LoadF { dest, arg, ty });
                TypedPlace::new_reg(dest)
            }
            Place::Variable(var) => self.load_var(var),
            Place::Index(var, index) => self.load_index(dest, var, index),
            Place::ChainedIndex(var, indices) => self.load_chained_index(dest, var, indices),
        }
    }

    fn load_var(&mut self, var: &Variable<'_>) -> TypedPlace {
        match var {
            Variable::User(ident) => TypedPlace::new_user(self, ident),
            var => TypedPlace::new_btin(var),
        }
    }

    fn load_index(&mut self, dest: Reg, var: &Variable<'_>, indices: &[Expr<'_>]) -> TypedPlace {
        self.scoped_reg_range(RtType::Scalar, indices, |this, (start, end)| {
            let (arg, ty) = this.load_var(var).into_place();
            this.emit(Instruction::IndexS { dest, arg, ty, start, end });

            // Element value was written to `dest`; subsequent ops must use the register.
            TypedPlace::new_reg(dest)
        })
    }

    fn store_place(&mut self, place: &Place<'_>, dest: Reg, src: TypedArg) {
        let (arg, ty) = src.into_arg();
        match place {
            Place::Record(expr) => {
                let rec = self.lower_expr(expr);
                let (src, tys) = rec.to_arg().into_arg();
                self.emit(Instruction::StoreF { dest, src, tys, arg, ty });
                rec.free(self);
            }
            Place::Variable(Variable::User(ident)) => {
                let t_arg = TypedPlace::new_user(self, ident);
                let (var, ty_place) = t_arg.into_place();

                if t_arg.as_reg().is_some_and(|reg| reg == dest) {
                    return; // Value already on destination.
                }

                match ty_place {
                    PlaceTy::Reg => {
                        let var_reg = unsafe { var.reg };
                        self.emit(Instruction::CopyP { dest: var_reg, arg, ty });
                        if var_reg != dest {
                            let (arg, ty) = TypedArg::new_reg(var_reg).into_arg();
                            self.emit(Instruction::CopyP { dest, arg, ty });
                        }
                    }
                    PlaceTy::UserVal => {
                        let var = unsafe { var.sym };
                        self.emit(Instruction::StoreS { dest, ty_place, var, arg, ty });
                    }
                    PlaceTy::BtInVal => todo!(),
                }
            }
            Place::Variable(var) => {
                let var = var_index(var);
                self.emit(Instruction::StoreS { dest, ty_place: PlaceTy::BtInVal, var, arg, ty });
            }
            Place::Index(var, indices) => {
                let (lhs, tyl) = self.load_var(var).into_place();
                let (rhs, tyr) = src.into_arg();
                self.scoped_reg_range(RtType::Scalar, indices, |this, (start, end)| {
                    this.emit(Instruction::Insert { dest, lhs, rhs, start, end, tyl, tyr });
                });
            }
            Place::ChainedIndex(var, indices) => {
                self.scoped_reg(|this, array| {
                    this.load_aoa_place(array, var, indices, |this, lhs, tyl, (start, end)| {
                        let (rhs, tyr) = (arg, ty);
                        this.emit(Instruction::Insert { dest, lhs, rhs, start, end, tyl, tyr });
                    });
                });
            }
        }
    }

    fn load_chained_index(
        &mut self,
        dest: Reg,
        var: &Variable<'_>,
        indices: &[Vec<'_, Expr<'_>>],
    ) -> TypedPlace {
        self.load_aoa_place(dest, var, indices, |this, arg, ty, (start, end)| {
            this.emit(Instruction::IndexS { dest, arg, start, end, ty });
        });
        TypedPlace::new_reg(dest)
    }

    fn load_aoa_place<T>(
        &mut self,
        dest: Reg,
        var: &Variable<'_>,
        indices: &[Vec<'_, Expr<'_>>],
        f: impl FnOnce(&mut Self, Arg, PlaceTy, (Reg, Reg)) -> T,
    ) -> T {
        let (mut arg, mut ty) = self.load_var(var).into_place();
        let last = indices.len() - 1;

        for index in &indices[..last] {
            self.scoped_reg_range(RtType::Scalar, index, |this, (start, end)| {
                this.emit(Instruction::IndexA { dest, arg, start, end, ty });
                (arg, ty) = TypedPlace::new_reg(dest).into_place();
            });
        }
        self.scoped_reg_range(RtType::Scalar, &indices[last], |this, range| {
            f(this, arg, ty, range)
        })
    }

    fn lower_and_into(&mut self, lhs: &Expr<'_>, rhs: &Expr<'_>, dest: Reg) {
        let (if_label, ()) = self.emit_branch(lhs, |this| {
            this.scoped_reg(|this, rhs_reg| {
                this.lower_expr_into(rhs, rhs_reg);
                this.truthify(dest, rhs_reg);
            });
        });
        self.bc.nth(if_label).push_end_label();
        self.emit_jump(|this| {
            let (arg, ty) = TypedArg::new_imm(0).into_arg();
            this.emit(Instruction::CopyP { dest, arg, ty });
        });
    }

    fn lower_or_into(&mut self, lhs: &Expr<'_>, rhs: &Expr<'_>, dest: Reg) {
        let (if_label, ()) = self.emit_branch(lhs, |this| {
            let (arg, ty) = TypedArg::new_imm(1).into_arg();
            this.emit(Instruction::CopyP { dest, arg, ty });
        });
        self.bc.nth(if_label).push_end_label();
        self.emit_jump(|this| {
            this.scoped_reg(|this, rhs_reg| {
                this.lower_expr_into(rhs, rhs_reg);
                this.truthify(dest, rhs_reg);
            });
        });
    }

    /// Coerce `src` to an integer truth value (0 or 1), as gawk does via `mkbool()`.
    fn truthify(&mut self, dest: Reg, src: Reg) {
        let (arg, ty) = TypedArg::new_reg(src).into_arg();
        self.emit(Instruction::Negation { dest, arg, ty });

        let (arg, ty) = TypedArg::new_reg(dest).into_arg();
        self.emit(Instruction::Negation { dest, arg, ty });
    }

    fn emit_branch<T>(
        &mut self,
        condition_expr: &Expr<'_>,
        cb: impl FnOnce(&mut Self) -> T,
    ) -> (Label, T) {
        let if_label = self.scoped_reg(|this, condition| {
            this.lower_expr_into(condition_expr, condition);
            let then_label = this.following_instr(1);
            this.emit(Instruction::br(condition, then_label))
        });

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

    fn scoped_reg_range<T>(
        &mut self,
        typeck: impl CallConv,
        args: &[Expr<'_>],
        f: impl FnOnce(&mut Self, (Reg, Reg)) -> T,
    ) -> T {
        let r = self
            .alloc_reg_range_patched::<false, _>(typeck, args, |_| {})
            .0;
        let ret = f(self, r.as_range());
        self.regs.free_many(r);
        ret
    }

    fn gen_call_convention(&mut self, typeck: impl CallConv, args: &[Expr<'_>]) -> LinearRegRange {
        self.alloc_reg_range_patched::<true, _>(typeck, args, |_| {})
            .0
    }

    fn alloc_reg_range_patched<const TOP_OF_STACK: bool, T>(
        &mut self,
        typeck: impl CallConv,
        args: &[Expr<'_>],
        extra: impl FnOnce(&mut CodeGen) -> T,
    ) -> (LinearRegRange, T) {
        // TODO: Nicer error reporting.
        let args_len = RegWidth::try_from(args.len()).expect("too many call args");

        let range = if TOP_OF_STACK {
            self.regs.alloc_many_top(args_len)
        } else {
            self.regs.alloc_many(args_len)
        };
        let (call_start, _) = range.as_range();

        let ret = self.regs.clone().scope(self, |this| {
            for ((i, arg), rt_ty) in args.iter().enumerate().zip(typeck.convention(args_len)) {
                let dest = Reg(call_start.0 + i as RegWidth);
                // Bypass Copy instruction for variables here, so we do not get
                // scalar context shenanigans in the VM.
                if let Expr::Leaf(Atom::Variable(var), _) = arg {
                    let instr = match rt_ty {
                        RtType::Scalar => {
                            let (arg, ty) = this.load_var(var).into_arg();
                            Instruction::CopyS { dest, arg, ty }
                        }
                        RtType::Array => {
                            let (arg, ty) = this.load_var(var).into_place();
                            Instruction::CopyA { dest, arg, ty }
                        }
                        RtType::Any => {
                            let (arg, ty) = this.load_var(var).into_arg();
                            Instruction::CopyP { dest, arg, ty }
                        }
                    };
                    this.emit(instr);
                } else {
                    this.lower_expr_into(arg, dest);
                }
            }
            extra(this)
        });

        (range, ret)
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
        replace(&mut self.bc, Bytecode::with_capacity_in(0, self.arena))
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
        let prev = self.break_exits.replace(SmallVec::new());
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

    /// Allocates a register and frees it at the end of the scope.
    pub fn scoped_reg<T>(&mut self, f: impl FnOnce(&mut Self, Reg) -> T) -> T {
        let reg = self.regs.alloc();
        let ret = f(self, *reg);
        self.regs.free(reg);
        ret
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

    pub const fn begin_code(&self) -> CodeRange {
        CodeRange(self.begin_label.0..self.begin_file_label.0)
    }

    pub const fn begin_file_code(&self) -> CodeRange {
        CodeRange(self.begin_file_label.0..self.end_file_label.0)
    }

    pub const fn end_file_code(&self) -> CodeRange {
        CodeRange(self.end_file_label.0..self.end_label.0)
    }

    pub const fn end_code(&self) -> CodeRange {
        CodeRange(self.end_label.0..self.rules_label.0)
    }

    pub fn rules_code(&self) -> CodeRange {
        CodeRange(self.rules_label.0..self.len())
    }

    pub const fn funs_code(&self) -> CodeRange {
        CodeRange(self.funs_label.0..self.begin_label.0)
    }
}

impl Instruction {
    pub(super) const fn from_unary(op: UnaryOperator, dest: Reg, arg: TypedArg) -> Self {
        let (arg, ty) = arg.into_arg();
        match op {
            UnaryOperator::Record => Self::LoadF { dest, arg, ty },
            UnaryOperator::Negation => Self::Negation { dest, arg, ty },
            UnaryOperator::ToInt => Self::ToInt { dest, arg, ty },
            UnaryOperator::Negative => Self::Negative { dest, arg, ty },
        }
    }

    pub(super) fn from_binary(op: BinaryOperator, dest: Reg, lhs: TypedArg, rhs: TypedArg) -> Self {
        let ((lhs, tyl), (rhs, tyr)) = (lhs.into_arg(), rhs.into_arg());
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

const fn lower_assign_ops(op: BinaryPlaceOperator) -> Option<BinaryOperator> {
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

const fn unary_place_to_binop(op: UnaryPlaceOperator) -> BinaryOperator {
    match op {
        UnaryPlaceOperator::IncrementL | UnaryPlaceOperator::IncrementR => BinaryOperator::Add,
        UnaryPlaceOperator::DecrementL | UnaryPlaceOperator::DecrementR => BinaryOperator::Subtract,
    }
}
