// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::fmt::{Debug, Display, Formatter, Result, Write};

use crate::{
    Ast, Function, Identifier,
    ast::{
        ArrayOperator, Atom, BinaryOperator, BinaryPlaceOperator, BindingPower, Body, Expr,
        ExprNode, Getline, Place, Redirection, Rule, RulePattern, SimpleStatement, Statement,
        Ternary, UnaryOperator, UnaryPlaceOperator, Variable,
    },
};

// Sorry not sorry for the DSL.
macro_rules! fmt_seq {
    ($f:expr $(,)?) => { Ok(()) };
    ($f:expr, select($c:expr, ($($a:tt)*), ($($b:tt)*)) $(, $($rest:tt)*)?) => {{
        if $c {
            fmt_seq!($f, $($a)* $(, $($rest)*)?)
        } else {
            fmt_seq!($f, $($b)* $(, $($rest)*)?)
        }
    }};
    ($f:expr, bt($($munch:tt)*) $(, $($rest:tt)*)?) => {{
        fmt_seq!($f, "[", $($munch)*, "]" $(, $($rest)*)?)
    }};
    ($f:expr, p($($munch:tt)*) $(, $($rest:tt)*)?) => {{
        fmt_seq!($f, "(", $($munch)*, ")" $(, $($rest)*)?)
    }};
    ($f:expr, b($($munch:tt)*) $(, $($rest:tt)*)?) => {{
        fmt_seq!($f, "{{", $($munch)*, "}}" $(, $($rest)*)?)
    }};
    ($f:expr, opt($v:expr, |$ff:ident, $vv:ident| $cb:expr) $(, $($rest:tt)*)?) => {{
        if let Some(x) = $v {
            let ($ff, $vv): (&mut Formatter<'_>, _) = ($f, x);
            fmt_seq!($f, $cb $(, $($rest)*)?)
        } else {
            fmt_seq!($f $(, $($rest)*)?)
        }
    }};
    ($f:expr, maybe($c:expr, p($($munch:tt)*)) $(, $($rest:tt)*)?) => {{
        if $c {
            fmt_seq!($f, "(", $($munch)*, ")" $(, $($rest)*)?)
        } else {
            fmt_seq!($f, $($munch)* $(, $($rest)*)?)
        }
    }};
    ($f:expr, $munch:literal $(, $($rest:tt)*)?) => {{
        write!($f, $munch)?;
        fmt_seq!($f $(, $($rest)*)?)
    }};
    ($f:expr, $munch:expr $(, $($rest:tt)*)?) => {{
        $munch?;
        fmt_seq!($f $(, $($rest)*)?)
    }};
}

struct NamespaceState<'a> {
    tl_ix: usize,
    namespace: &'a str,
}

impl Display for Ast<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let mut state = NamespaceState::default();

        for load in &self.loads {
            state.advance(f, self)?;
            writeln!(f, "@load \"{load}\"")?;
        }

        write_special_rule(self, f, &mut state, "BEGIN", &self.begin)?;
        write_special_rule(self, f, &mut state, "BEGINFILE", &self.begin_file)?;

        for rule in &self.rules {
            state.advance(f, self)?;
            rule.fmt(f, state.namespace)?;
        }

        write_special_rule(self, f, &mut state, "ENDFILE", &self.end_file)?;
        write_special_rule(self, f, &mut state, "END", &self.end)?;

        for (i, (fun, Function { args, body })) in self.functions.iter().enumerate() {
            state.advance(f, self)?;
            fmt_seq!(
                f,
                "function ",
                fun.fmt(f, state.namespace),
                p(write_args(f, args, state.namespace)),
                "\n",
                write_body(f, body, 0, state.namespace)
            )?;
            if i + 1 != self.functions.len() {
                writeln!(f, "\n")?;
            }
        }
        Ok(())
    }
}

fn write_special_rule<'a>(
    this: &Ast<'a>,
    f: &mut Formatter<'_>,
    state: &mut NamespaceState<'a>,
    lb: &str,
    rules: &[Body<'a>],
) -> Result {
    for body in rules {
        state.advance(f, this)?;
        fmt_seq!(f, "{lb} ", write_body_ln(f, body, 0, state.namespace), "\n")?;
    }
    Ok(())
}

