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
}

impl InterpreterError {
    pub fn emit_diagnostic(&self, store: &mut DiagnosticStore) {
        match self {
            Self::UnknownFunction(span) => self.add_diagnostic_cached(store, span.clone()),
        }
    }
}

impl Diagnostic for InterpreterError {
    fn message(&self) -> &'static str {
        "Execution error"
    }
    fn span(&self) -> Option<Span> {
        match self {
            Self::UnknownFunction((_, span)) => Some(span.clone()),
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
                "This code called a function that wasn't defined. Note \
            that, despite functions being statically defined,\nthis is emitted at runtime, \
            so the error may be seldomly generated if the code path is hard to reach."
            }
        };
        report.set_help(note);
    }
    fn is_unrecoverable(&self) -> bool {
        true
    }
}
