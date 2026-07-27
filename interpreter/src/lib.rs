// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.
#![allow(dead_code)]

pub(crate) mod ir;
mod vm;

use ariadne::{Color, Label, ReportBuilder};
pub use ir::{
    Instruction,
    lower::{Bytecode, CodeGen},
};
use parser::{AriadneSpan, Diagnostic, DiagnosticStore, Span};
pub use vm::{CodeRange, CtrlSig, ExecMode, Interpreter, IoRequest, IoResponse, Signal};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum InterpreterError {
    #[error("Call to an undefined function!")]
    UnknownFunction(AriadneSpan),
    #[error("Call with too many arguments!")]
    ArityMismatch(AriadneSpan, u8, u8),
    #[error("Maximum recursion/stack depth reached here!")]
    RecursionDepth(AriadneSpan),
    #[error("Indirect call to an undefined function!")]
    UnknownIndFunction(AriadneSpan, String),
}

impl InterpreterError {
    pub fn emit_diagnostic(&self, store: &mut DiagnosticStore) {
        match self {
            Self::UnknownIndFunction(span, _)
            | Self::RecursionDepth(span)
            | Self::ArityMismatch(span, _, _)
            | Self::UnknownFunction(span) => self.add_diagnostic_cached(store, span.clone()),
        }
    }
}

impl Diagnostic for InterpreterError {
    fn message(&self) -> &'static str {
        "Execution error"
    }
    fn span(&self) -> Option<Span> {
        match self {
            Self::UnknownIndFunction((_, span), _)
            | Self::RecursionDepth((_, span))
            | Self::ArityMismatch((_, span), _, _)
            | Self::UnknownFunction((_, span)) => Some(span.clone()),
        }
    }
    fn add_labels(&self, span: AriadneSpan, report: &mut ReportBuilder<AriadneSpan>) {
        report.add_label(
            Label::new(span)
                .with_message(self.to_string())
                .with_color(Color::Red)
                .with_order(1),
        );
    }
    fn add_help(&self, report: &mut ReportBuilder<AriadneSpan>) {
        let note = match self {
            Self::UnknownFunction(_) => {
                "This code called an undefined function. Although functions are statically \
                defined,\nthis error is emitted at runtime and may only occur on rarely executed \
                code paths."
            }
            &Self::ArityMismatch(_, expected, given) => &format!(
                "This function accepts up to {expected} arguments, but {given} were provided."
            ),
            Self::RecursionDepth(_) => {
                "The maximum stack depth is 4096. If you find this too limiting, please open an \
                issue so we can help!\nCurrently, GNU AWK does not provide an user-configurable \
                limit, but we are considering supporting this."
            }
            Self::UnknownIndFunction(_, name) => {
                &format!("This code tried to call the unknown function `{name}` indirectly.")
            }
        };
        report.set_help(note);
    }
    fn is_unrecoverable(&self) -> bool {
        true
    }
}