impl Statement<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, indent: u8, namespace: &str) -> Result {
        match self {
            Self::Simple(simple) => simple.fmt(f, indent, namespace),
            Self::If { condition, then_body, else_body, .. } => {
                fmt_seq!(
                    f,
                    "if ",
                    p(condition.fmt(f, indent, 0, namespace)),
                    " ",
                    write_body(f, then_body, indent, namespace),
                    opt(else_body, |f, else_body| {
                        write!(f, " else ")?;
                        if let [nest @ Statement::If { .. }] = else_body.0.as_slice() {
                            nest.fmt(f, indent, namespace)
                        } else {
                            write_body(f, else_body, indent, namespace)
                        }
                    })
                )
            }
            Self::While { condition, then_body, .. } => {
                fmt_seq!(f, "while ", p(condition.fmt(f, indent, 0, namespace)), " ")?;
                write_body(f, then_body, indent, namespace)
            }
            Self::DoWhile { then_body, condition, .. } => {
                fmt_seq!(
                    f,
                    "do ",
                    write_body(f, then_body, indent, namespace),
                    " while ",
                    p(condition.fmt(f, indent, 0, namespace))
                )
            }
            Self::For { init, condition, update, body, .. } => {
                fmt_seq!(
                    f,
                    "for ",
                    select(
                        init.is_none() && condition.is_none() && update.is_none(),
                        (p(";;")),
                        (p(
                            opt(init, |f, e| e.fmt(f, indent, namespace)),
                            "; ",
                            opt(condition, |f, e| e.fmt(f, indent, 0, namespace)),
                            "; ",
                            opt(update, |f, e| e.fmt(f, indent, namespace))
                        ))
                    ),
                    " ",
                    write_body(f, body, indent, namespace)
                )
            }
            Self::ForEach { variable, array, body, .. } => {
                fmt_seq!(
                    f,
                    "for ",
                    p(variable.fmt(f, namespace), " in ", array.fmt(f, namespace)),
                    " ",
                    write_body(f, body, indent, namespace)
                )
            }
            Self::Switch { scrutinee, branches, default, .. } => {
                fmt_seq!(
                    f,
                    "switch ",
                    p(scrutinee.fmt(f, indent, 0, namespace)),
                    " {{\n"
                )?;
                let default_pos = default.as_ref().map_or(branches.len(), |x| x.1);
                let print_case = |f: &mut Formatter<'_>, (case, branch): &(Atom<'_>, Body<'_>)| {
                    fmt_seq!(f, tabs(f, indent), "case ", case.fmt(f, namespace), ":\n")?;
                    write_stmnts(f, branch, indent + 1, namespace)
                };
                for i in 0..default_pos {
                    print_case(f, &branches[i])?;
                }
                if let Some((body, pos)) = default {
                    fmt_seq!(f, tabs(f, indent), "default:\n")?;
                    write_stmnts(f, body, indent + 1, namespace)?;
                    for i in *pos..branches.len() {
                        print_case(f, &branches[i])?;
                    }
                }
                tabs(f, indent)?;
                f.write_char('}')
            }
            Self::Break(_) => write!(f, "break"),
            Self::Continue(_) => write!(f, "continue"),
            Self::Return(Some(expr), _) => {
                fmt_seq!(f, "return ", expr.fmt(f, indent, 0, namespace))
            }
            Self::Return(None, _) => write!(f, "return"),
            Self::Exit(Some(expr), _) => fmt_seq!(f, "exit ", expr.fmt(f, indent, 0, namespace)),
            Self::Exit(None, _) => write!(f, "exit"),
            Self::Next(_) => write!(f, "next"),
            Self::NextFile(_) => write!(f, "nextfile"),
        }
    }
}

impl SimpleStatement<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, indent: u8, namespace: &str) -> Result {
        match self {
            SimpleStatement::Expression(expr, _) => expr.fmt(f, indent, 0, namespace),
            SimpleStatement::Command { name, args, redirection: Some((rx, expr)), .. } => {
                fmt_seq!(
                    f,
                    " {name}",
                    p(write_expr_args(f, args, indent, namespace)),
                    "{rx}",
                    expr.fmt(f, indent, 0, namespace)
                )
            }
            SimpleStatement::Command { name, args, redirection: None, .. } => {
                fmt_seq!(f, "{name} ", write_expr_args(f, args, indent, namespace))
            }
            SimpleStatement::Delete(array, Some(args), _) => {
                fmt_seq!(
                    f,
                    "delete ",
                    array.fmt(f, namespace),
                    bt(write_expr_args(f, args, indent, namespace))
                )
            }
            SimpleStatement::Delete(array, None, _) => {
                fmt_seq!(f, "delete ", array.fmt(f, namespace))
            }
        }
    }
}

