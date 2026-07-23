// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.
#![allow(dead_code)]

pub(crate) mod ir;
mod vm;

pub use ir::{
    Instruction,
    lower::{Bytecode, CodeGen},
};
pub use vm::{CodeRange, CtrlSig, ExecMode, Interpreter, IoRequest, IoResponse, Signal};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
enum InterpreterError {}
