// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! Dump the password database in `/etc/passwd` format for gawk library routines.
//!
//! Behavior matches the `pwcat` helper described in the GNU Awk User's Guide:
//! <https://www.gnu.org/software/gawk/manual/html_node/Passwd-Functions.html>
//!
//! Uses `getpwent()`/`endpwent()` so NSS sources beyond local files (LDAP, SSSD,
//! systemd-userdb, etc.) are included, as required by routines such as
//! `passwd.awk`.
//!
//! Platforms without a password database get an empty dump and a successful exit.

#[cfg(unix)]
use std::io::{self, Write};
use std::process::ExitCode;

#[cfg(unix)]
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(err) => {
            let _ = writeln!(io::stderr(), "pwcat: {err}");
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
    struct EndPwent;

    impl Drop for EndPwent {
        fn drop(&mut self) {
            // SAFETY: pairs with `setpwent` below; `pwcat` is single-threaded.
            unsafe {
                libc::endpwent();
            }
        }
    }

    let mut out = io::stdout().lock();

    // SAFETY: `getpwent`/`setpwent`/`endpwent` share process-global state and are
    // not thread-safe. `pwcat` is a single-threaded helper, so exclusive use is OK.
    unsafe {
        libc::setpwent();
    }
    let _end = EndPwent;

    loop {
        // SAFETY: see note above; pointer is only used while non-null and before
        // the next `getpwent`/`endpwent` call.
        let passwd = unsafe { libc::getpwent() };
        if passwd.is_null() {
            break;
        }
        // SAFETY: `getpwent` returned a non-null pointer to a valid `passwd`.
        let passwd = unsafe { &*passwd };

        write_field(&mut out, passwd.pw_name)?;
        out.write_all(b":")?;
        write_field_or_star(&mut out, passwd.pw_passwd)?;
        write!(out, ":{}:{}:", passwd.pw_uid, passwd.pw_gid)?;
        write_field(&mut out, passwd.pw_gecos)?;
        out.write_all(b":")?;
        write_field(&mut out, passwd.pw_dir)?;
        out.write_all(b":")?;
        write_field(&mut out, passwd.pw_shell)?;
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
