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
    error::Error,
    fs,
    io::{BufWriter, Write, stdout},
    mem::take,
};

use bumpalo::Bump;
use clap::{CommandFactory, FromArgMatches};
use interpreter::{CodeGen, ExecMode, Interpreter};
use parser::{FileCache, IncludeError, Parser, ProgramSource};

use crate::{
    cli::{ArgQueueItem, Args, KeyValue},
    event::AwkRt,
    utils::{AWK_FATAL_CODE, EXIT_FAILURE, ensure_consistent_panic, exit_err, exit_fatal},
};

fn main() {
    if let Err(e) = ensure_consistent_panic(uu_main) {
        exit_err(Some(e), EXIT_FAILURE)
    }
}

fn uu_main() -> Result<(), Box<dyn Error>> {
    // The matches keep each option's position, which orders -f and --source.
    let matches = match Args::command().try_get_matches_from(args_os()) {
        Ok(matches) => matches,
        Err(msg) => {
            msg.print()?;
            exit_err(Option::<&str>::None, EXIT_FAILURE)
        }
    };
    let mut args = Args::from_arg_matches(&matches)?;

    // With -f or --source there is no program operand: the first operand is an input.
    if (!args.file.is_empty() || !args.source.is_empty())
        && let Some(operand) = args.code.take()
    {
        args.read_queue
            .insert(0, ArgQueueItem::from_operand(&operand));
    }
    if args.read_queue.is_empty() {
        args.read_queue.push(ArgQueueItem::Stdio);
    }

    // Like gawk, each --source text is read as if it ended with a newline.
    let mut source_text = Vec::new();
    for text in &args.source {
        source_text.extend_from_slice(text.as_encoded_bytes());
        source_text.push(b'\n');
    }

    let rt_arena = Bump::with_capacity(4000); // 4KB minus metadata-ish
    let (mut cg, metadata, diagnostics) = {
        let ast_arena = Bump::with_capacity(4000);
        let mut parser = Parser::new(&ast_arena, args.pretty_print.is_some());
        let ast = if args.file.is_empty() {
            let code = match &args.code {
                Some(code) => code.as_encoded_bytes(),
                None => &source_text,
            };
            parser.parse(FileCache(None), code)
        } else {
            match parser.include_many(&program_sources(&matches, &args)) {
                Ok(ast) => Ok(ast),
                Err(IncludeError::Invalid(diagnostics)) => Err(diagnostics),
                Err(err) => exit_fatal(err),
            }
        };
        let ast = match ast {
            Ok(ast) => ast,
            Err(mut diagnostics) => {
                diagnostics.flush()?;
                // The program did not parse: exit with 1, as errors before
                // the program runs do.
                exit_err(Option::<&str>::None, EXIT_FAILURE)
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
        use parser::AriadneSpan;

        let code = args.code.unwrap();
        let source = String::from_utf8_lossy(code.as_encoded_bytes());
        let mut out = BufWriter::new(stdout().lock());
        assert_eq!(bc.code.len(), bc.metadata.len());

        let bytecode = bc.code.iter().zip(bc.metadata.iter()).map(|(&x, &m)| {
            let &AriadneSpan(ref file, span) = &metadata[m];
            let span = source[span.ix()].to_string();

            let span = source
                .split_once('\n')
                .map_or(span, |(s, _)| format!("{s}..."));
            [format!("{x:?}"), x.to_string(), span, format!("{file:?}")]
        });

        let mut table = Table::new();
        table
            .load_style(UTF8_FULL_CONDENSED)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(["Bytecode", "Disassembled", "Span", "File"])
            .add_rows(bytecode);
        writeln!(out, "{table}")?;
    }

    // `--posix` / `--traditional` select ExecMode::Posix (RS first-char-only,
    // FS="" whole-record field, stricter regex). Keep this mapping out of RS-only PRs.
    let mode = if args.posix || args.traditional {
        ExecMode::Posix
    } else {
        ExecMode::Uu
    };
    let intrp = Interpreter::new(mode, cg, metadata);
    let res = AwkRt::new(intrp, bc, &args.read_queue, diagnostics).main_event_loop();

    if let Err(err) = res {
        exit_err(Some(err), AWK_FATAL_CODE);
    }

    Ok(())
}

/// The -f files and --source texts in command-line order, as gawk reads them.
/// Adjacent --source texts stay one piece, so a construct can span them.
fn program_sources<'a>(matches: &clap::ArgMatches, args: &'a Args) -> Vec<ProgramSource<'a>> {
    let files = matches
        .indices_of("file")
        .into_iter()
        .flatten()
        .zip(&args.file);
    let texts = matches
        .indices_of("source")
        .into_iter()
        .flatten()
        .zip(&args.source);
    let mut pieces: Vec<(usize, Option<&'a std::path::Path>, &'a [u8])> = files
        .map(|(i, path)| (i, Some(path.as_path()), &[][..]))
        .chain(texts.map(|(i, text)| (i, None, text.as_encoded_bytes())))
        .collect();
    pieces.sort_by_key(|&(i, ..)| i);

    let mut sources = Vec::new();
    for (_, path, text) in pieces {
        match (path, sources.last_mut()) {
            (Some(path), _) => sources.push(ProgramSource::File(path)),
            (None, Some(ProgramSource::Text(joined))) => {
                joined.extend_from_slice(text);
                joined.push(b'\n');
            }
            (None, _) => {
                let mut joined = text.to_vec();
                joined.push(b'\n');
                sources.push(ProgramSource::Text(joined));
            }
        }
    }
    sources
}
