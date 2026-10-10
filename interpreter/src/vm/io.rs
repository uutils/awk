// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#[cfg(unix)]
use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
use std::{
    io::{self, BufRead, ErrorKind},
    num::NonZero,
    path::{Path, PathBuf},
};

use atoi::atoi;
use memchr::memchr;

use super::ExecMode;
use crate::{Interpreter, exactly_one_char, first_char, vm::types::Value};

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
    /// sets the built-in variables about the new file and handles errors from the I/O layer
    /// appropriately. Note that the VM is sans-io, so such logic lives separated here.
    ///
    /// Gives `res` back when the open error is fatal.
    pub fn begin_file_prelude<'e>(
        &mut self,
        f: Option<&Path>,
        res: Option<&'e io::Error>,
        has_begin_file: bool,
    ) -> Result<(), &'e io::Error> {
        // If path is none, this is stdin (filename `-`).
        let name = f.map_or(b"-".as_slice(), |p| p.as_os_str().as_encoded_bytes());
        let errno = res.map_or(0, |e| e.raw_os_error().unwrap_or(-1) as _);

        self.symbols.filename = Value::new_string(name.into());
        self.symbols.errno = Value::new_int(errno);
        self.symbols.fnr = Value::new_int(0);
        // Like gawk, BEGINFILE never sees the previous file's last record.
        // Without BEGINFILE, $0 survives an empty last file for END.
        if has_begin_file {
            self.record.clear();
        }

        // Leftover input must not leak across files.
        self.rs_leftover.clear();

        // TODO: an open error isn't fatal when `PROCINFO` marks input as non-fatal.
        res.map_or(Ok(()), Err)
    }

    // TODO: on Windows, we must strip `\r` depending on BINMODE.
    pub fn read_record(&mut self, mut reader: impl BufRead) -> io::Result<bool> {
        // At EOF, keep NR, FNR and $0 of the last record for END.
        // Leftover bytes from a prior regexp `RS` still form a record.
        if self.rs_leftover.is_empty() && reader.fill_buf()?.is_empty() {
            return Ok(false);
        }

        // Update vars
        // TODO: optimize and make more ergonomic
        self.symbols.nr = &self.symbols.nr + &Value::new_int(1);
        self.symbols.fnr = &self.symbols.fnr + &Value::new_int(1);

        let rs = self.symbols.rs.clone();
        let rs_bytes = &*rs.to_bytes();

        if rs_bytes.is_empty() {
            // Blank-line separated records (`RS = ""`).
            self.read_record_blank_lines(reader)
        } else if self.mode == ExecMode::Posix {
            // POSIX: never a regexp — only the first character (or first byte).
            if let Some(c) = first_char(rs_bytes) {
                self.read_record_until_char(c, reader)
            } else {
                self.read_record_until_byte(rs_bytes[0], reader)
            }
        } else if let Some(c) = exactly_one_char(rs_bytes) {
            self.read_record_until_char(c, reader)
        } else {
            // Multi-character (or non-UTF-8) `RS` is a regexp (gawk extension).
            self.read_record_regex(rs_bytes, reader)
        }
    }

    pub fn read_record_regex(&mut self, rs: &[u8], mut reader: impl BufRead) -> io::Result<bool> {
        self.ensure_rs_regex(rs);

        // Grow the leftover buffer until we see a usable non-empty match or EOF.
        // Empty matches are ignored: `RS = "()"` must not split between chars.
        // A match that ends at the buffer end may still grow (e.g. `RS = "\n+"`),
        // so keep reading unless we have reached EOF.
        let mut at_eof = false;
        loop {
            let range = {
                let re = self.rs_regex_cache.as_ref().unwrap();
                re.find_nonempty_match(&self.rs_leftover, self.mode)
                    .map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?
            };
            if let Some(range) = range {
                let touches_end = range.end == self.rs_leftover.len();
                if !touches_end || at_eof {
                    let rec = self.record.write_new();
                    rec.extend_from_slice(&self.rs_leftover[..range.start]);
                    self.symbols.rt =
                        Value::new_string(self.rs_leftover[range.start..range.end].into());
                    self.rs_leftover.drain(..range.end);
                    return Ok(true);
                }
                // Match reaches the end of the buffered input; try to extend it.
            }

            if at_eof {
                if self.rs_leftover.is_empty() {
                    self.symbols.rt = Value::new_str(b"");
                    return Ok(false);
                }
                let rec = self.record.write_new();
                rec.append(&mut self.rs_leftover);
                self.symbols.rt = Value::new_str(b"");
                return Ok(true);
            }

            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                at_eof = true;
                continue;
            }
            let n = buf.len();
            self.rs_leftover.extend_from_slice(buf);
            reader.consume(n);
        }
    }

    /// Caches a regex `Value` for `RS` while the pattern is unchanged.
    ///
    /// gawk keeps `typeof(RS)` as `"string"` after regexp splitting, so we do
    /// not type-taint `symbols.rs`; the automaton lives in this cache instead.
    fn ensure_rs_regex(&mut self, rs: &[u8]) {
        let needs_compile = self
            .rs_regex_cache
            .as_ref()
            .is_none_or(|v| v.as_str() != Some(rs));
        if needs_compile {
            self.rs_regex_cache = Some(Value::new_regex(rs.into()));
        }
    }

    pub fn read_record_until_char(
        &mut self,
        c: char,
        mut reader: impl BufRead,
    ) -> io::Result<bool> {
        let mut bytes = [0; 4];
        let bytes = c.encode_utf8(&mut bytes).as_bytes();
        self.read_record_until_delim(bytes, &mut reader)
    }

    fn read_record_until_byte(&mut self, byte: u8, mut reader: impl BufRead) -> io::Result<bool> {
        self.read_record_until_delim(&[byte], &mut reader)
    }

    /// Character / byte `RS`, consuming any `rs_leftover` from a previous regexp
    /// `RS` before reading the underlying stream.
    fn read_record_until_delim(
        &mut self,
        delim: &[u8],
        reader: &mut impl BufRead,
    ) -> io::Result<bool> {
        // Drain leftover from a prior regexp RS first so mid-file RS changes
        // do not drop already-buffered input.
        if let Some(pos) = find_delim(&self.rs_leftover, delim) {
            let rec = self.record.write_new();
            rec.extend_from_slice(&self.rs_leftover[..pos]);
            self.symbols.rt = Value::new_string(delim.into());
            self.rs_leftover.drain(..pos + delim.len());
            return Ok(true);
        }

        // It is correct behavior that we terminate the record on EOF too,
        // even if the text file is malformed (no trailing newline).
        let (read_ok, found_delim) = {
            let rec = self.record.write_new();
            rec.append(&mut self.rs_leftover);
            match *delim {
                [byte] => {
                    // fast path: single search
                    let read = reader.read_until(byte, rec)?;
                    let found_delim = rec.pop_if(|&mut last| last == byte).is_some();
                    (read > 0 || !rec.is_empty() || found_delim, found_delim)
                }
                [.., last] => {
                    // slow path: loop searching multiple bytes or EOF.
                    let mut found = false;
                    let mut read_any = false;
                    while reader.read_until(last, rec)? > 0 {
                        read_any = true;
                        if rec.ends_with(delim) {
                            rec.truncate(rec.len() - delim.len());
                            found = true;
                            break;
                        }
                    }
                    (found || read_any || !rec.is_empty(), found)
                }
                // This would be a bug in the std. It is often optimized away.
                _ => unreachable!(),
            }
        };
        self.symbols.rt = if found_delim {
            Value::new_string(delim.into())
        } else {
            Value::new_str(b"")
        };
        Ok(read_ok)
    }

    /// Blank-line record separation for `RS = ""`.
    ///
    /// Leading newlines are skipped. A record ends at the first blank line; the
    /// run of separating newlines becomes `RT`. A final newline at EOF (without
    /// a following blank line) is stripped from the record and stored in `RT`.
    ///
    /// Any `rs_leftover` from a previous regexp `RS` is consumed first.
    pub fn read_record_blank_lines(&mut self, mut reader: impl BufRead) -> io::Result<bool> {
        // Merge leftover into a working buffer so mid-file RS changes keep bytes.
        let mut pending = std::mem::take(&mut self.rs_leftover);
        let start = pending
            .iter()
            .position(|&b| b != b'\n')
            .unwrap_or(pending.len());
        pending.drain(..start);

        if pending.is_empty() {
            // Skip leading newlines on the stream (byte `\n`, not a UTF-8 char).
            loop {
                let buf = reader.fill_buf()?;
                if buf.is_empty() {
                    self.symbols.rt = Value::new_str(b"");
                    return Ok(false);
                }
                if let [b'\n', ..] = buf {
                    reader.consume(1);
                    continue;
                }
                break;
            }
        }

        let (rt, tail) = {
            let rec = self.record.write_new();
            rec.append(&mut pending);

            loop {
                if let Some((content_len, rt_len)) = blank_line_split(rec) {
                    let rt = rec[content_len..content_len + rt_len].to_vec();
                    let tail = rec.split_off(content_len + rt_len);
                    rec.truncate(content_len);
                    break (rt, tail);
                }

                let n = reader.read_until(b'\n', rec)?;
                if n == 0 || !rec.ends_with(b"\n") {
                    break (Vec::new(), Vec::new());
                }

                let buf = reader.fill_buf()?;
                if buf.is_empty() {
                    rec.pop();
                    break (Vec::from(b"\n".as_slice()), Vec::new());
                }
                if let [b'\n', ..] = buf {
                    rec.pop();
                    let mut rt = Vec::from(b"\n".as_slice());
                    while {
                        let b = reader.fill_buf()?;
                        matches!(b.first(), Some(&b'\n'))
                    } {
                        rt.push(b'\n');
                        reader.consume(1);
                    }
                    break (rt, Vec::new());
                }
            }
        };

        self.rs_leftover = tail;
        self.symbols.rt = Value::new_string(rt.as_slice().into());
        Ok(true)
    }
}

