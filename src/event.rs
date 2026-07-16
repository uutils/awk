// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{
    convert::Infallible,
    io::{Result, Write, stdout},
    path::Path,
    process::exit,
};

use interpreter::{Bytecode, CodeRange, CtrlSig, Interpreter, IoRequest, IoResponse, Signal};

use crate::cli::{ArgQueueItem, KeyValue};

pub struct AwkRt<'a> {
    intrp: Interpreter<'a>,
    // Owned (not `&'a Bytecode<'a>`) so arena-invariant values like
    // `Rc<RefCell<_>>` arrays do not force a self-borrow of a local.
    bc: Bytecode<'a>,
    queue: &'a [ArgQueueItem],
}

impl<'a> AwkRt<'a> {
    pub fn new(intrp: Interpreter<'a>, bc: Bytecode<'a>, queue: &'a [ArgQueueItem]) -> Self {
        Self { intrp, bc, queue }
    }

    pub fn main_event_loop(&mut self) -> Result<()> {
        let res = self.begin_event_loop().and_then(|_| self.rule_event_loop());
        self.end_event_loop(0).and(res)
    }

    /// Runs `code` to completion, dispatching I/O signals from the VM.
    fn drive(&mut self, code: CodeRange) -> Result<CtrlSig> {
        let mut sig = self.intrp.run_code(&self.bc, code.clone())?;
        loop {
            let req = match sig {
                Signal::Suspend(req) => req,
                Signal::Terminal(t) => return Ok(t),
            };
            let res = self.perform_io(&req);
            sig = self.intrp.resume(&self.bc, code.clone(), req, res)?;
        }
    }

    pub fn begin_event_loop(&mut self) -> Result<()> {
        match self.drive(self.bc.begin_code())? {
            CtrlSig::End => Ok(()),
            CtrlSig::Exit(code) => self.end_event_loop(code).map(|_| ()),
            CtrlSig::Next | CtrlSig::NextFile => unreachable!(),
        }
    }

    pub fn begin_file_event_loop(&mut self, _path: Option<&Path>) -> Result<bool> {
        // TODO: read path and set ERRNO
        match self.drive(self.bc.begin_file_code())? {
            CtrlSig::End => Ok(false),
            CtrlSig::NextFile => Ok(true),
            CtrlSig::Exit(code) => self.end_event_loop(code).map(|_| false),
            CtrlSig::Next => unreachable!(),
        }
    }

    pub fn end_event_loop(&mut self, code: i32) -> Result<Infallible> {
        match self.drive(self.bc.end_code())? {
            CtrlSig::Exit(code) => exit(code),
            CtrlSig::End => exit(code),
            CtrlSig::Next | CtrlSig::NextFile => unreachable!(),
        }
    }

    pub fn rule_event_loop(&mut self) -> Result<()> {
        let range = self.bc.rules_code();

        while let Some(item) = self.queue.split_off_first() {
            match item {
                ArgQueueItem::File(f) => {
                    self.begin_file_event_loop(Some(f))?;
                }
                ArgQueueItem::Stdio => {
                    self.begin_file_event_loop(None)?;
                }
                ArgQueueItem::Assignment(KeyValue { .. }) => {
                    // TODO assign variable
                    continue;
                }
            }

            match self.drive(range.clone())? {
                CtrlSig::End | CtrlSig::NextFile => {} // continues
                CtrlSig::Next => todo!(),
                CtrlSig::Exit(code) => return self.end_event_loop(code).map(|_| ()),
            }
            // TODO: read next record; if EOF execute endfile and continue 'file.
        }
        Ok(())
    }

    fn perform_io(&mut self, req: &IoRequest) -> Result<IoResponse> {
        match req {
            IoRequest::WriteStdout(buf) => {
                stdout().lock().write_all(buf).map(|_| IoResponse::Empty)
            }
        }
    }
}
