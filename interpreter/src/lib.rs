// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.
#![allow(dead_code)]

pub(crate) mod ir;
mod types;
mod vm;

pub use ir::test_interpreter;

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
enum InterpreterError {}
