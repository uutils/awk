// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#[cfg(unix)]
use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
use std::{
    io,
    num::NonZero,
    path::{Path, PathBuf},
};

use atoi::atoi;

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
}
