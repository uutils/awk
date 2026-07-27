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
    mem::take,
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
    let (mut cg, metadata, diagnostics) = {
        let ast_arena = Bump::with_capacity(4000);
        let code = args.code.as_ref().unwrap(); // TODO: handle other forms of code input.
        let mut parser = Parser::new(&ast_arena, args.pretty_print.is_some());
        let ast = match parser.parse(FileCache(None), code.as_encoded_bytes()) {
            Ok(ast) => ast,
            Err(mut diagnostics) => {
                diagnostics.flush()?;
                return Ok(());
            }
        };
        ast.diagnostics.flush()?;

        if let Some(file) = args.pretty_print {
            fs::write(file, format!("{ast}"))?;
        }

        let mut cg = CodeGen::new(&rt_arena);
        cg.lower_ast(ast);
        (cg, take(&mut ast.loc_metadata), take(&mut ast.diagnostics))
    };

    for KeyValue { .. } in args.assign {
        todo!()
    }

    let bc = cg.bytecode();

    // TODO: do away with this and get the actual debugger running.
    #[cfg(not(target_arch = "wasm32"))]
    if args.debug.is_some() {
        use comfy_table::{ContentArrangement, Table, presets::UTF8_FULL_CONDENSED};

        let code = args.code.unwrap();
        let source = String::from_utf8_lossy(code.as_encoded_bytes());
        let mut out = BufWriter::new(stdout().lock());
        assert_eq!(bc.code.len(), bc.metadata.len());

        let bytecode = bc.code.iter().zip(bc.metadata.iter()).map(|(&x, &m)| {
            let (file, span) = &metadata[m];
            let span = source[span.clone()].to_string();

            let span = source
                .split_once('\n')
                .map_or(span, |(s, _)| format!("{s}..."));
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

    let intrp = Interpreter::new(ExecMode::Uu, cg, metadata);
    AwkRt::new(intrp, bc, &args.read_queue, diagnostics).main_event_loop()?;

    Ok(())
}
