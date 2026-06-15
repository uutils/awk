// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use bumpalo::{collections::Vec, vec};
use lexer::Token;

use crate::{
    IdentifierExt, Lexer, Parser, Result,
    ast::{
        ArrayOperator, Atom, BinaryOperator, BinaryPlaceOperator, BindingPower, Expr, ExprNode,
        Getline, Place, Redirection, Ternary, UnaryOperator, UnaryPlaceOperator, Variable,
        WriteKind,
    },
    diagnostics::ParsingError,
    lex::TokenExt,
};

pub struct Pratt<'a, 'b> {
    parser: &'b mut Parser<'a>,
    typed_regex: bool,
}

impl<'a, 'b> Pratt<'a, 'b> {
    pub fn new(parser: &'b mut Parser<'a>, typed_regex: bool) -> Self {
        Self { parser, typed_regex }
    }

    pub fn parse(&mut self, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        self.parse_expression(lex, 0)
    }

    pub fn parse_command_argument(&mut self, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        let lhs = self.parse_lhs(lex, 0)?;
        self.fold_rhs(lex, lhs, 0, |t| Redirection::parse(t).is_some())
    }

    fn parse_lhs(&mut self, lex: &mut Lexer<'a>, min_bp: u8) -> Result<Expr<'a>> {
        if lex.consume(&Token::OpenParent) {
            self.parse_parenthesized(lex, min_bp)
        } else if lex.peek_with(Token::is_prefix_op) {
            self.parse_prefix(lex)
        } else if lex.consume(&Token::Getline) {
            self.parse_prefix_getline(lex)
        } else {
            let next = lex.expect_next()?;
            self.parse_atom_or_call(lex, next)
        }
    }

    fn parse_expression(&mut self, lex: &mut Lexer<'a>, min_bp: u8) -> Result<Expr<'a>> {
        let lhs = self.parse_lhs(lex, min_bp)?;
        self.fold_rhs(lex, lhs, min_bp, |_| false)
    }

    fn parse_index_exprs(
        &mut self,
        lex: &mut Lexer<'a>,
        op: ArrayOperator,
    ) -> Result<Vec<'a, Expr<'a>>> {
        lex.next();
        let expr = self.parse_expression(lex, op.binding_power().1)?;
        let indices = self.parse_comma_expr(lex, expr)?;
        lex.expect(&Token::ClosedBracket, ParsingError::UnclosedArrayAccess)?;
        Ok(indices)
    }

    fn fold_rhs(
        &mut self,
        lex: &mut Lexer<'a>,
        mut lhs: Expr<'a>,
        min_bp: u8,
        delimiter: impl Fn(&Token<'a>) -> bool,
    ) -> Result<Expr<'a>> {
        while let Some((next, span)) = lex.peek_with_span() {
            let next = next?;
            // Short circuits if requested. Useful for returning early when a
            // token may also match a known operator.
            if delimiter(next) {
                break;
            }
            // Reset typed regex acceptance.
            self.typed_regex = false;
            lhs = if let Ok(op) = UnaryPlaceOperator::parse_suffix(next, &span) {
                if op.binding_power() < min_bp {
                    break;
                }
                match Place::lower_from(lhs.take(), lex.span()) {
                    Ok(place) => {
                        lex.next();
                        Expr::node(op.expr(place), self.parser.arena)
                    }
                    Err((lhs, _)) => Expr::node(
                        BinaryOperator::Concat.expr(lhs, self.parse_prefix(lex)?),
                        self.parser.arena,
                    ),
                }
            } else if let Ok(op) = BinaryPlaceOperator::parse(next, &span) {
                // Places consume assignment operators with maximum precedence;
                // effectively ignoring the enclosing operator's precedence.
                // For example, `1 && x = 1` parses as `1 && (x = 1)`.
                let place = match Place::lower_from(lhs.take(), lex.span()) {
                    Ok(x) => x,
                    Err((expr, _)) => {
                        lhs = expr;
                        if op.binding_power().0 < min_bp {
                            break;
                        }
                        return Err(ParsingError::OperatorExpectsVariable(lex.span()));
                    }
                };
                self.parse_place_op(lex, op, place)?
            } else if let Ok(op) = ArrayOperator::parse(next, &span) {
                match op {
                    ArrayOperator::Index => match Place::lower_from(lhs.take(), lex.span()) {
                        Ok(Place::Variable(var)) => {
                            let index = self.parse_index_exprs(lex, op)?;
                            Expr::node(op.expr(var, index), self.parser.arena)
                        }
                        Ok(Place::Index(var, index)) => {
                            let new_indices = self.parse_index_exprs(lex, op)?;
                            let inner = Expr::node(
                                ExprNode::ArrayOperation(ArrayOperator::Index, var, index),
                                self.parser.arena,
                            );
                            Expr::node(ExprNode::NestedArray(inner, new_indices), self.parser.arena)
                        }
                        Ok(Place::ChainedIndex(arr, indices)) => {
                            let new_indices = self.parse_index_exprs(lex, op)?;
                            let inner =
                                Expr::node(ExprNode::NestedArray(arr, indices), self.parser.arena);
                            Expr::node(ExprNode::NestedArray(inner, new_indices), self.parser.arena)
                        }
                        Ok(_) => return Err(ParsingError::OperatorExpectsVariable(lex.span())),
                        Err((expr, _)) => {
                            lhs = expr;
                            if op.binding_power().0 < min_bp {
                                break;
                            }
                            return Err(ParsingError::OperatorExpectsVariable(lex.span()));
                        }
                    },
                    ArrayOperator::In => {
                        lex.next();
                        let Place::Variable(var) = self.parse_place(lex)? else {
                            return Err(ParsingError::OperatorExpectsVariable(lex.span()));
                        };
                        Expr::node(
                            op.expr(var, vec![in self.parser.arena; lhs.take()]),
                            self.parser.arena,
                        )
                    }
                }
            } else if let Ok(op) = BinaryOperator::parse(next, &span)
                && !matches!(next, Token::Increment | Token::Decrement)
            {
                if op.binding_power().0 < min_bp {
                    break;
                }
                self.parse_infix_op(lex, op, lhs)?
            } else if next == &Token::QuestionMark {
                if Ternary.binding_power().0 < min_bp {
                    break;
                }
                self.parse_ternary(lex, lhs)?
            } else if let Some(op) = WriteKind::parse(next) {
                if BinaryOperator::Concat.binding_power().0 < min_bp {
                    break;
                }
                self.parse_getline_pipe(lex, op, lhs)?
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_parenthesized(&mut self, lex: &mut Lexer<'a>, min_bp: u8) -> Result<Expr<'a>> {
        self.typed_regex = false;
        let inner = self.parse(lex)?;
        if min_bp < UnaryOperator::Record.binding_power() && lex.peek_is(&Token::Comma) {
            let expr = self.parse_comma_expr(lex, inner)?;
            lex.expect(
                &Token::ClosedParent,
                ParsingError::UnclosedParenthesisExpression,
            )?;
            lex.expect(&Token::In, |s| {
                ParsingError::UnexpectedToken(
                    s,
                    "expected `in` after multidimensional array look-up.".into(),
                )
            })?;
            let start = lex.span().start;
            let Place::Variable(var) = self.parse_place(lex)? else {
                return Err(ParsingError::OperatorExpectsVariable(start..lex.span().end));
            };
            Ok(Expr::node(
                ArrayOperator::In.expr(var, expr),
                self.parser.arena,
            ))
        } else {
            lex.expect(
                &Token::ClosedParent,
                ParsingError::UnclosedParenthesisExpression,
            )?;
            Ok(inner)
        }
    }

    fn parse_prefix(&mut self, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        let next = lex.expect_next()?;
        // No prefix operator accepts them.
        self.typed_regex = false;
        if let Ok(op) = UnaryPlaceOperator::parse_prefix(&next, &lex.span()) {
            let rhs = self.parse_place(lex)?;
            Ok(Expr::node(op.expr(rhs), self.parser.arena))
        } else if let Ok(op) = UnaryOperator::parse(&next, &lex.peeked_span()?) {
            let rhs = self.parse_expression(lex, op.binding_power())?;
            Ok(Expr::node(op.expr(rhs), self.parser.arena))
        } else {
            Err(ParsingError::InvalidExpression(
                lex.span(),
                "expected a valid prefix operator".into(),
            ))
        }
    }

    fn parse_prefix_getline(&mut self, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        // Consumes with maximum precedence the following place and/or
        // redirection reading from file. Does not accept typed regexes.
        self.typed_regex = false;
        let place = if lex.peek_with(Token::is_place) {
            Some(Place::lower_from(self.parse_redirection(lex)?, lex.span()))
        } else {
            None
        }
        .transpose(); // trick to simplify checks.

        let getline = |gl| Expr::node(ExprNode::Getline(gl), self.parser.arena);
        match place {
            // Nonsensical expression; gawk just assumes concatenation.
            Err((expr, _)) => Ok(Expr::node(
                BinaryOperator::Concat.expr(getline(Getline::FromInput(None)), expr),
                self.parser.arena,
            )),
            Ok(place) => {
                if lex.consume(&Token::LesserThan) {
                    let file = self.parse_expression(lex, BinaryOperator::Lt.binding_power().1)?;
                    Ok(getline(Getline::FromFile(place, file)))
                } else {
                    Ok(getline(Getline::FromInput(place)))
                }
            }
        }
    }

    fn parse_atom_or_call(&mut self, lex: &mut Lexer<'a>, next: Token<'a>) -> Result<Expr<'a>> {
        // Only accepts calls if the function name is next to the parenthesis.
        // If there is a space, we interpret it as a concatenation and let the
        // interpreter error if necessary; elsewhere we can't concat with vars.
        if let Token::Identifier(name) = next
            && lex.peek_is(&Token::OpenParent)
            && lex.is_yuxtaposed()
        {
            if name.namespace.is_none_or(|n| n == "awk") && is_special_var(name.literal) {
                return Err(ParsingError::SpecialVariableCall(
                    lex.span(),
                    name.literal.to_string(),
                ));
            }
            self.parser.parse_function_call(
                lex,
                |args| ExprNode::FunctionCall(name.qualify(self.parser.namespace), args),
                lex.span(),
            )
        } else if let Some(builtin) = next.maps_to_builtin() {
            self.parser.parse_function_call(
                lex,
                |args| ExprNode::BuiltinCall(builtin, args),
                lex.span(),
            )
        } else if let Token::IndirectCall(name) = next {
            // Possible gawk bug: it accepts special variables if qualified,
            // even if it is with the `awk` namespace.
            if name.namespace.is_none() && is_special_var(name.literal) {
                return Err(ParsingError::SpecialVariableIndirectCall(
                    lex.span(),
                    name.literal.to_string(),
                ));
            }
            let name = Variable::User(name.qualify(self.parser.namespace));
            self.parser.parse_function_call(
                lex,
                |args| ExprNode::IndirectCall(name, args),
                lex.span(),
            )
        } else if next.is_place() && lex.peek_is(&Token::OpenParent) && lex.is_yuxtaposed() {
            let name = match self.parser.get_place(lex, next) {
                Ok(var) => var.to_string(),
                Err(tok) => format!("{tok:?}"),
            };
            Err(ParsingError::SpecialVariableCall(lex.span(), name))
        } else {
            match self.parser.parse_atom(lex, next, self.typed_regex) {
                Ok(atom) => Ok(Expr::leaf(atom)),
                // Add detail to this error.
                Err(ParsingError::UnexpectedToken(_, str)) => {
                    Err(ParsingError::InvalidExpression(lex.span(), str))
                }
                Err(e) => Err(e),
            }
        }
    }

    fn parse_infix_op(
        &mut self,
        lex: &mut Lexer<'a>,
        op: BinaryOperator,
        lhs: Expr<'a>,
    ) -> Result<Expr<'a>> {
        // Ensures it's not a typed regex; rejects cases like `x = @/a/ + 1`.
        self.typecheck(lex, &lhs)?;
        // This is just a parsing construct; we only skip if it's a real token.
        lex.consume_with(|_| op != BinaryOperator::Concat);
        // Checks invalids like `a == b == c`. The docs are ambiguous about the
        // associativity of redirection operators, but I couldn't get awk to
        // error out when chaining them.
        if op.is_non_associative() && lhs.is_non_associative() {
            return Err(ParsingError::NonAssociativeOperator(lex.span()));
        }
        let is_regex = matches!(op, BinaryOperator::Matches | BinaryOperator::MatchesNot);
        self.typed_regex = is_regex;

        let mut rhs = self.parse_expression(lex, op.binding_power().1)?;
        if is_regex && let Expr::Leaf(Atom::Regex(r)) = rhs {
            // Has interactions with pretty printing, but makes the interpreter easier.
            rhs = Expr::Leaf(Atom::TypedRegex(r));
        }
        Ok(Expr::node(op.expr(lhs, rhs), self.parser.arena))
    }

    fn parse_place_op(
        &mut self,
        lex: &mut Lexer<'a>,
        op: BinaryPlaceOperator,
        place: Place<'a>,
    ) -> Result<Expr<'a>> {
        lex.next();
        self.typed_regex = matches!(op, BinaryPlaceOperator::Assignment);
        // Assignment expressions can consume with maximum precedence a
        // following typed regex, so it bypasses ternaries (the only operations
        // with lesser binding power); i.e., we parse `x = @/a/ ? a : b` into
        // `(?: (= x @/a/) a b)`. This is generally true for all positions of
        // typed regexes, but only an edge case here.
        let rhs = if self.typed_regex
            && let Some(Token::TypedRegex(slice)) =
                lex.next_if(|t| matches!(t, Token::TypedRegex(_)))?
        {
            let lhs = Expr::Leaf(Atom::TypedRegex(slice));
            // We fold it in order to catch invalid cases, like `x = @/a/ + 1`.
            // Also allows us to bypass ternaries without binding power hacks.
            self.fold_rhs(lex, lhs, op.binding_power().0, |t| {
                t == &Token::QuestionMark
            })?
        } else {
            self.parse_expression(lex, op.binding_power().1)?
        };
        Ok(Expr::node(op.expr(place, rhs), self.parser.arena))
    }

    /// Parses a given place/value receiver/lvalue. These are non-parenthesized
    /// identifiers, array accesses, and records. This functions ensures parsing
    /// is non-greedy.
    fn parse_place(&mut self, lex: &mut Lexer<'a>) -> Result<Place<'a>> {
        let start = lex.peeked_span()?.start;
        let lhs = match lex.expect_peek()? {
            Token::Record => {
                lex.next();
                return self
                    .parse_expression(lex, UnaryOperator::Record.binding_power())
                    .map(Place::Record);
            }
            Token::OpenParent => {
                // advance expression for nicer errors
                let _ = self.parse_expression(lex, 0);
                Expr::Leaf(Atom::Number(0.))
            }
            tok if tok.is_place() => {
                let expr = self.parse_lhs(lex, 0)?;
                if lex.peek_is(&Token::OpenBracket) {
                    let Expr::Leaf(Atom::Variable(var)) = expr else {
                        return Err(ParsingError::OperatorExpectsVariable(start..lex.span().end));
                    };

                    let index = self.parse_index_exprs(lex, ArrayOperator::Index)?;

                    if !lex.peek_is(&Token::OpenBracket) {
                        return Ok(Place::Index(var, index));
                    }

                    let mut lhs = Expr::node(
                        ExprNode::ArrayOperation(ArrayOperator::Index, var, index),
                        self.parser.arena,
                    );

                    while lex.peek_is(&Token::OpenBracket) {
                        let index = self.parse_index_exprs(lex, ArrayOperator::Index)?;
                        if lex.peek_is(&Token::OpenBracket) {
                            lhs = Expr::node(ExprNode::NestedArray(lhs, index), self.parser.arena);
                        } else {
                            return Ok(Place::ChainedIndex(lhs, index));
                        }
                    }
                }
                expr
            }
            _ => {
                lex.next();
                Expr::Leaf(Atom::Number(0.)) // force error below
            }
        };
        Place::lower_from(lhs, start..lex.span().end).map_err(Into::into)
    }

    /// Continuously consumes comma-separated expressions.
    pub fn parse_comma_expr(
        &mut self,
        lex: &mut Lexer<'a>,
        lhs: Expr<'a>,
    ) -> Result<Vec<'a, Expr<'a>>> {
        let mut rhs = vec![in self.parser.arena; lhs];
        while lex.consume(&Token::Comma) {
            rhs.push(self.parse(lex)?);
        }
        Ok(rhs)
    }

    fn parse_ternary(&mut self, lex: &mut Lexer<'a>, lhs: Expr<'a>) -> Result<Expr<'a>> {
        // There should be no need to typecheck lhs since there is no way it
        // wasn't caught first, but checking is cheap, so we make sure.
        self.typecheck(lex, &lhs)?;
        let right_bp = Ternary.binding_power().1;
        lex.next();
        let then_branch = self.parse_expression(lex, right_bp)?;
        lex.expect(&Token::Colon, ParsingError::MissingTernaryOr)?;
        let else_branch = self.parse_expression(lex, right_bp)?;
        Ok(Expr::node(
            ExprNode::Ternary(lhs, then_branch, else_branch),
            self.parser.arena,
        ))
    }

    fn parse_getline_pipe(
        &mut self,
        lex: &mut Lexer<'a>,
        op: WriteKind,
        lhs: Expr<'a>,
    ) -> Result<Expr<'a>> {
        lex.next();
        lex.expect(&Token::Getline, |span| {
            ParsingError::UnexpectedToken(
                span,
                "operand must precede `getline` in an expression.".into(),
            )
        })?;

        let pipe = |place| Expr::node(op.expr_getline(place, lhs), self.parser.arena);
        if lex.peek_with(Token::is_place) {
            let expr = self.parse_redirection(lex)?;
            match Place::lower_from(expr, lex.span()) {
                Ok(place) => Ok(pipe(Some(place))),
                Err((expr, _)) => Ok(Expr::node(
                    BinaryOperator::Concat.expr(pipe(None), expr),
                    self.parser.arena,
                )),
            }
        } else {
            Ok(pipe(None))
        }
    }

    pub fn parse_redirection(&mut self, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        self.parse_expression(lex, BinaryOperator::Concat.binding_power().1 - 1)
    }

    /// Errors if `expr` is a typed regex.
    fn typecheck(&self, lex: &mut Lexer<'a>, expr: &Expr<'a>) -> Result<()> {
        if matches!(expr, Expr::Leaf(Atom::TypedRegex(_))) {
            Err(ParsingError::UnexpectedTypedRegex(lex.span()))
        } else {
            Ok(())
        }
    }
}

trait NonAssociativity {
    fn is_non_associative(&self) -> bool;
}

impl NonAssociativity for Expr<'_> {
    fn is_non_associative(&self) -> bool {
        matches!(
            self,
            Expr::Node(x) if matches!(x.as_ref(), ExprNode::BinaryOperation(
                op,
                _,
                _
            ) if op.is_non_associative())
        )
    }
}

impl NonAssociativity for BinaryOperator {
    fn is_non_associative(&self) -> bool {
        matches!(
            self,
            Self::Eq | Self::NEq | Self::Gt | Self::Lt | Self::LtE | Self::GtE,
        )
    }
}

fn is_special_var(name: &str) -> bool {
    matches!(
        name,
        "NR" | "NF"
            | "FS"
            | "RS"
            | "OFS"
            | "ORS"
            | "FILENAME"
            | "ARGC"
            | "ARGV"
            | "SUBSEP"
            | "FNR"
            | "ARGIND"
            | "OFMT"
            | "RSTART"
            | "RLENGTH"
            | "ENVIRON"
    )
}
