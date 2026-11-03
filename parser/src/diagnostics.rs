// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{
    borrow::Cow,
    error::Error,
    fmt::Display,
    io::{Write, stderr},
    path::Path,
    rc::Rc,
};

use either::Either;
use lexer::{LexingError, Span};
use memchr::{memchr, memchr_iter, memrchr};
use thiserror::Error;
use uucore::diagnostics::SourceLine;

/// How an error reads when it is not drawn against its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainForm {
    /// Echo the offending source line, with a caret under the error.
    Caret,
    /// A single `fatal:` line.
    Fatal,
}

#[derive(Debug, Clone)]
pub struct AriadneSpan(pub FileCache, pub Span);

#[derive(Debug, Default)]
pub struct DiagnosticStore {
    messages: Vec<String>,
    sources: Vec<(FileCache, Rc<[u8]>)>,
    unrecoverable: bool,
}

pub trait Diagnostic: Error {
    fn add_diagnostic(&self, store: &mut DiagnosticStore, file: FileCache, source: &[u8]) {
        let span = self.span().unwrap_or((source.len()..source.len()).into());
        store.cache(file.clone(), source);
        self.add_diagnostic_cached(store, AriadneSpan(file, span));
    }
    fn add_diagnostic_cached(&self, store: &mut DiagnosticStore, span: AriadneSpan) {
        store.unrecoverable |= self.is_unrecoverable();
        let message = store.format(&span, &self.to_string(), self.plain_form(), self.help());
        store.messages.push(message);
    }
    fn span(&self) -> Option<Span>;
    fn help(&self) -> Option<Cow<'static, str>>;
    fn is_unrecoverable(&self) -> bool;
    fn plain_form(&self) -> PlainForm;
}

impl DiagnosticStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cache(&mut self, file: FileCache, source: &[u8]) {
        // One entry per opened file, and there are very few: a vector beats a map.
        if !self.sources.iter().any(|(f, _)| *f == file) {
            self.sources.push((file, Rc::from(source)));
        }
    }

    /// `message`, located at `span`. Drawn against the offending line at a
    /// terminal or as `UUTILS_DIAG` asks; otherwise the plain lines scripts
    /// and test suites match on.
    fn format(
        &self,
        AriadneSpan(file, span): &AriadneSpan,
        message: &str,
        form: PlainForm,
        help: Option<Cow<'static, str>>,
    ) -> String {
        let program = uucore::util_name();
        let location = file.to_string();
        let message = match form {
            PlainForm::Fatal => Cow::Owned(format!("fatal: {message}")),
            PlainForm::Caret => Cow::Borrowed(message),
        };
        let Some(source) = self
            .sources
            .iter()
            .find_map(|(f, source)| (f == file).then_some(source))
        else {
            return format!("{program}: {location}: {message}\n");
        };

        let start = span.start.min(source.len());
        let line_start = memrchr(b'\n', &source[..start]).map_or(0, |i| i + 1);
        let line_end = memchr(b'\n', &source[start..]).map_or(source.len(), |i| start + i);
        let line_number = memchr_iter(b'\n', &source[..start]).count() + 1;
        let line = String::from_utf8_lossy(&source[line_start..line_end]);
        let prefix = format!("{program}: {location}:{line_number}:");

        if uucore::diagnostics::enabled() {
            // A span running past its first line is cut at the line's end.
            let end = span.end.clamp(start, line_end);
            let report = SourceLine::new(&location, line_number, &line).report(
                start - line_start..end - line_start,
                None,
                help.as_deref(),
            );
            if let Some(report) = report {
                return format!("{prefix} {message}\n{report}\n");
            }
        }

        match form {
            PlainForm::Fatal => format!("{prefix} {message}\n"),
            PlainForm::Caret => {
                // Keep tabs so the caret lines up with what the terminal shows.
                let padding: String = String::from_utf8_lossy(&source[line_start..start])
                    .chars()
                    .map(|c| if c == '\t' { '\t' } else { ' ' })
                    .collect();
                format!("{prefix} {line}\n{prefix} {padding}^ {message}\n")
            }
        }
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        let mut stderr = stderr().lock();
        for message in self.messages.drain(..) {
            stderr.write_all(message.as_bytes())?;
        }
        Ok(())
    }

    pub const fn is_unrecoverable(&self) -> bool {
        self.unrecoverable
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
    #[error("Duplicated function definition of `{}`!", .1)]
    DuplicatedFunction(Span, String),
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
    #[error("`break' is not allowed outside a loop or switch.")]
    BreakOutsideLoopOrSwitch(Span),
    #[error("`continue' is not allowed outside a loop.")]
    ContinueOutsideLoop(Span),
    #[error("`return' used outside function context.")]
    ReturnOutsideFunction(Span),
    #[error("Arguments already provided in function-style call!")]
    CommandDoubleCall(Span, Span),
    #[error("`next` is forbidden in BEGIN, END, BEGINFILE, and ENDFILE rules!")]
    NextInForbiddenCtx(Span),
    #[error("`nextfile` is forbidden in BEGIN, END, and ENDFILE rules!")]
    NextFileInForbiddenCtx(Span),
}

