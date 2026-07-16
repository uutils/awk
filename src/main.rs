// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

// static POSIX: bool = false;

mod cli;
mod event;
mod utils;

use std::{
    env::args_os,
    fs,
    io::{BufWriter, Write, stdout},
};

use bumpalo::Bump;
use clap::Parser as _;
use color_eyre::Result;
use interpreter::{CodeGen, ExecMode, Interpreter};
use parser::{FileCache, Parser};

use crate::{
    cli::{Args, KeyValue},
    event::AwkRt,
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

    let rt_arena = Bump::with_capacity(4000); // 4KB minus metadata-ish
    let (mut cg, metadata) = {
        let ast_arena = Bump::with_capacity(4000);
        let code = args.code.as_ref().unwrap(); // TODO: handle other forms of code input.
        let mut parser = Parser::new(&ast_arena, args.pretty_print.is_some());
        let ast = match parser.parse(None, code.as_encoded_bytes()) {
            Ok(ast) => ast,
            Err((report, source)) => {
                report.eprint((FileCache(None), source)).unwrap();
                return Ok(());
            }
        };

        if let Some(file) = args.pretty_print {
            fs::write(file, format!("{ast}"))?;
        }

        let mut cg = CodeGen::new(&rt_arena);
        cg.lower_ast(ast);
        (cg, ast.loc_metadata.clone())
    };

    for KeyValue { .. } in args.assign {
        todo!()
    }

    let bc = cg.bytecode();
    let intrp = Interpreter::new(ExecMode::Uu, cg);

    #[cfg(not(target_arch = "wasm32"))]
    if args.debug.is_some() {
        use comfy_table::{ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};

        let source = args.code.as_ref().unwrap().as_encoded_bytes();
        let mut out = BufWriter::new(stdout().lock());
        assert_eq!(bc.code.len(), bc.metadata.len());

        let bytecode = bc.code.iter().zip(bc.metadata.iter()).map(|(&x, &m)| {
            let (_, (span, file)) = &metadata[m];
            let span = String::from_utf8_lossy(&source[span.clone()]);
            let span = span
                .split_once('\n')
                .map_or(span.to_string(), |(s, _)| format!("{s}..."));
            [format!("{x:?}"), x.to_string(), span, format!("{file:?}")]
        });

        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL_CONDENSED)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(["Bytecode", "Dissassembled", "Span", "File"])
            .add_rows(bytecode);
        writeln!(out, "{table}")?;
    }

    AwkRt::new(intrp, bc, &args.read_queue).main_event_loop()?;

    Ok(())
}
