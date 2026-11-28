// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

#![allow(dead_code)]

use std::{
    convert::Infallible,
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use clap::{
    ArgAction, ArgMatches, CommandFactory, Error, FromArgMatches, Parser, ValueEnum,
    builder::{TypedValueParser, ValueParserFactory},
    error::ErrorKind,
    value_parser,
};
use lexer::{BlobId, IdentId, StaticInterner, Token};
use memchr::memchr;
use parser::ProgramSource;

#[derive(Parser, Debug)]
#[clap(version, name = "uutils AWK")]
#[clap(about = ::std::concat!("uutils awk ", ::std::env!("CARGO_PKG_VERSION")))]
pub struct Args {
    #[arg(required_unless_present_any = ["file", "source"])]
    pub code: Option<OsString>,
    #[command(flatten)]
    pub program: ProgramSources,
    /// Input files and assignments as typed, kept raw for `ARGV`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub operands: Vec<OsString>,
    #[arg(skip)]
    pub read_queue: Vec<ArgQueueItem>,
    #[arg(short = 'F', long)]
    pub field_separator: Option<OsString>,
    #[arg(short = 'v', long)]
    pub assign: Vec<KeyValue>,
    #[arg(short = 'b', long)]
    pub characters_as_bytes: bool,
    #[arg(short = 'c', long)]
    pub traditional: bool,
    #[arg(short = 'C', long)]
    pub copyright: bool,
    #[arg(
        short = 'd',
        long,
        num_args = 0..=1,
        default_missing_value = "./awkvars.out",
        help = "[default value, if empty: `./awkvars.out`]"
    )]
    pub dump_variables: Option<PathBuf>,
    #[arg(short = 'D', long, num_args = 0..=1, default_missing_value = "", value_parser = parse_debug_mode)]
    pub debug: Option<DebugMode>,
    #[arg(short = 'E', long)]
    pub exec: Option<PathBuf>,
    #[arg(short = 'g', long)]
    pub gen_pot: bool,
    #[arg(short = 'i', long)]
    pub include: Vec<PathBuf>,
    #[arg(short = 'I', long)]
    pub trace: bool,
    #[arg(short = 'l', long)]
    pub load: Vec<OsString>,
    #[arg(short = 'L', long, num_args = 0..=1, default_missing_value = "basic", value_enum)]
    pub lint: Vec<LintMode>,
    #[arg(short = 'M', long)]
    pub bignum: bool,
    #[arg(short = 'n', long)]
    pub non_decimal_data: bool,
    #[arg(short = 'N', long)]
    pub use_lc_numeric: bool,
    #[arg(short = 'o',
         long,
         num_args = 0..=1,
         default_missing_value = "./awkprof.out",
         help = "[default value, if empty: `./awkprof.out`]"
     )]
    pub pretty_print: Option<PathBuf>,
    #[arg(short = 'O', long, default_value_t = true, action = ArgAction::SetTrue, overrides_with = "no_optimize")]
    pub optimize: bool,
    #[arg(short = 's', long = "no-optimize", action = ArgAction::SetTrue, overrides_with = "optimize")]
    pub no_optimize: bool,
    #[arg(short = 'p',
        long,
        num_args = 0..=1,
        default_missing_value = "./awkprof.out",
        help = "[default value, if empty: `./awkprof.out`]"
    )]
    pub profile: Option<PathBuf>,
    #[arg(short = 'P', long)]
    pub posix: bool,
    #[arg(short = 'r', long, default_value_t = true, action = ArgAction::SetTrue)]
    pub re_interval: bool,
    #[arg(short = 'S', long)]
    pub sandbox: bool,
    #[arg(short = 't', long)]
    pub lint_old: bool,
    #[arg(short = 'k', long, conflicts_with = "posix")]
    pub csv: bool,
}

impl Args {
    /// Parses the command line like `try_parse_from`, then settles the operands:
    /// with -f or --source the first operand is an input, and no inputs means stdin.
    pub fn try_parse_args<I, T>(itr: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut args = Self::try_parse_from(end_options_at_first_operand(
            itr.into_iter().map(Into::into).collect(),
        ))?;
        if !args.program.0.is_empty()
            && let Some(operand) = args.code.take()
        {
            args.operands.insert(0, operand);
        }
        args.read_queue = args
            .operands
            .iter()
            .map(|operand| ArgQueueItem::from_operand(operand))
            .collect();
        if args.read_queue.is_empty() {
            args.read_queue.push(ArgQueueItem::Stdio);
        }
        Ok(args)
    }
}

/// gawk reads options only up to the first operand (the program text, or the first input
/// with -f) and keeps the rest as typed. A `--` there stops clap parsing them.
fn end_options_at_first_operand(mut args: Vec<OsString>) -> Vec<OsString> {
    let mut cmd = Args::command();
    cmd.build();
    let mut i = 1;
    while let Some(arg) = args.get(i) {
        let bytes = arg.as_encoded_bytes();
        if bytes == b"--" {
            // clap ends the options there itself and keeps the rest as typed.
            return args;
        }
        if bytes == b"-" || !bytes.starts_with(b"-") {
            args.insert(i, "--".into());
            break;
        }
        i += 1;
        let option = if let Some(long) = bytes.strip_prefix(b"--") {
            if long.contains(&b'=') {
                continue;
            }
            cmd.get_arguments().find(|option| {
                option
                    .get_long()
                    .is_some_and(|name| name.as_bytes() == long)
            })
        } else {
            // In a cluster such as `-bF:`, the first option that takes a value ends it.
            let mut options = bytes[1..].iter().map(|&c| {
                cmd.get_arguments()
                    .find(|option| option.get_short() == Some(char::from(c)))
            });
            let Some(option) = options.find(|option| option.is_some_and(takes_value)) else {
                continue;
            };
            if options.next().is_some() {
                continue; // the value is attached
            }
            option
        };
        if option.is_some_and(|option| takes_next_argument(option, args.get(i))) {
            i += 1;
        }
    }
    args
}

