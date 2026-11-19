// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{
    convert::Infallible,
    fs::File,
    io::{self, BufRead, BufReader, Read, Result, StdinLock, Write, stdin, stdout},
    path::Path,
    process::exit,
};

use interpreter::{
    Bytecode, CodeRange, CtrlSig, Interpreter, Signal,
    io::{FilePath, IoRequest, IoResponse},
};
use parser::DiagnosticStore;

use crate::{
    cli::{ArgQueueItem, KeyValue},
    utils::AWK_FATAL_CODE,
};

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
        // Only fatal errors are `?` up the call stack. These include I/O errors
        // the VM does not handle, as well as runtime errors like div by zero.
        self.begin_event_loop()?;
        self.rule_event_loop()?;
        self.end_event_loop(0)?
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
                        self.end_event_loop(AWK_FATAL_CODE)?;
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

    /// Called by [`Self::rule_event_loop`] each time a file or stdin is processed. This is
    /// important since the VM must set some variables and handle I/O errors logic.
    #[must_use = "Handle file skipping"]
    pub fn begin_file_event_loop(
        &mut self,
        path: Option<&Path>,
        res: Option<&io::Error>,
    ) -> Result<bool> {
        let code = self.bc.begin_file_code();
        let opened = self.intrp.begin_file_prelude(path, res, !code.is_empty());
        match self.drive(code)? {
            // Like gawk, BEGINFILE may skip a file that didn't open; if it doesn't, that's fatal.
            CtrlSig::End if let Err(e) = opened => Err(e.kind().into()),
            CtrlSig::End => Ok(false),
            CtrlSig::NextFile => Ok(true),
            CtrlSig::Exit(code) => self.end_event_loop(code).map(|_| false),
            CtrlSig::Next => unreachable!(),
        }
    }

    pub fn end_file_event_loop(&mut self) -> Result<()> {
        match self.drive(self.bc.end_file_code())? {
            CtrlSig::End => Ok(()),
            CtrlSig::Exit(code) => self.end_event_loop(code).map(|_| ()),
            CtrlSig::Next | CtrlSig::NextFile => unreachable!(),
        }
    }

    pub fn end_event_loop(&mut self, code: i32) -> Result<Infallible> {
        match self.drive(self.bc.end_code())? {
            CtrlSig::Exit(code) => exit(code),
            CtrlSig::End => exit(code),
            CtrlSig::Next | CtrlSig::NextFile => unreachable!(),
        }
    }

    /// Executes the user's regular rules. Will consume the arguments queue and dispatch as needed.
    pub fn rule_event_loop(&mut self) -> Result<()> {
        let range = self.bc.rules_code();
        // Input is read unless the program has only BEGIN rules
        if range.is_empty()
            && self.bc.end_code().is_empty()
            && self.bc.begin_file_code().is_empty()
            && self.bc.end_file_code().is_empty()
        {
            return Ok(());
        }

        let mut reader = BufReader::new(ReadSource::None);

        while let Some(item) = self.queue.split_off_first() {
            let (path, res) = match item {
                ArgQueueItem::File(path) => {
                    (Some(path.as_path()), File::open(path).map(ReadSource::File))
                }
                ArgQueueItem::Stdio => (None, Ok(ReadSource::Stdin(stdin().lock()))),
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

            // File event loop: read record, wrap rule event loop, endfile.
            while self.intrp.read_record(&mut reader)? {
                match self.drive(range.clone())? {
                    CtrlSig::End | CtrlSig::Next => {} // read next record & run rules
                    CtrlSig::NextFile => break,        // clean-up & read next file
                    CtrlSig::Exit(code) => return self.end_event_loop(code).map(|_| ()),
                }
            }
            self.end_file_event_loop()?;
            *reader.get_mut() = ReadSource::None; // close file before opening next one
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

enum ReadSource<'a> {
    Stdin(StdinLock<'a>),
    File(File),
    None,
}

impl Read for ReadSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self {
            Self::File(f) => f.read(buf),
            Self::Stdin(f) => f.read(buf),
            Self::None => Ok(0),
        }
    }
}
