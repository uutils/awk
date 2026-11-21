// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! Dump the group database in `/etc/group` format.
//!
//! Iterates the system group database with `getgrent()`/`endgrent()` so NSS
//! backends beyond local files (LDAP, SSSD, systemd-sysusers, …) are included.
//! Intended for awk library helpers that need a portable group dump.
//!
//! Platforms without a group database get an empty dump and a successful exit.
//!
//! `nix`/`rustix` do not expose `getgrent` iteration (only lookup by name/gid),
//! so this helper uses a small `libc` FFI surface.

#[cfg(unix)]
use std::io::{self, Write};
use std::process::ExitCode;

#[cfg(unix)]
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(err) => {
            let _ = writeln!(io::stderr(), "grcat: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    ExitCode::SUCCESS
}

#[cfg(unix)]
fn run() -> io::Result<()> {
    struct EndGrent;

    impl Drop for EndGrent {
        fn drop(&mut self) {
            // SAFETY: pairs with `setgrent` below; `grcat` is single-threaded.
            unsafe {
                libc::endgrent();
            }
        }
    }

    let mut out = io::stdout().lock();

    // SAFETY: `getgrent`/`setgrent`/`endgrent` share process-global state and are
    // not thread-safe. `grcat` is a single-threaded helper, so exclusive use is OK.
    unsafe {
        libc::setgrent();
    }
    let _end = EndGrent;

    loop {
        // SAFETY: see note above; pointer is only used while non-null and before
        // the next `getgrent`/`endgrent` call.
        let group = unsafe { libc::getgrent() };
        if group.is_null() {
            break;
        }
        // SAFETY: `getgrent` returned a non-null pointer to a valid `group`.
        let group = unsafe { &*group };

        write_field(&mut out, group.gr_name)?;
        out.write_all(b":")?;
        write_field_or_star(&mut out, group.gr_passwd)?;
        write!(out, ":{}:", group.gr_gid)?;

        if !group.gr_mem.is_null() {
            let mut i = 0usize;
            loop {
                // SAFETY: `gr_mem` is a NULL-terminated array of C string pointers.
                let member = unsafe { *group.gr_mem.add(i) };
                if member.is_null() {
                    break;
                }
                if i > 0 {
                    out.write_all(b",")?;
                }
                write_field(&mut out, member)?;
                i += 1;
            }
        }
        out.write_all(b"\n")?;
    }

    Ok(())
}

#[cfg(unix)]
fn write_field(out: &mut impl Write, ptr: *const libc::c_char) -> io::Result<()> {
    if ptr.is_null() {
        return Ok(());
    }
    // SAFETY: caller guarantees a NUL-terminated C string when non-null.
    let bytes = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes();
    out.write_all(bytes)
}

#[cfg(unix)]
fn write_field_or_star(out: &mut impl Write, ptr: *const libc::c_char) -> io::Result<()> {
    if ptr.is_null() {
        return out.write_all(b"*");
    }
    write_field(out, ptr)
}
