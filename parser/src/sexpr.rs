// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::fmt::{Debug, Display, Formatter, Result};

use crate::ast::{
    Atom, Body, Expr, ExprNode, Getline, Identifier, Place, Redirection, RulePattern,
    SimpleStatement, Statement, Variable,
};

const PRETTY_PRINT_INDENT: usize = 2;

fn fmt_vars(f: &mut Formatter<'_>) -> (bool, usize, String) {
    let ni = f.width().unwrap_or(0) + PRETTY_PRINT_INDENT;
    (f.alternate(), ni, " ".repeat(ni))
}

impl Debug for Statement<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let (alt, ni, pad) = fmt_vars(f);

        match self {
            Statement::Simple(simple) => <_ as Debug>::fmt(simple, f),
            Self::If { condition, then_body, else_body } => {
                if alt {
                    write!(f, "(if {condition:?}\n{pad}")?;
                    write!(f, "{then_body:#ni$?}")?;
                    if let Some(else_body) = else_body {
                        write!(
                            f,
                            "\n{}(else\n{pad}{else_body:#ni$?}))",
                            &pad[PRETTY_PRINT_INDENT..],
                        )
                    } else {
                        write!(f, ")")
                    }
                } else if let Some(else_body) = else_body {
                    write!(f, "(if {condition:?} {then_body:?} (else {else_body:?}))")
                } else {
                    write!(f, "(if {condition:?} {then_body:?})")
                }
            }
            Self::While { condition, then_body } => {
                if alt {
                    write!(f, "(while {condition:?}\n{pad}{then_body:#ni$?})")
                } else {
                    write!(f, "(while {condition:?} {then_body:?})")
                }
            }
            Self::DoWhile { then_body, condition } => {
                if alt {
                    write!(f, "(do-while\n{pad}{then_body:#ni$?}\n{pad}{condition:?})")
                } else {
                    write!(f, "(do-while {then_body:?} {condition:?})")
                }
            }
            Self::For { init, condition, update, body } => {
                write!(f, "(for")?;
                if alt {
                    let write_fragment = |f: &mut Formatter, x: Option<&dyn Debug>| {
                        if let Some(x) = x {
                            write!(f, "\n{pad}{x:?}")
                        } else {
                            write!(f, "\n{pad}(pass)")
                        }
                    };
                    write_fragment(f, init.as_ref().map(|x| x as _))?;
                    write_fragment(f, condition.as_ref().map(|x| x as _))?;
                    write_fragment(f, update.as_ref().map(|x| x as _))?;
                    write!(f, "\n{pad}{body:#ni$?})")
                } else {
                    let write_fragment = |f: &mut Formatter, x: Option<&dyn Debug>| {
                        if let Some(x) = x {
                            write!(f, " {x:?}")
                        } else {
                            write!(f, " (pass)")
                        }
                    };
                    write_fragment(f, init.as_ref().map(|x| x as _))?;
                    write_fragment(f, condition.as_ref().map(|x| x as _))?;
                    write_fragment(f, update.as_ref().map(|x| x as _))?;
                    write!(f, " {body:?})")
                }
            }
            Self::ForEach { variable, array, body } => {
                if alt {
                    write!(f, "(for-each {variable:?} {array:?}\n{pad}{body:#ni$?})")
                } else {
                    write!(f, "(for-each {variable:?} {array:?} {body:?})")
                }
            }
            Self::Switch { scrutinee, branches, default } => {
                if alt {
                    if let Some((dx, i)) = default {
                        write!(
                            f,
                            "(switch {scrutinee:?}{:#ni$?}\n{pad}(default {i}\n{pad}  {dx:#width$?}))",
                            ListLispCasesFmt(branches.as_slice()),
                            width = ni + PRETTY_PRINT_INDENT,
                        )
                    } else {
                        write!(
                            f,
                            "(switch {scrutinee:?}\n{pad}(cases{:#width$?}))",
                            ListLispCasesFmt(branches),
                            width = ni
                        )
                    }
                } else {
                    if let Some((dx, i)) = default {
                        write!(
                            f,
                            "(switch {scrutinee:?}{:?} (default {i} {dx:?}))",
                            ListLispCasesFmt(branches.as_slice())
                        )
                    } else {
                        write!(f, "(switch {scrutinee:?}{:?})", ListLispCasesFmt(branches))
                    }
                }
            }
            Self::Continue => write!(f, "(continue)"),
            Self::Break => write!(f, "(break)"),
            Self::Return(Some(expr)) => write!(f, "(return {expr:?})"),
            Self::Return(None) => write!(f, "(return)"),
            Self::Exit(Some(expr)) => write!(f, "(exit {expr:?})"),
            Self::Exit(None) => write!(f, "(exit)"),
            Self::Next => write!(f, "(next)"),
            Self::NextFile => write!(f, "(nextfile)"),
        }
    }
}

