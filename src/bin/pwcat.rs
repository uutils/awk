// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! Dump the password database in `/etc/passwd` format for gawk library routines.
//!
//! Based on the program from the GNU Awk User's Guide (public domain).
//! <https://www.gnu.org/software/gawk/manual/html_node/Passwd-Functions.html>

use std::{
    io::{self, Write},
    process,
};

#[cfg(unix)]
const PASSWD_DB: &str = "/etc/passwd";

fn main() {
    #[cfg(unix)]
    {
        if let Err(err) = run()
            && err.kind() != io::ErrorKind::BrokenPipe
        {
            let _ = writeln!(io::stderr(), "pwcat: {err}");
            process::exit(1);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = writeln!(io::stderr(), "pwcat: not supported on this platform");
        process::exit(1);
    }
}

#[cfg(unix)]
fn run() -> io::Result<()> {
    use rustix::fs::{Mode, OFlags, open};
    use std::fs::File;

    let fd = open(PASSWD_DB, OFlags::RDONLY, Mode::empty())
        .map_err(|err| io::Error::from_raw_os_error(err.raw_os_error()))?;
    let mut input = File::from(fd);
    let mut out = io::stdout().lock();
    io::copy(&mut input, &mut out)?;
    Ok(())
}
