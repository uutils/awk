// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::fmt::{Debug, Display};
use std::io::{self, Write};
use std::panic::{UnwindSafe, catch_unwind, set_hook, take_hook};
use std::process::exit;

use color_eyre::config::HookBuilder;
use rustix::process::{EXIT_FAILURE, EXIT_SUCCESS};
use tracing_error::ErrorLayer;
use tracing_subscriber::prelude::*;

type ExitCode = i32;

const AWK_PANIC_CODE: ExitCode = 2;

fn install_abort_hook() {
    let run_hook = take_hook();

    set_hook(Box::new(move |info| {
        run_hook(info);
        exit(AWK_PANIC_CODE);
    }));
}

#[inline(always)]
fn install_error_hooks() {
    tracing_subscriber::registry()
        // .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().compact())
        .with(ErrorLayer::default())
        .init();

    HookBuilder::default()
        .capture_span_trace_by_default(true)
        .install()
        .unwrap();
}

/// Ensures exit code 2 on panics, which is GNU's behavior. Also installs
/// color-eyre's panic hook.
/// https://www.gnu.org/software/gawk/manual/html_node/Exit-Status.html
#[inline(always)] // Hide from stack trace on panics.
pub fn ensure_consistent_panic<T>(f: impl UnwindSafe + FnOnce() -> T) -> T {
    install_error_hooks();
    if cfg!(panic = "abort") {
        // Prevents core dumps on panic. We _might_ want to carve an exception.
        install_abort_hook();
        f()
    } else {
        // If unwinding is enabled, we catch it before the Rust entry point
        // does and exit with our custom exit code. The panic msg is printed.
        match catch_unwind(f) {
            Ok(x) => x,
            Err(_) => exit(AWK_PANIC_CODE),
        }
    }
}

/// Exits with a custom exit code or libc's codes, as per POSIX.
#[allow(dead_code)]
pub fn exit_with(res: Result<Option<impl Into<ExitCode>>, impl Display + Debug>) -> ! {
    let code = match res {
        Ok(Some(x)) => x.into(),
        Ok(None) => EXIT_SUCCESS,
        Err(e) => exit_err(Some(e)),
    };

    exit(code)
}

pub fn exit_err(err: Option<impl Display + Debug>) -> ! {
    if let Some(err) = err {
        let _ = writeln!(io::stderr(), "{err}");
    }
    exit(EXIT_FAILURE)
}
