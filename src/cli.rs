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

use bumpalo::Bump;
use clap::{
    ArgAction, Error, Parser, ValueEnum,
    builder::{TypedValueParser, ValueParserFactory},
    error::ErrorKind,
};
use lexer::{Identifier, Token};
use memchr::memchr;

#[derive(Parser, Debug)]
#[clap(version, name = "uutils AWK")]
#[clap(about = ::std::concat!("uutils awk ", ::std::env!("CARGO_PKG_VERSION")))]
pub struct Args {
    #[arg(required_unless_present_any = ["file", "source"])]
    pub code: Option<OsString>,
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        default_value = "-"
    )]
    pub read_queue: Vec<ArgQueueItem>,
    #[arg(short = 'f', long)]
    pub file: Vec<PathBuf>,
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
    #[arg(short = 'e', long)]
    pub source: Vec<OsString>,
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

#[derive(Clone, Debug)]
pub enum ArgQueueItem {
    File(PathBuf),
    Assignment(KeyValue),
    Stdio,
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
    namespace: String,
    ident: String,
    value: OsString,
}

impl KeyValue {
    fn parse_from(value: &OsStr) -> Result<Self, Error> {
        // TODO: move to ariadne.
        let err = |msg| Err(Error::raw(ErrorKind::InvalidValue, msg));
        let bytes = value.as_encoded_bytes();
        let Some(del) = memchr(b'=', bytes) else {
            return err("Expected `key=value`");
        };
        let (k, value) = bytes.split_at(del);

        // SAFETY: `=` is a valid char boundary; encoded bytes are also valid.
        let value = unsafe { OsString::from_encoded_bytes_unchecked(value[1..].to_vec()) };
        let tmp_arena = Bump::with_capacity(32);
        let mut lex = Token::lex(k, &tmp_arena, false, false);
        let Some(Ok(Token::Identifier(Identifier { literal }))) = lex.next() else {
            return err("Invalid identifier");
        };

        let kv = match lex.next() {
            None => Self {
                namespace: "awk".to_string(),
                ident: literal.to_string(),
                value,
            },
            Some(Ok(Token::PathSpec))
                if let Some(Ok(Token::Identifier(Identifier { literal: ident }))) = lex.next() =>
            {
                Self {
                    namespace: literal.to_string(),
                    ident: ident.to_string(),
                    value,
                }
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

impl TypedValueParser for ArgQueueItemFactory {
    type Value = ArgQueueItem;

    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, Error> {
        if value == OsStr::new("-") {
            Ok(ArgQueueItem::Stdio)
        } else if let Ok(key_val) = KeyValue::parse_from(value) {
            Ok(ArgQueueItem::Assignment(key_val))
        } else {
            Ok(ArgQueueItem::File(PathBuf::from(value)))
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ArgQueueItemFactory;

impl ValueParserFactory for ArgQueueItem {
    type Parser = ArgQueueItemFactory;

    fn value_parser() -> Self::Parser {
        ArgQueueItemFactory
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
