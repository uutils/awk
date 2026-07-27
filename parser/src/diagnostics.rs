// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{borrow::Cow, error::Error, fmt::Display, path::Path, rc::Rc};

use ariadne::{Color, Label, Report, ReportBuilder, ReportKind, Source};
use either::Either;
use lexer::{LexingError, Span};
use thiserror::Error;

pub type AriadneSpan = (FileCache, Span);

#[derive(Debug, Default)]
pub struct DiagnosticStore<'a> {
    storage: Vec<Report<'a, AriadneSpan>>,
    cache: Cache<'a>,
    unrecoverable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerSource<'a>(Rc<Cow<'a, str>>);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Cache<'a>(Vec<(FileCache, Source<InnerSource<'a>>)>);

pub trait Diagnostic: Error {
    fn add_diagnostic<'a>(
        &self,
        store: &mut DiagnosticStore<'a>,
        file: FileCache,
        source: &'a [u8],
    ) {
        let span = self.span().unwrap_or(source.len()..source.len());
        let mut report = Report::build(ReportKind::Error, (file.clone(), span.clone()))
            .with_message(self.message());

        self.add_labels((file.clone(), span), &mut report);
        self.add_help(&mut report);

        store.unrecoverable |= self.is_unrecoverable();
        store.push(report.finish(), file, source);
    }
    fn span(&self) -> Option<Span>;
    fn message(&self) -> &'static str;
    fn add_labels(&self, span: AriadneSpan, report: &mut ReportBuilder<AriadneSpan>);
    fn add_help(&self, report: &mut ReportBuilder<AriadneSpan>);
    fn is_unrecoverable(&self) -> bool;
}

impl<'a> DiagnosticStore<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(
        &mut self,
        report: Report<'a, (FileCache, Span)>,
        file: FileCache,
        source: &'a [u8],
    ) {
        // Cache UTF-8 validated file source. Given we have one for each opened
        // file _with_ generated diagnostics, it's much better to use a vector.
        if !self.cache.0.iter().any(|(f, _)| *f == file) {
            let cached = Source::from(InnerSource(Rc::new(String::from_utf8_lossy(source))));
            self.cache.0.push((file, cached));
        }

        self.storage.push(report);
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        for diag in &self.storage {
            diag.eprint(&mut self.cache)?;
        }
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Report<'a, AriadneSpan>> {
        self.storage.iter()
    }
}

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
    #[error("Function calls must not have a space between the name and the parenthesis `(`.")]
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
                "Ternaries select between two expressions based on a condition, like `bool ? foo : bar`.",
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
pub struct FileCache(pub Option<Rc<Path>>);

impl Display for FileCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(p) => write!(f, "{}", p.display()),
            None => f.write_str("CLI"),
        }
    }
}

impl Diagnostic for ParsingError {
    fn span(&self) -> Option<Span> {
        self.span()
    }

    fn message(&self) -> &'static str {
        "Syntax error"
    }

    fn add_labels(&self, (file, span): AriadneSpan, report: &mut ReportBuilder<AriadneSpan>) {
        report.add_label(
            Label::new((file.clone(), span))
                .with_message(self.to_string())
                .with_color(Color::Red)
                .with_order(1),
        );

        if let Some((str, span, order)) = self.secondary() {
            report.add_label(
                Label::new((file, span))
                    .with_message(str)
                    .with_color(Color::Yellow)
                    .with_order(order),
            );
        }
    }

    fn add_help(&self, report: &mut ReportBuilder<AriadneSpan>) {
        if let Some(str) = self.hint() {
            report.set_help(str);
        }
    }

    fn is_unrecoverable(&self) -> bool {
        true
    }
}

impl<'a> ariadne::Cache<FileCache> for Cache<'a> {
    type Storage = InnerSource<'a>;

    fn fetch(&mut self, id: &FileCache) -> Result<&Source<Self::Storage>, impl std::fmt::Debug> {
        match self.0.iter().find_map(|(f, c)| (f == id).then_some(c)) {
            Some(x) => Ok(x),
            None => Err("Internal error while preparing diagnostics!"),
        }
    }

    fn display<'b>(&self, id: &'b FileCache) -> Option<impl Display + 'b> {
        Some(id)
    }
}

impl AsRef<str> for InnerSource<'_> {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl<T> From<(T, Self)> for ParsingError {
    fn from(value: (T, Self)) -> Self {
        value.1
    }
}
