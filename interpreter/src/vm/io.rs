// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#[cfg(unix)]
use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
use std::{num::NonZero, path::PathBuf};

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
            s if let Some(n) = s
                .strip_prefix(b"/dev/fd/")
                .and_then(|s| str::from_utf8(s).ok()?.parse::<u32>().ok()) =>
            {
                // SAFETY: The zero variant is handled in the first arm.
                Self::Fd(unsafe { NonZero::new_unchecked(n as i32) })
            }
            s => cfg_select! {
                unix => Self::Path(OsStr::from_bytes(s).into()),
                _ => Self::Path(String::from_utf8_lossy(s).into_owned().into()),
            },
        }
    }
}