impl Debug for SimpleStatement<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let (alt, ni, pad) = fmt_vars(f);
        match self {
            Self::Expression(expr) => {
                if alt {
                    write!(f, "{expr:#ni$?}")
                } else {
                    write!(f, "{expr:?}")
                }
            }
            Self::Command { name, args, redirection: Some((rx, expr)) } => {
                if alt {
                    write!(
                        f,
                        "({name:?}{:#width$?}\n{pad}({rx:?} {expr:?}))",
                        ListLispFmt(args),
                        width = ni - PRETTY_PRINT_INDENT
                    )
                } else {
                    write!(f, "({name:?}{:?} ({rx:?} {expr:?}))", ListLispFmt(args))
                }
            }
            Self::Command { name, args, redirection: None } => {
                write!(f, "({name:?}{:?})", ListLispFmt(args))
            }

            Self::Delete(array, Some(index)) => {
                write!(f, "(delete (Index {array:?}")?;
                for i in index {
                    write!(f, " {i}")?;
                }
                write!(f, "))")
            }
            Self::Delete(array, None) => write!(f, "(delete {array:?})"),
        }
    }
}

impl Debug for Expr<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::Leaf(atom) => write!(f, "{atom:?}"),
            Self::Node(expr) => match expr.as_ref() {
                ExprNode::FunctionCall(ident, args) => {
                    write!(f, "({ident:?}")?;
                    for arg in args {
                        write!(f, " {arg:?}")?;
                    }
                    write!(f, ")")
                }
                ExprNode::IndirectCall(ident, args) => {
                    write!(f, "(@{ident:?}")?;
                    for arg in args {
                        write!(f, " {arg:?}")?;
                    }
                    write!(f, ")")
                }
                ExprNode::UnaryOperation(op, a) => write!(f, "({op:?} {a:?})"),
                ExprNode::BinaryOperation(op, a, b) => write!(f, "({op:?} {a:?} {b:?})"),
                ExprNode::BinaryPlaceOperation(op, a, b) => write!(f, "({op:?} {a:?} {b:?})"),
                ExprNode::ArrayOperation(op, arr, args) => {
                    write!(f, "({op:?} {arr:?}")?;
                    for arg in args {
                        write!(f, " {arg:?}")?;
                    }
                    write!(f, ")")
                }
                ExprNode::NestedArray(inner, args) => {
                    write!(f, "(Index {inner:?}")?;
                    for arg in args {
                        write!(f, " {arg:?}")?;
                    }
                    write!(f, ")")
                }
                ExprNode::UnaryPlaceOperation(op, a) => write!(f, "({op:?} {a:?})"),
                ExprNode::Ternary(a, b, c) => write!(f, "(?: {a:?} {b:?} {c:?})"),
                ExprNode::Getline(getline) => match getline {
                    Getline::FromInput(Some(a)) => write!(f, "(getline {a:?})"),
                    Getline::FromInput(None) => write!(f, "(getline)"),
                    Getline::FromFile(Some(a), b) => write!(f, "(getline< {b:?} {a:?})"),
                    Getline::FromFile(None, b) => write!(f, "(getline< {b:?})"),
                    Getline::PipeOut(Some(a), b) => write!(f, "(getline| {b:?} {a:?})"),
                    Getline::PipeOut(None, b) => write!(f, "(getline| {b:?})"),
                    Getline::CoprocessOut(Some(a), b) => write!(f, "(getline|& {b:?} {a:?})"),
                    Getline::CoprocessOut(None, b) => write!(f, "(getline|& {b:?})"),
                },
            },
        }
    }
}