impl Rule<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, namespace: &str) -> Result {
        match (&self.pattern, &self.actions) {
            (None, None) => Ok(()),
            (None, Some(body)) => write_body_ln(f, body, 0, namespace),
            (Some(pat), None) => {
                fmt_seq!(f, pat.fmt(f, namespace), " {{\n\tprint\n}}")
            }
            (Some(pat), Some(body)) => {
                pat.fmt(f, namespace)?;
                f.write_char(' ')?;
                write_body_ln(f, body, 0, namespace)
            }
        }
    }
}

impl RulePattern<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, namespace: &str) -> Result {
        match self {
            Self::Range(a, b) => {
                fmt_seq!(f, a.fmt(f, 0, 0, namespace), ", ")?;
                b.fmt(f, 0, 0, namespace)
            }
            Self::Expression(x) => x.fmt(f, 0, 0, namespace),
        }
    }
}

impl Expr<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, indent: u8, parent_bp: u8, namespace: &str) -> Result {
        match self {
            Expr::Leaf(atom, _) => atom.fmt(f, namespace),
            Expr::Node(node, _) => node.as_ref().fmt(f, indent, parent_bp, namespace),
        }
    }
}
impl Atom<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, namespace: &str) -> Result {
        if let Self::Variable(var) = self {
            var.fmt(f, namespace)
        } else {
            <Self as Debug>::fmt(self, f)
        }
    }
}

impl Variable<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, namespace: &str) -> Result {
        match self {
            Variable::User(ident) => ident.fmt(f, namespace),
            _ => <_ as Debug>::fmt(self, f),
        }
    }
}

impl Place<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, namespace: &str) -> Result {
        match self {
            Self::Variable(var) => var.fmt(f, namespace),
            Self::Record(Expr::Leaf(leaf, _)) => {
                fmt_seq!(f, "$", leaf.fmt(f, namespace))
            }
            // Handles edge cases of `$(literal) = rvalue`.
            Self::Record(Expr::Node(node, _))
                if let ExprNode::Parenthesized(Expr::Leaf(leaf, _)) = node.as_ref() =>
            {
                fmt_seq!(f, "$", leaf.fmt(f, namespace))
            }
            Self::Record(Expr::Node(node, _)) => {
                fmt_seq!(f, "$", p(node.as_ref().fmt(f, 0, 0, namespace)))
            }
            Self::Index(var, args) => {
                fmt_seq!(
                    f,
                    var.fmt(f, namespace),
                    bt(write_expr_args(f, args, 0, namespace))
                )
            }
            Self::ChainedIndex(var, index) => {
                fmt_seq!(f, var.fmt(f, namespace))?;
                for args in index {
                    fmt_seq!(f, bt(write_expr_args(f, args, 0, namespace)))?;
                }
                Ok(())
            }
        }
    }
}

