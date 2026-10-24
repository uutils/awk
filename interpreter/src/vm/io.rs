// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#[cfg(unix)]
use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
use std::{
    io::{self, BufRead, ErrorKind},
    num::NonZero,
    ops::Range,
    path::{Path, PathBuf},
};

use atoi::atoi;

use super::ExecMode;
use crate::{Interpreter, exactly_one_char, first_char, vm::regex, vm::types::Value};

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

        self.symbols.filename = Value::new_string(name.into());
        self.symbols.errno = Value::new_int(errno);
        self.symbols.fnr = Value::new_int(0);
        self.record.clear();
        self.rs_leftover.clear();
    }

    // TODO: on Windows, we must strip `\r` depending on BINMODE.
    pub fn read_record(&mut self, reader: impl BufRead) -> io::Result<bool> {
        // Update vars
        // TODO: optimize and make more ergonomic
        self.symbols.nr = &self.symbols.fnr + &Value::new_int(1);
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
        self.ensure_rs_regex(rs)?;

        // Grow the leftover buffer until we see a usable non-empty match or EOF.
        // Empty matches are ignored: `RS = "()"` must not split between chars.
        // A match that ends at the buffer end may still grow (e.g. `RS = "\n+"`),
        // so keep reading unless we have reached EOF.
        let mut at_eof = false;
        loop {
            let range = {
                let re = &self.rs_regex_cache.as_ref().unwrap().1;
                find_nonempty_match(re, &self.rs_leftover)?
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

    /// Compiles `RS` once and reuses the automaton while the pattern is unchanged.
    fn ensure_rs_regex(&mut self, rs: &[u8]) -> io::Result<()> {
        let needs_compile = self
            .rs_regex_cache
            .as_ref()
            .is_none_or(|(pat, _)| pat.as_slice() != rs);
        if needs_compile {
            let re = regex::automaton(rs, self.mode, false)
                .map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
            self.rs_regex_cache = Some((rs.to_vec(), re));
        }
        Ok(())
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
            // Skip leading newlines on the stream.
            loop {
                let buf = reader.fill_buf()?;
                if buf.is_empty() {
                    self.symbols.rt = Value::new_str(b"");
                    return Ok(false);
                }
                if buf[0] == b'\n' {
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
                if buf[0] == b'\n' {
                    rec.pop();
                    let mut rt = Vec::from(b"\n".as_slice());
                    while {
                        let b = reader.fill_buf()?;
                        !b.is_empty() && b[0] == b'\n'
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
fn blank_line_split(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i] != b'\n' {
            i += 1;
            continue;
        }
        if i + 1 < buf.len() && buf[i + 1] == b'\n' {
            let mut j = i;
            while j < buf.len() && buf[j] == b'\n' {
                j += 1;
            }
            return Some((i, j - i));
        }
        i += 1;
    }
    None
}

fn find_delim(haystack: &[u8], delim: &[u8]) -> Option<usize> {
    match delim {
        [] => None,
        [b] => haystack.iter().position(|x| x == b),
        d => haystack.windows(d.len()).position(|w| w == d),
    }
}

fn find_nonempty_match(re: &minrx::Regex, haystack: &[u8]) -> io::Result<Option<Range<usize>>> {
    for m in re.find_iter(haystack) {
        let m = m.map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
        let range = m.range();
        if !range.is_empty() {
            return Ok(Some(range.into()));
        }
    }
    Ok(None)
}
