// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::env;

use uutests::util::TestScenario;

pub const TESTS_BINARY: &str = env!("CARGO_BIN_EXE_awk");

#[ctor::ctor(unsafe)]
fn init() {
    unsafe {
        env::set_var("UUTESTS_BINARY_PATH", TESTS_BINARY);
        env::remove_var("UUTESTS_UTIL_NAME");
        env::set_var("UUTESTS_UTIL_NAME", "");
        env::set_var("UUTILS_MULTICALL", "0");
    }
}

fn ucmd() -> uutests::util::UCommand {
    TestScenario::new("awk").cmd(TESTS_BINARY)
}

#[path = "by-util/test_awk.rs"]
mod test_awk;
