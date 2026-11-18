// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{
    fmt::{Debug, Display},
    io::{self, Write},
    panic::{UnwindSafe, catch_unwind, set_hook, take_hook},
    process::exit,
};

type ExitCode = i32;

/// Exit status of a fatal error raised while running the program, and of a
/// panic, as in gawk. Errors before the program runs exit with
/// [`EXIT_FAILURE`] instead.
pub const AWK_FATAL_CODE: ExitCode = 2;
pub const EXIT_FAILURE: ExitCode = 1;
pub const EXIT_SUCCESS: ExitCode = 0;

fn install_abort_hook() {
    let run_hook = take_hook();

    set_hook(Box::new(move |info| {
        run_hook(info);
        exit(AWK_FATAL_CODE);
    }));
}

/// Ensures exit code 2 on panics, which is GNU's behavior. Also installs
/// color-eyre's panic hook.
/// <https://www.gnu.org/software/gawk/manual/html_node/Exit-Status.html>
#[inline(always)] // Hide from stack trace on panics.
pub fn ensure_consistent_panic<T>(f: impl UnwindSafe + FnOnce() -> T) -> T {
    if cfg!(panic = "abort") {
        // Prevents core dumps on panic. We _might_ want to carve an exception.
        install_abort_hook();
        f()
    } else {
        // If unwinding is enabled, we catch it before the Rust entry point
        // does and exit with our custom exit code. The panic msg is printed.
        match catch_unwind(f) {
            Ok(x) => x,
            Err(_) => exit(AWK_FATAL_CODE),
        }
    }
}

/// Exits with a custom exit code or uses POSIX default.
#[allow(dead_code)]
pub fn exit_with(res: Result<Option<impl Into<ExitCode>>, impl Display + Debug>) -> ! {
    let code = match res {
        Ok(Some(x)) => x.into(),
        Ok(None) => EXIT_SUCCESS,
        Err(e) => exit_err(Some(e), EXIT_FAILURE),
    };

    exit(code)
}

pub fn exit_err(err: Option<impl Display + Debug>, code: i32) -> ! {
    if let Some(err) = err {
        let _ = writeln!(io::stderr(), "{err}");
    }
    exit(code)
}

/// Prints a fatal error and exits with 2, gawk's status for fatal errors.
pub fn exit_fatal(msg: impl Display) -> ! {
    let _ = writeln!(io::stderr(), "awk: fatal: {msg}");
    exit(AWK_FATAL_CODE)
}
