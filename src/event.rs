// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{
    convert::Infallible,
    fs::File,
    io::{self, BufRead, BufReader, Read, Result, Write, empty, stdin, stdout},
    path::Path,
    process::exit,
};

use interpreter::{
    Bytecode, CodeRange, CtrlSig, Interpreter, Signal,
    io::{FilePath, IoRequest, IoResponse},
};
use parser::DiagnosticStore;

use crate::cli::{ArgQueueItem, KeyValue};

pub struct AwkRt<'a> {
    intrp: Interpreter<'a>,
    // Owned (not `&'a Bytecode<'a>`) so arena-invariant values like
    // `Rc<RefCell<_>>` arrays do not force a self-borrow of a local.
    bc: Bytecode<'a>,
    queue: &'a [ArgQueueItem],
    diagnostics: DiagnosticStore,
}

impl<'a> AwkRt<'a> {
    pub const fn new(
        intrp: Interpreter<'a>,
        bc: Bytecode<'a>,
        queue: &'a [ArgQueueItem],
        diagnostics: DiagnosticStore,
    ) -> Self {
        Self { intrp, bc, queue, diagnostics }
    }

    pub fn main_event_loop(&mut self) -> Result<()> {
        let res = self.begin_event_loop().and_then(|_| self.rule_event_loop());
        self.end_event_loop(0).and(res)
    }

    /// Runs `code` to completion, dispatching I/O signals from the VM.
    fn drive(&mut self, code: CodeRange) -> Result<CtrlSig> {
        let mut sig = self.intrp.run_code(&self.bc, code.clone());
        loop {
            let req = match sig {
                Ok(Signal::Suspend(req)) => req,
                Ok(Signal::Terminal(t)) => return Ok(t),
                Err(ref err) => {
                    err.emit_diagnostic(&mut self.diagnostics);
                    self.diagnostics.flush()?;
                    if self.diagnostics.is_unrecoverable() {
                        self.end_event_loop(1)?;
                        continue;
                    }
                    continue;
                }
            };
            let res = self.perform_io(&req);
            sig = self.intrp.resume(&self.bc, req, res)?;
        }
    }

    pub fn begin_event_loop(&mut self) -> Result<()> {
        match self.drive(self.bc.begin_code())? {
            CtrlSig::End => Ok(()),
            CtrlSig::Exit(code) => self.end_event_loop(code).map(|_| ()),
            CtrlSig::Next | CtrlSig::NextFile => unreachable!(),
        }
    }

    #[must_use = "Handle file skipping"]
    pub fn begin_file_event_loop(
        &mut self,
        path: Option<&Path>,
        res: Option<&io::Error>,
    ) -> Result<bool> {
        self.intrp.begin_file_prelude(path, res);
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
        let mut reader = BufReader::new(Box::new(empty()) as Box<dyn Read>);

        while let Some(item) = self.queue.split_off_first() {
            let (path, res) = match item {
                ArgQueueItem::File(path) => (
                    Some(path.as_path()),
                    File::open(path).map(|f| Box::new(f) as Box<dyn Read>),
                ),
                ArgQueueItem::Stdio => (None, Ok(Box::new(stdin().lock()) as Box<dyn Read>)),
                ArgQueueItem::Assignment(KeyValue { .. }) => {
                    // TODO assign variable
                    continue;
                }
            };
            let skip_file = self.begin_file_event_loop(path, res.as_ref().err())?;

            if skip_file {
                continue;
            }

            reader.consume(reader.buffer().len());
            *reader.get_mut() = res?; // Propagate open errors.

            match self.drive(range.clone())? {
                CtrlSig::End | CtrlSig::NextFile => {} // clean-up & continue.
                CtrlSig::Next => todo!(),
                CtrlSig::Exit(code) => return self.end_event_loop(code).map(|_| ()),
            }
            // TODO: read next record; if EOF execute endfile and continue 'file.
        }
        Ok(())
    }

    fn perform_io(&mut self, req: &IoRequest) -> Result<IoResponse> {
        match req {
            IoRequest::FileWrite { buf, at: FilePath::Stdout } => {
                stdout().lock().write_all(buf).map(|()| IoResponse::Empty)
            }
            _ => todo!(),
        }
    }
}