struct ListLispFmt<'a, T: Debug>(&'a [T]);
impl<T: Debug> Debug for ListLispFmt<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let (alt, ni, pad) = fmt_vars(f);
        for e in self.0 {
            if alt {
                write!(f, "\n{pad}{e:#ni$?}")?;
            } else {
                write!(f, " {e:?}")?;
            }
        }
        Ok(())
    }
}

struct ListLispCasesFmt<'a, T: Debug>(&'a [(T, Body<'a>)]);
impl<T: Debug> Debug for ListLispCasesFmt<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let (alt, ni, pad) = fmt_vars(f);
        let (ni, pad) = (ni - PRETTY_PRINT_INDENT, &pad[PRETTY_PRINT_INDENT..]);
        for (i, e) in self.0 {
            if alt {
                write!(
                    f,
                    "\n{pad}(case {i:?}\n{pad}  {e:#width$?})",
                    width = ni + PRETTY_PRINT_INDENT
                )?;
            } else {
                write!(f, " (case {i:?} {e:?})")?;
            }
        }
        Ok(())
    }
}

impl Debug for Identifier<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "{}::{}", self.namespace, self.literal)
    }
}

impl Display for Identifier<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        <&str as Display>::fmt(&self.literal, f)
    }
}

impl Debug for Body<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let (alt, ni, pad) = fmt_vars(f);
        write!(f, "(body")?;
        for e in &self.0 {
            if alt {
                write!(f, "\n{pad}{e:#ni$?}")?;
            } else {
                write!(f, " {e:?}")?;
            }
        }
        write!(f, ")")
    }
}

impl Debug for Atom<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::Variable(var) => write!(f, "{var:?}"),
            Self::String(str) => write!(f, "{str:?}"),
            Self::Number(num) => write!(f, "{num}"),
            Self::Integer(num) => write!(f, "{num}"),
            Self::Regex(rgx) => write!(f, "/{rgx}/"),
            Self::TypedRegex(rgx) => write!(f, "@/{rgx}/"),
            Self::BigInt() | Self::BigFloat() => unimplemented!(),
        }
    }
}

impl Debug for Place<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::Record(expr) => write!(f, "(Record {expr:?})"),
            Self::Variable(var) => <_ as Debug>::fmt(var, f),
            Self::Index(var, index) => {
                write!(f, "(Index {var:?}")?;
                for i in index {
                    write!(f, " {i:?}")?;
                }
                write!(f, ")")
            }
            Self::ChainedIndex(expr, index) => {
                write!(f, "(Index {expr:?}")?;
                for i in index {
                    write!(f, " {i:?}")?;
                }
                write!(f, ")")
            }
        }
    }
}

impl Debug for Variable<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::User(ident) => <Identifier as Debug>::fmt(ident, f),
            Self::Nr => write!(f, "NR"),
            Self::Nf => write!(f, "NF"),
            Self::Fs => write!(f, "FS"),
            Self::Rs => write!(f, "RS"),
            Self::Ofs => write!(f, "OFS"),
            Self::Ors => write!(f, "ORS"),
            Self::Filename => write!(f, "FILENAME"),
            Self::Argc => write!(f, "ARGC"),
            Self::Argv => write!(f, "ARGV"),
            Self::Subsep => write!(f, "SUBSEP"),
            Self::Fnr => write!(f, "FNR"),
            Self::Argind => write!(f, "ARGIND"),
            Self::Ofmt => write!(f, "OFMT"),
            Self::Rstart => write!(f, "RSTART"),
            Self::Rlength => write!(f, "RLENGTH"),
            Self::Environ => write!(f, "ENVIRON"),
        }
    }
}

impl Debug for Redirection {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::WriteFile => write!(f, ">"),
            Self::AppendFile => write!(f, ">>"),
            Self::PipeIn => write!(f, "|"),
            Self::CoprocessIn => write!(f, "|&"),
        }
    }
}

impl Debug for RulePattern<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::Expression(expr) => <_ as Debug>::fmt(expr, f),
            Self::Range(on, off) => write!(f, "(Range {on:?} {off:?})"),
        }
    }
}
