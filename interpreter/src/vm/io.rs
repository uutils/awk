// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#[cfg(unix)]
use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
use std::{
    io::{self, BufRead, Result},
    num::NonZero,
    path::{Path, PathBuf},
};

use atoi::atoi;

use super::ExecMode;
use crate::{Interpreter, vm::types::Value};

#[derive(Debug)]
pub enum FilePath {
    Stdin,
    Stdout,
    Stderr,
    Fd(NonZero<i32>),
    Path(PathBuf),
}

#[derive(Debug)]
pub enum IoRequest {
    FileRead { buf: Vec<u8>, at: FilePath },
    FileWrite { buf: Vec<u8>, at: FilePath },
}

#[derive(Debug)]
pub enum IoResponse {
    Empty,
}

impl From<&[u8]> for FilePath {
    fn from(value: &[u8]) -> Self {
        match value {
            b"/dev/stdin" | b"/dev/fd/0" => Self::Stdin,
            b"/dev/stdout" | b"/dev/fd/1" => Self::Stdout,
            b"/dev/stderr" | b"/dev/fd/2" => Self::Stderr,
            s if let Some(n) = s.strip_prefix(b"/dev/fd/").and_then(atoi::<i32>)
                && n.is_positive() =>
            {
                // SAFETY: The zero variant is handled in the first arm *and* in
                // the `is_positive` check.
                Self::Fd(unsafe { NonZero::new_unchecked(n) })
            }
            s => cfg_select! {
                unix => Self::Path(OsStr::from_bytes(s).into()),
                _ => Self::Path(String::from_utf8_lossy(s).into_owned().into()),
            },
        }
    }
}

impl Interpreter<'_> {
    pub fn begin_file_prelude(&mut self, f: Option<&Path>, res: Option<&io::Error>) {
        let name = f.map_or(b"-".as_slice(), |p| p.as_os_str().as_encoded_bytes());
        let errno = res.map_or(0, |e| e.raw_os_error().unwrap_or(-1) as _);

        self.symbols.filename = Value::String(name.to_vec().into());
        self.symbols.errno = Value::Int(errno);
        self.symbols.fnr = Value::Int(0);
        self.record.clear();
    }

    // TODO: consider caching the result of all this validation, so we can
    // directly dispatch to the correct functions and avoid extra writes to RT.
    // TODO: on Windows, we must strip `\r` depending on BINMODE.
    pub fn read_record(&mut self, reader: impl BufRead) -> Result<bool> {
        // Update vars
        // TODO: optimize and make more ergonomic
        self.symbols.nr = &self.symbols.fnr + &Value::Int(1);
        self.symbols.fnr = &self.symbols.fnr + &Value::Int(1);

        // TODO: cache string repr across all values, raw byte sequences.
        let rs = self.symbols.rs.to_string();
        match self.mode {
            // Regex matching (GNU extension)
            ExecMode::Uu | ExecMode::Gnu if rs.chars().count() > 1 => {
                self.read_record_regex(rs.as_bytes(), reader)
            }
            // Single char matching
            _ if let Some(c) = rs.chars().next() => {
                self.symbols.rt = Value::String(rs.into_bytes().into());
                self.read_record_until_char(c, reader)
            }
            // Empty RS
            _ => self.read_record_blank_lines(reader),
        }
    }

    pub fn read_record_regex(&mut self, _rs: &[u8], mut _reader: impl BufRead) -> Result<bool> {
        todo!()
    }

    pub fn read_record_until_char(&mut self, c: char, mut reader: impl BufRead) -> Result<bool> {
        let mut bytes = [0; 4];
        let bytes = c.encode_utf8(&mut bytes).as_bytes();
        let rec = self.record.write_new();

        // It is correct behavior that we terminate the record on EOF too,
        // even if the text file is malformed (no trailing newline).
        if let [byte] = *bytes {
            // fast path: single search
            reader.read_until(byte, rec).map(|n| n > 0)
        } else {
            // slow path: loop searching multiple bytes or EOF.
            let last = bytes[bytes.len() - 1];

            while reader.read_until(last, rec)? > 0 && !rec.ends_with(bytes) {}
            Ok(!rec.is_empty())
        }
    }

    pub fn read_record_blank_lines(&mut self, mut _reader: impl BufRead) -> Result<bool> {
        todo!()
    }
}