impl ExprNode<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, indent: u8, parent_bp: u8, namespace: &str) -> Result {
        match self {
            // This is an AST construct; the bp checks resolve parenthesis.
            Self::Parenthesized(expr) => expr.fmt(f, indent, parent_bp, namespace),
            Self::FunctionCall(fun, args) => {
                fmt_seq!(
                    f,
                    fun.fmt(f, namespace),
                    p(write_expr_args(f, args, indent, namespace))
                )
            }
            Self::IndirectCall(var, args) => {
                fmt_seq!(
                    f,
                    "@",
                    var.fmt(f, namespace),
                    p(write_expr_args(f, args, indent, namespace))
                )
            }
            Self::BuiltinCall(fun, args) => {
                fmt_seq!(f, "{fun}", p(write_expr_args(f, args, indent, namespace)))
            }
            Self::UnaryOperation(op, x) => {
                let bp = op.binding_power();
                let child_bp = bp.saturating_add(1);
                fmt_seq!(
                    f,
                    maybe(
                        bp < parent_bp,
                        p("{op}", x.fmt(f, indent, child_bp, namespace))
                    )
                )
            }
            Self::BinaryOperation(op, a, b) => {
                let (left_bp, right_bp) = op.binding_power();
                fmt_seq!(
                    f,
                    maybe(
                        left_bp < parent_bp,
                        p(
                            a.fmt(f, indent, left_bp, namespace),
                            "{op}",
                            b.fmt(f, indent, right_bp, namespace)
                        )
                    )
                )
            }
            Self::UnaryPlaceOperation(op, place) => {
                let bp = op.binding_power();
                match op {
                    UnaryPlaceOperator::IncrementL => {
                        fmt_seq!(f, maybe(bp < parent_bp, p("++", place.fmt(f, namespace))))
                    }
                    UnaryPlaceOperator::DecrementL => {
                        fmt_seq!(f, maybe(bp < parent_bp, p("--", place.fmt(f, namespace))))
                    }
                    UnaryPlaceOperator::DecrementR => {
                        fmt_seq!(f, maybe(bp < parent_bp, p(place.fmt(f, namespace), "--")))
                    }
                    UnaryPlaceOperator::IncrementR => {
                        fmt_seq!(f, maybe(bp < parent_bp, p(place.fmt(f, namespace), "++")))
                    }
                }
            }
            Self::BinaryPlaceOperation(op, place, idx) => {
                let (left_bp, right_bp) = op.binding_power();
                fmt_seq!(
                    f,
                    maybe(
                        left_bp < parent_bp,
                        p(
                            place.fmt(f, namespace),
                            "{op}",
                            idx.fmt(f, indent, right_bp, namespace)
                        )
                    )
                )
            }
            Self::ArrayOperation(op, arr, args) => {
                let (left_bp, right_bp) = op.binding_power();
                fmt_seq!(
                    f,
                    maybe(
                        left_bp < parent_bp,
                        p(match op {
                            ArrayOperator::Index => {
                                fmt_seq!(
                                    f,
                                    arr.fmt(f, namespace),
                                    bt(write_expr_args(f, args, indent, namespace))
                                )
                            }
                            ArrayOperator::In if args.len() > 1 => {
                                fmt_seq!(
                                    f,
                                    p(write_expr_args(f, args, indent, namespace)),
                                    " in ",
                                    arr.fmt(f, namespace)
                                )
                            }
                            ArrayOperator::In => {
                                fmt_seq!(
                                    f,
                                    args[0].fmt(f, indent, right_bp, namespace),
                                    " in ",
                                    arr.fmt(f, namespace)
                                )
                            }
                        })
                    )
                )
            }
            Self::ChainedIndex(var, indices) => {
                let left_bp = ArrayOperator::Index.binding_power().0;
                fmt_seq!(
                    f,
                    maybe(
                        left_bp < parent_bp,
                        p(var.fmt(f, namespace), {
                            for args in indices {
                                fmt_seq!(f, bt(write_expr_args(f, args, 0, namespace)))?;
                            }
                            Ok(())
                        })
                    )
                )
            }
            Self::Ternary(cond, then_expr, else_expr) => {
                let ternary_bp = Ternary.binding_power().0;
                let child_bp = ternary_bp.saturating_add(1);
                fmt_seq!(
                    f,
                    maybe(
                        ternary_bp < parent_bp,
                        p(
                            cond.fmt(f, indent, child_bp, namespace),
                            " ? ",
                            then_expr.fmt(f, indent, child_bp, namespace),
                            " : ",
                            else_expr.fmt(f, indent, child_bp, namespace)
                        )
                    )
                )
            }
            Self::Getline(getline) => match getline {
                Getline::FromInput(Some(var)) => {
                    fmt_seq!(f, "getline ", var.fmt(f, namespace))
                }
                Getline::FromInput(None) => write!(f, "getline"),
                Getline::FromFile(Some(var), file) => {
                    fmt_seq!(
                        f,
                        "getline ",
                        var.fmt(f, namespace),
                        " < ",
                        file.fmt(f, indent, 0, namespace)
                    )
                }
                Getline::FromFile(None, file) => {
                    fmt_seq!(f, "getline < ", file.fmt(f, indent, 0, namespace))
                }
                Getline::PipeOut(Some(place), e) => {
                    fmt_seq!(
                        f,
                        e.fmt(f, indent, 0, namespace),
                        " | getline ",
                        place.fmt(f, namespace)
                    )
                }
                Getline::PipeOut(None, e) => {
                    fmt_seq!(f, e.fmt(f, indent, 0, namespace), " | getline")
                }
                Getline::CoprocessOut(Some(place), e) => {
                    fmt_seq!(
                        f,
                        e.fmt(f, indent, 0, namespace),
                        " |& getline ",
                        place.fmt(f, namespace)
                    )
                }
                Getline::CoprocessOut(None, e) => {
                    fmt_seq!(f, e.fmt(f, indent, 0, namespace), " |& getline")
                }
            },
        }
    }
}

