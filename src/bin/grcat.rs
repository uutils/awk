// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! Dump the group database in `/etc/group` format for gawk library routines.
//!
//! Behavior matches the `grcat` helper described in the GNU Awk User's Guide:
//! <https://www.gnu.org/software/gawk/manual/html_node/Group-Functions.html>
//!
//! Uses `getgrent()`/`endgrent()` so NSS sources beyond local files (LDAP, SSSD,
//! systemd-sysusers, etc.) are included, as required by routines such as
//! `group.awk`.

use std::{
    io::{self, Write},
    process::ExitCode,
};

fn main() -> ExitCode {
    #[cfg(unix)]
    {
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
    {
        let _ = writeln!(io::stderr(), "grcat: not supported on this platform");
        ExitCode::FAILURE
    }
}

#[cfg(unix)]
fn run() -> io::Result<()> {
    use std::ffi::CStr;

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

        // SAFETY: `gr_name` is a NUL-terminated C string from the C library.
        let name = unsafe { CStr::from_ptr(group.gr_name) };
        let passwd = if group.gr_passwd.is_null() {
            None
        } else {
            // SAFETY: when non-null, `gr_passwd` is a NUL-terminated C string.
            Some(unsafe { CStr::from_ptr(group.gr_passwd) })
        };

        out.write_all(name.to_bytes())?;
        out.write_all(b":")?;
        match passwd {
            Some(pw) => out.write_all(pw.to_bytes())?,
            None => out.write_all(b"*")?,
        }
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
                // SAFETY: non-null entries are NUL-terminated C strings.
                let member = unsafe { CStr::from_ptr(member) };
                out.write_all(member.to_bytes())?;
                i += 1;
            }
        }
        out.write_all(b"\n")?;
    }

    Ok(())
}