/// Finds a blank-line boundary in `buf`.
///
/// Returns `(content_len, rt_len)` where `buf[..content_len]` is the record and
/// `buf[content_len..content_len + rt_len]` is the run of separator newlines.
/// Boundaries are newline *bytes* (`0x0A`), matching awk's byte-oriented records.
fn blank_line_split(buf: &[u8]) -> Option<(usize, usize)> {
    for (i, &b) in buf.iter().enumerate() {
        if b != b'\n' || buf.get(i + 1) != Some(&b'\n') {
            continue;
        }
        let rt_len = buf[i..].iter().take_while(|&&c| c == b'\n').count();
        return Some((i, rt_len));
    }
    None
}

/// Byte offset of the first occurrence of `delim` in `haystack`.
fn find_delim(haystack: &[u8], delim: &[u8]) -> Option<usize> {
    match *delim {
        [] => None,
        [b] => memchr(b, haystack),
        [a, b] => {
            let mut i = 0;
            while let Some(pos) = memchr(a, &haystack[i..]) {
                let idx = i + pos;
                if haystack.get(idx + 1) == Some(&b) {
                    return Some(idx);
                }
                i = idx + 1;
            }
            None
        }
        [a, b, c] => {
            let mut i = 0;
            while let Some(pos) = memchr(a, &haystack[i..]) {
                let idx = i + pos;
                if haystack.get(idx + 1) == Some(&b) && haystack.get(idx + 2) == Some(&c) {
                    return Some(idx);
                }
                i = idx + 1;
            }
            None
        }
        ref d => {
            let first = d[0];
            let mut i = 0;
            while let Some(pos) = memchr(first, &haystack[i..]) {
                let idx = i + pos;
                if haystack.get(idx..idx + d.len()) == Some(d) {
                    return Some(idx);
                }
                i = idx + 1;
            }
            None
        }
    }
}