impl ParsingError {
    pub const fn span(&self) -> Option<Span> {
        match self {
            Self::LexingError(LexingError::Unknown | LexingError::UnexpectedEof) => None,
            &Self::LexingError(LexingError::Unexpected(span, _))
            | &Self::LexingError(LexingError::UnterminatedString(span))
            | &Self::LexingError(LexingError::UnterminatedRegex(span))
            | &Self::LexingError(LexingError::UnavailableOnPosix(span, _))
            | &Self::LexingError(LexingError::UnavailableOnGnu(span, _))
            | &Self::UnclosedScope(span)
            | &Self::UnexpectedToken(span, _)
            | &Self::DuplicatedArgument(span, _, _)
            | &Self::ExpectedStatementEnd(span)
            | &Self::ExpectedOpeningBrace(span)
            | &Self::ExpectedOpeningParenthesis(span)
            | &Self::InvalidForLoop(span)
            | &Self::ColonMustFollowCase(span)
            | &Self::DuplicatedDefaultBranch(span)
            | &Self::MissingSwitchBranch(span)
            | &Self::InvalidCaseValue(span)
            | &Self::MissingWhileAfterDo(span)
            | &Self::MissingParenthesisInStatement(span)
            | &Self::UnclosedParenthesisInStatement(span)
            | &Self::NoFunctionSignature(span, _)
            | &Self::UnclosedSignature(span, _)
            | &Self::DuplicatedFunction(span, _)
            | &Self::UnclosedParenthesisExpression(span)
            | &Self::UnclosedArrayAccess(span)
            | &Self::OperatorExpectsVariable(span)
            | &Self::InvalidExpression(span, _)
            | &Self::MissingTernaryOr(span)
            | &Self::FunctionCallMissingParenthesis(span)
            | &Self::FunctionCallSeparatedIdent(span)
            | &Self::FunctionCallUnclosed(span, _)
            | &Self::ExpectedIdentifier(span, _)
            | &Self::ExpectedUnaryOperator(span)
            | &Self::ExpectedBinaryOperator(span)
            | &Self::ExpectedPlaceOperator(span)
            | &Self::UnexpectedTypedRegex(span)
            | &Self::SpecialVariableCall(span, _)
            | &Self::SpecialVariableIndirectCall(span, _)
            | &Self::NonAssociativeOperator(span)
            | &Self::BreakOutsideLoopOrSwitch(span)
            | &Self::ContinueOutsideLoop(span)
            | &Self::ReturnOutsideFunction(span)
            | &Self::CommandDoubleCall(span, _)
            | &Self::NextInForbiddenCtx(span)
            | &Self::NextFileInForbiddenCtx(span) => Some(span),
        }
    }
    const fn hint(&self) -> Option<&'static str> {
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
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileCache(pub Option<Rc<Path>>);

impl FileCache {
    pub const fn new_file(f: Rc<Path>) -> Self {
        Self(Some(f))
    }
}

impl Display for FileCache {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.0 {
            Some(p) => write!(f, "{}", p.display()),
            None => f.write_str("cmd. line"),
        }
    }
}

impl Diagnostic for ParsingError {
    fn span(&self) -> Option<Span> {
        self.span()
    }

    fn help(&self) -> Option<Cow<'static, str>> {
        self.hint().map(Cow::Borrowed)
    }

    fn is_unrecoverable(&self) -> bool {
        true
    }

    fn plain_form(&self) -> PlainForm {
        PlainForm::Caret
    }
}

impl<T> From<(T, Self)> for ParsingError {
    fn from(value: (T, Self)) -> Self {
        value.1
    }
}
