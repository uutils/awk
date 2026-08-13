// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

// TODO: More regex helpers/utilities, otherwise merge into another module.

use minrx::{BuildError, Regex, RegexBuilder};

use crate::ExecMode;

pub fn automaton(pattern: &[u8], mode: ExecMode, icase: bool) -> Result<Regex, BuildError> {
    match mode {
        // On POSIX mode, gawk does not enable brace compat or other extensions.
        // TODO: C-locale-dependant native encoding? Must test.
        ExecMode::Posix => RegexBuilder::new().case_insensitive(icase),
        _ => RegexBuilder::new()
            .case_insensitive(icase)
            .brace_compat(true)
            .bsd_extensions(true)
            .gnu_extensions(true),
    }
    .build(pattern)
}