impl Identifier<'_> {
    fn fmt(&self, f: &mut Formatter<'_>, namespace: &str) -> Result {
        if namespace != self.namespace {
            write!(f, "{}::", self.namespace)?;
        }
        <_ as Display>::fmt(self.literal, f)
    }
}

impl Display for UnaryOperator {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::Record => f.write_char('$'),
            Self::Negation => f.write_char('!'),
            Self::ToInt => f.write_char('+'),
            Self::Negative => f.write_char('-'),
        }
    }
}

impl Display for BinaryOperator {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::Concat => f.write_char(' '),
            Self::Eq => write!(f, " == "),
            Self::NEq => write!(f, " != "),
            Self::Gt => write!(f, " > "),
            Self::Lt => write!(f, " < "),
            Self::LtE => write!(f, " <= "),
            Self::GtE => write!(f, " >= "),
            Self::And => write!(f, " && "),
            Self::Or => write!(f, " || "),
            Self::Matches => write!(f, " ~ "),
            Self::MatchesNot => write!(f, " !~ "),
            Self::Add => write!(f, " + "),
            Self::Subtract => write!(f, " - "),
            Self::Multiply => write!(f, " * "),
            Self::Divide => write!(f, " / "),
            Self::Raise => write!(f, " ^ "),
            Self::Modulo => write!(f, " % "),
        }
    }
}

impl Display for BinaryPlaceOperator {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::Assignment => write!(f, " = "),
            Self::AddAssign => write!(f, " += "),
            Self::SubAssign => write!(f, " -= "),
            Self::MulAssign => write!(f, " *= "),
            Self::DivAssign => write!(f, " /= "),
            Self::PowAssign => write!(f, " ^= "),
            Self::ModAssign => write!(f, " %= "),
        }
    }
}

impl Display for Redirection {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Self::WriteFile => write!(f, " > "),
            Self::AppendFile => write!(f, " >> "),
            Self::PipeIn => write!(f, " | "),
            Self::CoprocessIn => write!(f, " |& "),
        }
    }
}

impl<'a> NamespaceState<'a> {
    fn advance(&mut self, f: &mut Formatter<'_>, ast: &Ast<'a>) -> Result {
        // Heuristic: we know search range is upper-bounded by `len() - tl_ix`.
        // Linear search is the best option given constraints.
        if let Some((_, s)) = ast.ns_metadata.iter().find(|&&(i, _)| i == self.tl_ix) {
            self.tl_ix += 1;
            self.namespace = s;
            writeln!(f, "@namespace \"{s}\"\n")?;
        }
        self.tl_ix += 1;
        Ok(())
    }
}

impl Display for Identifier<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        <Self as Debug>::fmt(self, f)
    }
}

impl Default for NamespaceState<'_> {
    fn default() -> Self {
        Self { tl_ix: 0, namespace: "awk" }
    }
}

fn write_cb<T>(
    f: &mut Formatter<'_>,
    args: &[T],
    cb: impl Fn(&mut Formatter<'_>, &T) -> Result,
) -> Result {
    for (i, arg) in args.iter().enumerate() {
        if i != 0 {
            write!(f, ", ")?;
        }
        cb(f, arg)?;
    }
    Ok(())
}

fn write_args(f: &mut Formatter<'_>, args: &[Identifier], namespace: &str) -> Result {
    write_cb(f, args, |f, ident| ident.fmt(f, namespace))
}

fn write_expr_args(f: &mut Formatter<'_>, args: &[Expr], indent: u8, namespace: &str) -> Result {
    write_cb(f, args, |f, expr| expr.fmt(f, indent, 0, namespace))
}

fn write_stmnts(f: &mut Formatter<'_>, body: &Body, indent: u8, namespace: &str) -> Result {
    for stmnt in &body.0 {
        fmt_seq!(f, tabs(f, indent), stmnt.fmt(f, indent, namespace), "\n")?;
    }
    Ok(())
}

fn write_body(f: &mut Formatter<'_>, body: &Body, indent: u8, namespace: &str) -> Result {
    fmt_seq!(
        f,
        b(
            "\n",
            write_stmnts(f, body, indent + 1, namespace),
            tabs(f, indent)
        )
    )
}

fn write_body_ln(f: &mut Formatter<'_>, body: &Body, indent: u8, namespace: &str) -> Result {
    write_body(f, body, indent, namespace)?;
    writeln!(f)
}

fn tabs(f: &mut Formatter<'_>, indent: u8) -> Result {
    for _ in 0..indent {
        f.write_char('\t')?;
    }
    Ok(())
}
