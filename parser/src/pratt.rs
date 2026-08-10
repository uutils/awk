// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use bumpalo::{collections::Vec, vec};
use lexer::{Span, Token};

use crate::{
    IdentifierExt, Lexer, Parser, Result,
    ast::{
        ArrayOperator, Atom, BinaryOperator, BinaryPlaceOperator, BindingPower, Expr, ExprNode,
        Getline, Place, Redirection, Ternary, UnaryOperator, UnaryPlaceOperator, Variable,
        WriteKind,
    },
    diagnostics::ParsingError,
    lex::{SpanExt, TokenExt},
};

fn extend_operator_expects_variable(err: ParsingError, span: Span) -> ParsingError {
    if matches!(err, ParsingError::OperatorExpectsVariable(_)) {
        ParsingError::OperatorExpectsVariable(span)
    } else {
        err
    }
}

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
        let anchor = lex.peeked_span()?.start;
        let lhs = self.parse_lhs(lex, 0)?;
        self.fold_rhs(lex, lhs, anchor, 0, |t| Redirection::parse(t).is_some())
    }

    fn parse_lhs(&mut self, lex: &mut Lexer<'a>, min_bp: u8) -> Result<Expr<'a>> {
        if lex.consume(&Token::OpenParent) {
            let anchor = lex.span().start;
            self.parse_parenthesized(lex, min_bp, anchor)
        } else if lex.peek_with(Token::is_prefix_op) {
            self.parse_prefix(lex)
        } else if lex.consume(&Token::Getline) {
            self.parse_prefix_getline(lex)
        } else {
            let next = lex.expect_next()?;
            let anchor = lex.span().start;
            self.parse_atom_or_call(lex, next, anchor)
        }
    }

    fn parse_expression(&mut self, lex: &mut Lexer<'a>, min_bp: u8) -> Result<Expr<'a>> {
        let anchor = lex.peeked_span()?.start;
        let lhs = self.parse_lhs(lex, min_bp)?;
        self.fold_rhs(lex, lhs, anchor, min_bp, |_| false)
    }

    fn parse_index_exprs(
        &mut self,
        lex: &mut Lexer<'a>,
        op: ArrayOperator,
        expr_anchor: usize,
    ) -> Result<Vec<'a, Expr<'a>>> {
        let _bracket = lex.peeked_span()?.start;
        lex.next();
        let expr = self.parse_expression(lex, op.binding_power().1)?;
        let indices = self.parse_comma_expr(lex, expr)?;
        lex.expect(&Token::ClosedBracket, |s| {
            ParsingError::UnclosedArrayAccess(s.since(expr_anchor))
        })?;
        Ok(indices)
    }

    pub fn fold_rhs(
        &mut self,
        lex: &mut Lexer<'a>,
        mut lhs: Expr<'a>,
        expr_anchor: usize,
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
            lhs = if let Ok(op) = UnaryPlaceOperator::parse_suffix(next, span) {
                if op.binding_power() < min_bp {
                    break;
                }
                match Place::lower_from(lhs.take(), span.since(expr_anchor)) {
                    Ok(place) => {
                        lex.next();
                        let node_span = lex.span().since(expr_anchor);
                        Expr::node(op.expr(place), self.parser, node_span)
                    }
                    Err((lhs, _)) => {
                        let rhs = self.parse_prefix(lex)?;
                        let node_span = lex.span().since(expr_anchor);
                        Expr::node(
                            BinaryOperator::Concat.expr(lhs, rhs),
                            self.parser,
                            node_span,
                        )
                    }
                }
            } else if let Ok(op) = BinaryPlaceOperator::parse(next, span) {
                // Places consume assignment operators with maximum precedence
                // on exprs with certain operators, overriding their precedence.
                // For example, `1 && x = 1` parses as `1 && (x = 1)`.
                if min_bp >= BinaryOperator::Concat.binding_power().0 {
                    break;
                }
                let place = match Place::lower_from(lhs.take(), span.since(expr_anchor)) {
                    Ok(x) => x,
                    Err((expr, _)) => {
                        lhs = expr;
                        if op.binding_power().0 < min_bp {
                            break;
                        }
                        return Err(ParsingError::OperatorExpectsVariable(
                            span.since(expr_anchor),
                        ));
                    }
                };
                self.parse_place_op(lex, op, place, expr_anchor)?
            } else if let Ok(op) = ArrayOperator::parse(next, span) {
                match op {
                    ArrayOperator::Index => {
                        match Place::lower_from(lhs.take(), span.since(expr_anchor)) {
                            Ok(Place::Variable(var)) => {
                                let index = self.parse_index_exprs(lex, op, expr_anchor)?;
                                let node_span = lex.span().since(expr_anchor);
                                Expr::node(op.expr(var, index), self.parser, node_span)
                            }
                            Ok(Place::Index(var, index)) => {
                                let new_indices = self.parse_index_exprs(lex, op, expr_anchor)?;
                                let indices = vec![in self.parser.arena; index, new_indices];
                                let node_span = lex.span().since(expr_anchor);
                                Expr::node(
                                    ExprNode::ChainedIndex(var, indices),
                                    self.parser,
                                    node_span,
                                )
                            }
                            Ok(Place::ChainedIndex(var, mut indices)) => {
                                let new_indices = self.parse_index_exprs(lex, op, expr_anchor)?;
                                indices.push(new_indices);
                                let node_span = lex.span().since(expr_anchor);
                                Expr::node(
                                    ExprNode::ChainedIndex(var, indices),
                                    self.parser,
                                    node_span,
                                )
                            }
                            Ok(_) => {
                                return Err(ParsingError::OperatorExpectsVariable(
                                    span.since(expr_anchor),
                                ));
                            }
                            Err((expr, _)) => {
                                lhs = expr;
                                if op.binding_power().0 < min_bp {
                                    break;
                                }
                                return Err(ParsingError::OperatorExpectsVariable(
                                    span.since(expr_anchor),
                                ));
                            }
                        }
                    }
                    ArrayOperator::In => {
                        lex.next();
                        let Place::Variable(var) = self.parse_place(lex)? else {
                            return Err(ParsingError::OperatorExpectsVariable(
                                lex.span().since(expr_anchor),
                            ));
                        };
                        let node_span = lex.span().since(expr_anchor);
                        Expr::node(
                            op.expr(var, vec![in self.parser.arena; lhs.take()]),
                            self.parser,
                            node_span,
                        )
                    }
                }
            } else if let Ok(op) = BinaryOperator::parse(next, span)
                && !matches!(next, Token::Increment | Token::Decrement)
            {
                if op.binding_power().0 < min_bp {
                    break;
                }
                self.parse_infix_op(lex, op, lhs, expr_anchor)?
            } else if next == &Token::QuestionMark {
                if Ternary.binding_power().0 < min_bp {
                    break;
                }
                self.parse_ternary(lex, lhs, expr_anchor)?
            } else if let Some(op) = WriteKind::parse(next) {
                if BinaryOperator::Concat.binding_power().0 < min_bp {
                    break;
                }
                self.parse_getline_pipe(lex, op, lhs, expr_anchor)?
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_parenthesized(
        &mut self,
        lex: &mut Lexer<'a>,
        min_bp: u8,
        anchor: usize,
    ) -> Result<Expr<'a>> {
        self.typed_regex = false;
        let inner = self.parse(lex)?;
        // Handle cases where the parenthesis are the lhs of an mdim `in` array.
        if min_bp < UnaryOperator::Record.binding_power() && lex.peek_is(&Token::Comma) {
            let expr = self.parse_comma_expr(lex, inner)?;
            lex.expect(&Token::ClosedParent, |s| {
                ParsingError::UnclosedParenthesisExpression(s.since(anchor))
            })?;
            lex.expect(&Token::In, |s| {
                ParsingError::UnexpectedToken(
                    s,
                    "expected `in` after multidimensional array look-up.".into(),
                )
            })?;
            let Place::Variable(var) = self.parse_place(lex)? else {
                return Err(ParsingError::OperatorExpectsVariable(
                    lex.span().since(anchor),
                ));
            };
            let node_span = lex.span().since(anchor);
            Ok(Expr::node(
                ArrayOperator::In.expr(var, expr),
                self.parser,
                node_span,
            ))
        } else {
            lex.expect(&Token::ClosedParent, |s| {
                ParsingError::UnclosedParenthesisExpression(s.since(anchor))
            })?;
            let node_span = lex.span().since(anchor);
            let inner = Expr::node(ExprNode::Parenthesized(inner), self.parser, node_span);
            Ok(inner)
        }
    }

    fn parse_prefix(&mut self, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        let anchor = lex.peeked_span()?.start;
        let next = lex.expect_next()?;
        // No prefix operator accepts them.
        self.typed_regex = false;
        if let Ok(op) = UnaryPlaceOperator::parse_prefix(&next, lex.span().since(anchor)) {
            let rhs = self
                .parse_place(lex)
                .map_err(|e| extend_operator_expects_variable(e, lex.span().since(anchor)))?;
            let node_span = lex.span().since(anchor);
            Ok(Expr::node(op.expr(rhs), self.parser, node_span))
        } else if let Ok(op) = UnaryOperator::parse(&next, lex.peeked_span()?) {
            let rhs = self.parse_expression(lex, op.binding_power())?;
            let node_span = lex.span().since(anchor);
            Ok(Expr::node(op.expr(rhs), self.parser, node_span))
        } else {
            Err(ParsingError::InvalidExpression(
                lex.span().since(anchor),
                "expected a valid prefix operator".into(),
            ))
        }
    }

    fn parse_prefix_getline(&mut self, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        // Consumes with maximum precedence the following place and/or
        // redirection reading from file. Does not accept typed regexes.
        let anchor = lex.span().start;
        let keyword_span = lex.span();
        self.typed_regex = false;
        let place = if lex.peek_with(Token::is_place) {
            Some(Place::lower_from(
                self.parse_redirection(lex)?,
                lex.span().since(anchor),
            ))
        } else {
            None
        }
        .transpose(); // trick to simplify checks.

        match place {
            // Nonsensical expression; gawk just assumes concatenation.
            Err((expr, _)) => {
                let getline = Expr::node(
                    ExprNode::Getline(Getline::FromInput(None)),
                    self.parser,
                    keyword_span.since(anchor),
                );
                let node_span = lex.span().since(anchor);
                Ok(Expr::node(
                    BinaryOperator::Concat.expr(getline, expr),
                    self.parser,
                    node_span,
                ))
            }
            Ok(place) => {
                if lex.consume(&Token::LesserThan) {
                    let file = self.parse_expression(lex, BinaryOperator::Lt.binding_power().1)?;
                    let node_span = lex.span().since(anchor);
                    Ok(Expr::node(
                        ExprNode::Getline(Getline::FromFile(place, file)),
                        self.parser,
                        node_span,
                    ))
                } else {
                    let node_span = lex.span().since(anchor);
                    Ok(Expr::node(
                        ExprNode::Getline(Getline::FromInput(place)),
                        self.parser,
                        node_span,
                    ))
                }
            }
        }
    }

    fn parse_atom_or_call(
        &mut self,
        lex: &mut Lexer<'a>,
        next: Token<'a>,
        anchor: usize,
    ) -> Result<Expr<'a>> {
        // Only accepts calls if the function name is next to the parenthesis.
        // If there is a space, we interpret it as a concatenation and let the
        // interpreter error if necessary; elsewhere we can't concat with vars.
        if let Token::Identifier(name) = next {
            let name = name.qualify(lex, self.parser.namespace)?;
            if lex.peek_is(&Token::OpenParent) {
                self.parser.parse_function_call(
                    lex,
                    |args| ExprNode::FunctionCall(name, args),
                    lex.span(),
                )
            } else {
                let leaf_span = lex.span().since(anchor);
                Ok(Expr::leaf(
                    Atom::Variable(Variable::User(name)),
                    self.parser,
                    leaf_span,
                ))
            }
        } else if let Some(builtin) = next.maps_to_builtin() {
            self.parser.parse_function_call(
                lex,
                |args| ExprNode::BuiltinCall(builtin, args),
                lex.span(),
            )
        } else if let Token::IndirectCall(name) = next {
            // BUG(gawk): it accepts special variables iff qualified,
            // even if it is with the `awk` namespace.
            let name = Variable::User(name.qualify(lex, self.parser.namespace)?);
            self.parser.parse_function_call(
                lex,
                |args| ExprNode::IndirectCall(name, args),
                lex.span().since(anchor),
            )
        } else if next.is_place() && lex.peek_is(&Token::OpenParent) && lex.is_yuxtaposed() {
            let name = match self.parser.get_place(lex, next) {
                Ok(var) => format!("{var:?}"),
                Err((_, tok)) => format!("{tok:?}"),
            };
            Err(ParsingError::SpecialVariableCall(
                lex.span().since(anchor),
                name,
            ))
        } else {
            match self.parser.parse_atom(lex, next, self.typed_regex) {
                Ok(atom) => {
                    let leaf_span = lex.span().since(anchor);
                    Ok(Expr::leaf(atom, self.parser, leaf_span))
                }
                // Add detail to this error.
                Err(ParsingError::UnexpectedToken(_, str)) => Err(ParsingError::InvalidExpression(
                    lex.span().since(anchor),
                    str,
                )),
                Err(e) => Err(e),
            }
        }
    }

    fn parse_infix_op(
        &mut self,
        lex: &mut Lexer<'a>,
        op: BinaryOperator,
        lhs: Expr<'a>,
        expr_anchor: usize,
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

        let rhs_anchor = lex.peeked_span()?.start;
        let mut rhs = self.parse_expression(lex, op.binding_power().1)?;
        if is_regex && let Expr::Leaf(Atom::Regex(r), _) = rhs {
            // Has interactions with pretty printing, but makes the interpreter easier.
            let rhs_span = lex.span().since(rhs_anchor);
            rhs = Expr::leaf(Atom::TypedRegex(r), self.parser, rhs_span);
        }
        let node_span = lex.span().since(expr_anchor);
        Ok(Expr::node(op.expr(lhs, rhs), self.parser, node_span))
    }

    fn parse_place_op(
        &mut self,
        lex: &mut Lexer<'a>,
        op: BinaryPlaceOperator,
        place: Place<'a>,
        expr_anchor: usize,
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
            let anchor = lex.span().start;
            let leaf_span = lex.span().since(anchor);
            let lhs = Expr::leaf(Atom::TypedRegex(slice), self.parser, leaf_span);
            // We fold it in order to catch invalid cases, like `x = @/a/ + 1`.
            // Also allows us to bypass ternaries without binding power hacks.
            self.fold_rhs(lex, lhs, anchor, op.binding_power().0, |t| {
                t == &Token::QuestionMark
            })?
        } else {
            self.parse_expression(lex, op.binding_power().1)?
        };
        let node_span = lex.span().since(expr_anchor);
        Ok(Expr::node(op.expr(place, rhs), self.parser, node_span))
    }

    /// Parses a given place/value receiver/lvalue. These are non-parenthesized
    /// identifiers, array accesses, and records. This functions ensures parsing
    /// is non-greedy.
    pub fn parse_place(&mut self, lex: &mut Lexer<'a>) -> Result<Place<'a>> {
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
                Expr::leaf(Atom::Number(0.), self.parser, lex.span().since(start))
            }
            tok if tok.is_place() => {
                let expr = self.parse_lhs(lex, 0)?;
                if lex.peek_is(&Token::OpenBracket) {
                    let Expr::Leaf(Atom::Variable(var), _) = expr else {
                        return Err(ParsingError::OperatorExpectsVariable(
                            lex.span().since(start),
                        ));
                    };

                    let index = self.parse_index_exprs(lex, ArrayOperator::Index, start)?;

                    if !lex.peek_is(&Token::OpenBracket) {
                        return Ok(Place::Index(var, index));
                    }

                    let mut indices = vec![in self.parser.arena; index];

                    while lex.peek_is(&Token::OpenBracket) {
                        let index = self.parse_index_exprs(lex, ArrayOperator::Index, start)?;
                        indices.push(index);
                    }

                    return Ok(Place::ChainedIndex(var, indices));
                }
                expr
            }
            _ => {
                lex.next();
                // force error below
                Expr::leaf(Atom::Number(0.), self.parser, lex.span().since(start))
            }
        };
        Place::lower_from(lhs, lex.span().since(start)).map_err(Into::into)
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

    fn parse_ternary(
        &mut self,
        lex: &mut Lexer<'a>,
        lhs: Expr<'a>,
        expr_anchor: usize,
    ) -> Result<Expr<'a>> {
        // There should be no need to typecheck lhs since there is no way it
        // wasn't caught first, but checking is cheap, so we make sure.
        self.typecheck(lex, &lhs)?;
        let right_bp = Ternary.binding_power().1;
        lex.next();
        let then_branch = self.parse_expression(lex, right_bp)?;
        lex.expect(&Token::Colon, ParsingError::MissingTernaryOr)?;
        let else_branch = self.parse_expression(lex, right_bp)?;
        let node_span = lex.span().since(expr_anchor);
        Ok(Expr::node(
            ExprNode::Ternary(lhs, then_branch, else_branch),
            self.parser,
            node_span,
        ))
    }

    fn parse_getline_pipe(
        &mut self,
        lex: &mut Lexer<'a>,
        op: WriteKind,
        lhs: Expr<'a>,
        expr_anchor: usize,
    ) -> Result<Expr<'a>> {
        lex.next();
        lex.expect(&Token::Getline, |span| {
            ParsingError::UnexpectedToken(
                span,
                "operand must precede `getline` in an expression.".into(),
            )
        })?;
        let getline_span = lex.span();

        if lex.peek_with(Token::is_place) {
            let anchor = lex.peeked_span()?.start;
            let expr = self.parse_redirection(lex)?;
            match Place::lower_from(expr, lex.span().since(anchor)) {
                Ok(place) => {
                    let node_span = lex.span().since(expr_anchor);
                    Ok(Expr::node(
                        op.expr_getline(Some(place), lhs),
                        self.parser,
                        node_span,
                    ))
                }
                Err((expr, _)) => {
                    let pipe = Expr::node(
                        op.expr_getline(None, lhs),
                        self.parser,
                        getline_span.since(expr_anchor),
                    );
                    let node_span = lex.span().since(expr_anchor);
                    Ok(Expr::node(
                        BinaryOperator::Concat.expr(pipe, expr),
                        self.parser,
                        node_span,
                    ))
                }
            }
        } else {
            let node_span = getline_span.since(expr_anchor);
            Ok(Expr::node(
                op.expr_getline(None, lhs),
                self.parser,
                node_span,
            ))
        }
    }

    pub fn parse_redirection(&mut self, lex: &mut Lexer<'a>) -> Result<Expr<'a>> {
        self.parse_expression(lex, BinaryOperator::Concat.binding_power().1 - 1)
    }

    /// Errors if `expr` is a typed regex.
    fn typecheck(&self, lex: &mut Lexer<'a>, expr: &Expr<'a>) -> Result<()> {
        if matches!(expr, Expr::Leaf(Atom::TypedRegex(_), _)) {
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
            Expr::Node(x, _) if matches!(x.as_ref(), ExprNode::BinaryOperation(
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