fn takes_value(option: &clap::Arg) -> bool {
    option.get_num_args().is_some_and(|n| n.max_values() > 0)
}

/// Whether an option written without a value takes the next argument, as clap decides:
/// an optional value only takes it if it doesn't look like an option.
fn takes_next_argument(option: &clap::Arg, next: Option<&OsString>) -> bool {
    let Some(num_args) = option.get_num_args() else {
        return false;
    };
    num_args.min_values() > 0
        || (num_args.max_values() > 0
            && !option.is_require_equals_set()
            && next.is_some_and(|next| next == "-" || !next.as_encoded_bytes().starts_with(b"-")))
}

/// The `-f` files and `--source` texts in command-line order, as gawk reads them.
/// clap keeps each option in its own list, so the order comes from their indices.
#[derive(Clone, Debug, Default)]
pub struct ProgramSources(pub Vec<ProgramSource>);

impl FromArgMatches for ProgramSources {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, Error> {
        let files = matches.indices_of("file").into_iter().flatten().zip(
            matches
                .get_many::<PathBuf>("file")
                .into_iter()
                .flatten()
                .map(|path| ProgramSource::File(path.clone())),
        );
        let texts = matches.indices_of("source").into_iter().flatten().zip(
            matches
                .get_many::<OsString>("source")
                .into_iter()
                .flatten()
                .map(|text| ProgramSource::Text(text.clone())),
        );
        let mut sources: Vec<_> = files.chain(texts).collect();
        sources.sort_by_key(|&(index, _)| index);
        Ok(Self(
            sources.into_iter().map(|(_, source)| source).collect(),
        ))
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl clap::Args for ProgramSources {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        cmd.arg(
            clap::Arg::new("file")
                .short('f')
                .long("file")
                .value_name("FILE")
                .action(ArgAction::Append)
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            clap::Arg::new("source")
                .short('e')
                .long("source")
                .value_name("SOURCE")
                .action(ArgAction::Append)
                .value_parser(value_parser!(OsString)),
        )
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        Self::augment_args(cmd)
    }
}

#[derive(Clone, Debug)]
pub enum ArgQueueItem {
    File(PathBuf),
    Assignment(KeyValue),
    Stdio,
}

impl ArgQueueItem {
    pub fn from_operand(value: &OsStr) -> Self {
        if value == OsStr::new("-") {
            Self::Stdio
        } else if let Ok(key_val) = KeyValue::parse_from(value) {
            Self::Assignment(key_val)
        } else {
            Self::File(PathBuf::from(value))
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum LintMode {
    Basic,
    Fatal,
    Invalid,
    #[value(name = "no-ext")]
    NoExt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugMode {
    Interactive,
    File(PathBuf),
}

#[allow(clippy::unnecessary_wraps)]
fn parse_debug_mode(s: &str) -> Result<DebugMode, Infallible> {
    if s.is_empty() {
        Ok(DebugMode::Interactive)
    } else {
        Ok(DebugMode::File(s.into()))
    }
}

#[derive(Clone, Debug)]
pub struct KeyValue {
    namespace: IdentId,
    ident: IdentId,
    value: BlobId,
}

impl KeyValue {
    fn parse_from(value: &OsStr) -> Result<Self, Error> {
        // TODO: move to ariadne.
        let err = |msg| Err(Error::raw(ErrorKind::InvalidValue, msg));
        let bytes = value.as_encoded_bytes();
        let Some(del) = memchr(b'=', bytes) else {
            return err("Expected `key=value`");
        };
        let (key, value) = bytes.split_at(del);

        // SAFETY: `=` is a valid char boundary; encoded bytes are also valid.
        let value = StaticInterner::intern(&value[1..]);
        let mut lex = Token::lex(key, false, false);
        let Some(Ok(Token::Identifier(literal))) = lex.next() else {
            return err("Invalid identifier");
        };

        let kv = match lex.next() {
            None => Self {
                namespace: StaticInterner::awk_ident(),
                ident: literal,
                value,
            },
            Some(Ok(Token::PathSpec)) if let Some(Ok(Token::Identifier(ident))) = lex.next() => {
                Self { namespace: literal, ident, value }
            }
            _ => return err("Invalid identifier"),
        };

        let None = lex.next() else {
            return err("Expected `key=value`");
        };
        Ok(kv)
    }
}

impl TypedValueParser for KeyValueFactory {
    type Value = KeyValue;

    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, Error> {
        KeyValue::parse_from(value)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KeyValueFactory;

impl ValueParserFactory for KeyValue {
    type Parser = KeyValueFactory;

    fn value_parser() -> Self::Parser {
        KeyValueFactory
    }
}
