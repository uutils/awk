// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

// static POSIX: bool = false;

mod cli;
mod utils;

use std::env::args_os;
use std::io::{self, Write};

use bumpalo::Bump;
use clap::Parser as _;
use color_eyre::Result;
use parser::Parser;

use crate::{
    cli::Args,
    utils::{ensure_consistent_panic, exit_err},
};

fn main() {
    if let Err(e) = ensure_consistent_panic(uu_main) {
        exit_err(Some(e))
    }
}

#[tracing::instrument]
fn uu_main() -> Result<()> {
    let args = match Args::try_parse_from(args_os()) {
        Ok(args) => args,
        Err(msg) => {
            msg.print()?;
            exit_err(Option::<&str>::None)
        }
    };

    let arena = Bump::with_capacity(4000); // 4KB minus metadata-ish
    let mut parser = Parser::new(&arena);
    let ast = match parser.parse("CLI", args.code.as_encoded_bytes()) {
        Ok(ast) => dbg!(ast),
        Err((report, source)) => {
            report.eprint(("CLI", source)).unwrap();
            return Ok(());
        }
    };
    if let Err(e) = writeln!(io::stdout(), "---\n{ast}")
        && e.kind() != io::ErrorKind::BrokenPipe
    {
        exit_err(Some(format!("awk: error writing to standard output: {e}")));
    }
    dbg!(arena.chunk_capacity());

    // for token in lex {
    //     let Ok(x) = token else {
    //         return token.map(drop).map_err(color_eyre::Report::from);
    //     };
    //     println!("{x:?}");
    // }
    // exit_with(Interpreter.run())
    Ok(())
}
