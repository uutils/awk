// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{fmt::Display, path::PathBuf, rc::Rc};

use ariadne::{Color, Label, Report, ReportKind, Source};
use either::Either;
use lexer::{LexingError, Span};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum ParsingError {
    #[error(transparent)]
    LexingError(#[from] LexingError),
    #[error("Unclosed scope.")]
    UnclosedScope(Span),
    #[error("Unexpected token: {}", .1)]
    UnexpectedToken(Span, String),
    #[error("Duplicated argument `{}` to function `{}`.", .1, .2)]
    DuplicatedArgument(Span, String, String),
    #[error("Expected statement end.")]
    ExpectedStatementEnd(Span),
    #[error("Expected opening brace `{{`.")]
    ExpectedOpeningBrace(Span),
    #[error("Expected parenthesis `(`.")]
    ExpectedOpeningParenthesis(Span),
    #[error("Malformed for loop.")]
    InvalidForLoop(Span),
    #[error("All case branches must be followed by a colon `:`.")]
    ColonMustFollowCase(Span),
    #[error("There may only be one default branch in a switch statement.")]
    DuplicatedDefaultBranch(Span),
    #[error("Switch statements must start with a case or default branch.")]
    MissingSwitchBranch(Span),
    #[error("Case values may only be literal values; not variables or expressions.")]
    InvalidCaseValue(Span),
    #[error("Expected a while statement after a do block.")]
    MissingWhileAfterDo(Span),
    #[error("This statement must have its operand wrapped in parenthesis.")]
    MissingParenthesisInStatement(Span),
    #[error("Missing closing parenthesis `(` in statement operand.")]
    UnclosedParenthesisInStatement(Span),
    #[error("Missing function signature for `{}`.", .1)]
    NoFunctionSignature(Span, String),
    #[error("Missing closing parenthesis `(` in function `{}`'s signature.", .1)]
    UnclosedSignature(Span, String),
    #[error("Missing closing parenthesis `(` in expression.")]
    UnclosedParenthesisExpression(Span),
    #[error("Missing closing bracket `]` in array access.")]
    UnclosedArrayAccess(Span),
    #[error("Expected operand to be a variable.")]
    OperatorExpectsVariable(Span),
    #[error("Malformed expression: {}", .1)]
    InvalidExpression(Span, String),
    #[error("Missing alternate branch in ternary expression.")]
    MissingTernaryOr(Span),
    #[error("Missing closing parenthesis in function call.")]
    FunctionCallMissingParenthesis(Span),
    #[error("Functions calls must have their name yuxtaposed to the parenthesis `(`.")]
    FunctionCallSeparatedIdent(Span),
    #[error("Missing closing parenthesis `(` in function call to `{}`.", .1)]
    FunctionCallUnclosed(Span, String),
    #[error("Expected to be a valid identifier.")]
    ExpectedIdentifier(Span, Option<Either<Span, Span>>),
    #[error("Expected an unary operation.")]
    ExpectedUnaryOperator(Span),
    #[error("Expected a binary operation.")]
    ExpectedBinaryOperator(Span),
    #[error("Expected a placing operation.")]
    ExpectedPlaceOperator(Span),
    #[error("Typed regular expressions not accepted in this position.")]
    UnexpectedTypedRegex(Span),
    #[error("Can't call non-function, special variable `{}`.", .1)]
    SpecialVariableCall(Span, String),
    #[error("Can't use special variable `{}` for indirect function call.", .1)]
    SpecialVariableIndirectCall(Span, String),
    #[error("Can't chain non-associative operators.")]
    NonAssociativeOperator(Span),
    #[error("`break' is not allowed outside a loop or switch")]
    BreakOutsideLoopOrSwitch(Span),
    #[error("`continue' is not allowed outside a loop")]
    ContinueOutsideLoop(Span),
    #[error("`return' used outside function context")]
    ReturnOutsideFunction(Span),
    #[error("Arguments already provided in function-style call!")]
    CommandDoubleCall(Span, Span),
}

impl ParsingError {
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::LexingError(err) => match err {
                LexingError::Unknown => panic!("Unknown lexing error!"),
                LexingError::Unexpected(span, _) => Some(span.clone()),
                LexingError::UnterminatedString(span) => Some(span.clone()),
                LexingError::UnterminatedRegex(span) => Some(span.clone()),
                LexingError::UnexpectedEof => None,
                LexingError::UnavailableOnPosix(span, _) => Some(span.clone()),
                LexingError::UnavailableOnGnu(span, _) => Some(span.clone()),
            },
            Self::UnclosedScope(span) => Some(span.clone()),
            Self::UnexpectedToken(span, _) => Some(span.clone()),
            Self::DuplicatedArgument(span, _, _) => Some(span.clone()),
            Self::ExpectedStatementEnd(span) => Some(span.clone()),
            Self::ExpectedOpeningBrace(span) => Some(span.clone()),
            Self::ExpectedOpeningParenthesis(span) => Some(span.clone()),
            Self::InvalidForLoop(span) => Some(span.clone()),
            Self::ColonMustFollowCase(span) => Some(span.clone()),
            Self::DuplicatedDefaultBranch(span) => Some(span.clone()),
            Self::MissingSwitchBranch(span) => Some(span.clone()),
            Self::InvalidCaseValue(span) => Some(span.clone()),
            Self::MissingWhileAfterDo(span) => Some(span.clone()),
            Self::MissingParenthesisInStatement(span) => Some(span.clone()),
            Self::UnclosedParenthesisInStatement(span) => Some(span.clone()),
            Self::NoFunctionSignature(span, _) => Some(span.clone()),
            Self::UnclosedSignature(span, _) => Some(span.clone()),
            Self::UnclosedParenthesisExpression(span) => Some(span.clone()),
            Self::UnclosedArrayAccess(span) => Some(span.clone()),
            Self::OperatorExpectsVariable(span) => Some(span.clone()),
            Self::InvalidExpression(span, _) => Some(span.clone()),
            Self::MissingTernaryOr(span) => Some(span.clone()),
            Self::FunctionCallMissingParenthesis(span) => Some(span.clone()),
            Self::FunctionCallSeparatedIdent(span) => Some(span.clone()),
            Self::FunctionCallUnclosed(span, _) => Some(span.clone()),
            Self::ExpectedIdentifier(span, _) => Some(span.clone()),
            Self::ExpectedUnaryOperator(span) => Some(span.clone()),
            Self::ExpectedBinaryOperator(span) => Some(span.clone()),
            Self::ExpectedPlaceOperator(span) => Some(span.clone()),
            Self::UnexpectedTypedRegex(span) => Some(span.clone()),
            Self::SpecialVariableCall(span, _) => Some(span.clone()),
            Self::SpecialVariableIndirectCall(span, _) => Some(span.clone()),
            Self::NonAssociativeOperator(span) => Some(span.clone()),
            Self::BreakOutsideLoopOrSwitch(span) => Some(span.clone()),
            Self::ContinueOutsideLoop(span) => Some(span.clone()),
            Self::ReturnOutsideFunction(span) => Some(span.clone()),
            Self::CommandDoubleCall(span, _) => Some(span.clone()),
        }
    }
    fn hint(&self) -> Option<&'static str> {
        match self {
            Self::DuplicatedArgument(_, _, _) => Some("Consider giving the argument another name."),
            Self::ExpectedStatementEnd(_) => Some(
                "Valid statement ends are newlines, semicolons `;` and right brackets `}` if on a block.",
            ),
            Self::InvalidForLoop(_) => Some(
                "Valid syntaxes are `for (init; condition; end)` and `for (element in array)`.",
            ),
            Self::ColonMustFollowCase(_) => Some("Consider appending a colon like so: `case 1:`"),
            Self::InvalidCaseValue(_) => {
                Some("Consider an if statement if you need to check against an expression.")
            }
            Self::NoFunctionSignature(_, _) => {
                Some("Declare the signature as `foo()` if you require no arguments.")
            }
            Self::OperatorExpectsVariable(_) => Some(
                "This operand must modify the value of a variable. Consider alternatives like `+` or `-`.",
            ),
            Self::MissingTernaryOr(_) => Some(
                "Ternaries select between to expression based on a condition, like `bool ? foo : bar`.",
            ),
            Self::LexingError(LexingError::UnavailableOnPosix(_, _)) => {
                Some("This item is not available in POSIX-strict or traditional modes.")
            }
            Self::LexingError(LexingError::UnavailableOnGnu(_, _)) => {
                Some("This item is not available in GNU-strict mode.")
            }
            Self::UnexpectedTypedRegex(_) => Some(
                "This is only valid in some contexts, like a right-hand assignment or a function argument.",
            ),
            Self::NonAssociativeOperator(_) => Some(
                "Some operators cannot be chained because doing so could lead to logical errors. Comparison operators are one example.\n\
                Example: write `a == b && b == c` instead of `a == b == c`.",
            ),
            Self::ExpectedIdentifier(_, _) => Some(
                "Valid identifiers are sequences of ASCII letters, numbers and underscores, not \
                starting with a number.\nAdditionally, these must not match keywords (`if`, \
                `while`, etc.) and built-in functions.\n\nNote: qualified identifiers, like \
                `foo::bar`, must not have spaces around the `::`.",
            ),
            Self::CommandDoubleCall(_, _) => Some(
                "print and printf may use function-style syntax with arguments in parentheses. \
                 When this syntax is used,\nno additional arguments are allowed outside the \
                 parentheses. For example, `print(1, 2), 3` is invalid.",
            ),
            _ => None,
        }
    }
    fn secondary(&self) -> Option<(&'static str, Span, i32)> {
        match self {
            Self::ExpectedIdentifier(_, Some(span)) => Some((
                "Unexpected space.",
                span.clone().into_inner(),
                2 * span.is_left() as i32,
            )),
            Self::CommandDoubleCall(_, span) => {
                Some(("Unexpected additional arguments.", span.clone(), 0))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileCache(pub Option<Rc<PathBuf>>);

impl Display for FileCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(p) => write!(f, "{}", p.display()),
            None => f.write_str("CLI"),
        }
    }
}

pub fn report_error(
    error: ParsingError,
    name: Option<Rc<PathBuf>>,
    source: &[u8],
) -> super::AriadneErr<'_> {
    // TODO: invert the interface, so error types set the diagnostic labels.
    // TODO: use a shared ariadne instance so we can also emit warnings.
    let span = error.span().unwrap_or(source.len()..source.len());
    let source = str::from_utf8(source).unwrap();
    let mut report = Report::build(ReportKind::Error, (FileCache(name.clone()), span.clone()))
        .with_message("Syntax error")
        .with_label(
            Label::new((FileCache(name.clone()), span.clone()))
                .with_message(format!("{error}"))
                .with_color(Color::Red)
                .with_order(1),
        );
    if let Some((str, span, order)) = error.secondary() {
        report.add_label(
            Label::new((FileCache(name), span))
                .with_message(str)
                .with_color(Color::Yellow)
                .with_order(order),
        );
    }
    if let Some(str) = error.hint() {
        report.set_help(str);
    }
    (Box::new(report.finish()), Source::from(source))
}

impl<T> From<(T, Self)> for ParsingError {
    fn from(value: (T, Self)) -> Self {
        value.1
    }
}
