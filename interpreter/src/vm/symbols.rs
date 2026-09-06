// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! TODO: `SYMTAB`, `FUNCTAB`, `PROCINFO` magic, auto-set variables.

use std::{borrow::Cow, cell::RefCell, io::Write, iter::once, mem::take, rc::Rc};

use ahash::RandomState;
use bumpalo::Bump;
use indexmap_allocator_api::IndexMap;
use itertools::Itertools;
use memchr::{memchr_iter, memchr3_iter};
use minrx::RegexError;
use parser::{Identifier, Span, SpanExt};
use smallvec::SmallVec;

use crate::{
    ExecMode,
    ir::{BuiltInVar, UserNonLocal},
    vm::{
        Function, regex,
        types::{ArrayMap, Value},
    },
};

#[derive(Debug)]
pub(super) struct RawSymbolTable<'a, T>(IndexMap<Identifier<'a>, T, RandomState, &'a Bump>);

#[derive(Debug)]
pub struct SymbolTable<'a> {
    pub(super) user: RawSymbolTable<'a, Value<'a>>,
    pub(super) functions: RawSymbolTable<'a, Option<Function>>,
    // Built-in variables as dedicated fields. `ENVIRON`, `PROCINFO`, `SYMTAB`, and
    // `FUNCTAB` are intentionally omitted — they will be separate instructions.
    /// Number of elements in `ARGV`. Set from the CLI at startup; the program may
    /// change it to add/drop input files.
    pub(super) argc: Value<'a>,
    /// GNU extension: index in `ARGV` of the current input file. Updated when the
    /// interpreter opens the next file.
    pub(super) argind: Value<'a>,
    /// Command-line arguments (`ARGV[0]` … `ARGV[ARGC-1]`). Filled at startup;
    /// rewriting elements changes which files are read.
    pub(super) argv: Value<'a>,
    /// GNU extension: binary I/O mode on non-POSIX platforms. Examined when
    /// opening files or pipes.
    pub(super) binmode: Value<'a>,
    /// `sprintf` format for number→string conversion outside `print`. Default
    /// `"%.6g"`. Read on numeric-to-string coercion.
    pub(super) convfmt: Value<'a>,
    /// GNU extension: set to a descriptive string when redirected `getline`, a
    /// read, or `close` fails.
    pub(super) errno: Value<'a>,
    /// GNU extension: whitespace-separated fixed field widths. When assigned,
    /// overrides `FS` for input field splitting.
    pub(super) fieldwidths: Value<'a>,
    /// Current input file name (`"-"` for stdin). Updated on each file switch;
    /// empty in `BEGIN` until input starts.
    pub(super) filename: Value<'a>,
    /// Record number within the current file. Incremented per record; reset when
    /// a new file is opened.
    pub(super) fnr: Value<'a>,
    /// GNU extension: regexp describing field contents. When assigned, overrides
    /// `FS` for input field splitting.
    pub(super) fpat: Value<'a>,
    /// Input field separator. Default `" "`. Examined when splitting `$0`.
    pub(super) fs: Value<'a>,
    /// GNU extension: non-zero enables case-insensitive string/regexp ops.
    pub(super) ignorecase: Value<'a>,
    /// GNU extension: dynamic control of `--lint` from AWK code.
    pub(super) lint: Value<'a>,
    /// Total records read so far. Incremented on each record read.
    pub(super) nr: Value<'a>,
    /// `sprintf` format for numbers in `print`. Default `"%.6g"`.
    pub(super) ofmt: Value<'a>,
    /// Output field separator. Default `" "`. Inserted between `print` fields.
    pub(super) ofs: Value<'a>,
    /// Output record separator. Default `"\n"`. Appended after each `print`.
    pub(super) ors: Value<'a>,
    /// GNU extension: working precision for arbitrary-precision floats. Default
    /// `53`.
    pub(super) prec: Value<'a>,
    /// GNU extension: rounding mode for arbitrary-precision arithmetic. Default
    /// `"N"` (IEEE-754 roundTiesToEven).
    pub(super) roundmode: Value<'a>,
    /// Input record separator. Default `"\n"`. Examined when reading records.
    pub(super) rs: Value<'a>,
    /// GNU extension: input text that matched `RS` for the last record read.
    pub(super) rt: Value<'a>,
    /// Start index (1-based) of the last `match()` hit; `0` if none. Set by
    /// `match()`.
    pub(super) rstart: Value<'a>,
    /// Length of the last `match()` hit (`-1` after a failed match). Set by
    /// `match()`.
    pub(super) rlength: Value<'a>,
    /// Subscript separator for multi-dimensional array keys. Default `"\034"`.
    /// Read when building compound array indices.
    pub(super) subsep: Value<'a>,
    /// GNU extension: gettext text domain for localized strings. Default
    /// `"messages"`.
    pub(super) textdomain: Value<'a>,
}

#[derive(Debug, Default)]
pub struct Record {
    raw: Vec<u8>,
    fields: Option<Vec<Span>>,
}

impl<'a, T> RawSymbolTable<'a, T> {
    pub(super) fn new_in(arena: &'a Bump) -> Self {
        Self(IndexMap::new_in(arena))
    }

    pub(super) fn register(
        &mut self,
        ident: &Identifier,
        value: T,
        bump: &'a Bump,
    ) -> UserNonLocal {
        if let Some(index) = self.0.get_index_of(ident) {
            UserNonLocal(index.try_into().unwrap())
        } else {
            let ident = Identifier {
                namespace: bump.alloc_str(ident.namespace),
                literal: bump.alloc_str(ident.literal),
            };
            UserNonLocal(self.0.insert_full(ident, value).0.try_into().unwrap())
        }
    }

    #[inline(always)]
    pub(super) fn get_index(&self, var: UserNonLocal) -> Option<&T> {
        self.0.get_index(var.0 as usize).map(|x| x.1)
    }

    #[inline(always)]
    pub(super) fn get_index_mut(&mut self, var: UserNonLocal) -> Option<&mut T> {
        self.0.get_index_mut(var.0 as usize).map(|x| x.1)
    }

    #[inline(always)]
    pub(super) fn insert(&mut self, ident: Identifier<'a>, value: T) -> Option<T> {
        self.0.insert(ident, value)
    }

    #[inline(always)]
    pub(super) fn lookup(&mut self, ident: &Identifier) -> Option<(UserNonLocal, &mut T)> {
        self.0
            .get_index_of(ident)
            .map(|ix| (UserNonLocal(ix.try_into().unwrap()), &mut self.0[ix]))
    }

    #[inline(always)]
    pub(super) fn iter(&self) -> impl Iterator<Item = (&Identifier<'a>, &T)> {
        self.0.iter()
    }
}

impl<'a> SymbolTable<'a> {
    pub fn new_in(arena: &'a Bump) -> Self {
        Self {
            user: RawSymbolTable::new_in(arena),
            functions: RawSymbolTable::new_in(arena),
            // Static / well-known defaults; I/O-driven values stay at their
            // pre-input zeros/empties until the reader wires them up.
            argc: Value::Int(0),
            argind: Value::Int(0),
            argv: Value::empty_array(),
            binmode: Value::Int(0),
            convfmt: Value::String(b"%.6g".into()),
            errno: Value::String(b"".into()),
            fieldwidths: Value::String(b"".into()),
            filename: Value::String(b"".into()),
            fnr: Value::Int(0),
            fpat: Value::String(b"[^[:space:]]+".into()),
            fs: Value::String(b" ".into()),
            ignorecase: Value::Int(0),
            lint: Value::Int(0),
            nr: Value::Int(0),
            ofmt: Value::String(b"%.6g".into()),
            ofs: Value::String(b" ".into()),
            ors: Value::String(b"\n".into()),
            prec: Value::Int(53),
            roundmode: Value::String(b"N".into()),
            rs: Value::String(b"\n".into()),
            rt: Value::String(b"".into()),
            rstart: Value::Int(0),
            rlength: Value::Int(0),
            subsep: Value::String(b"\x1c".into()),
            textdomain: Value::String(b"messages".into()),
        }
    }

    /// Populate `ARGC` / `ARGV` from a process argument list (`ARGV[0]` is the
    /// program name). Indices are decimal strings, as in AWK.
    pub fn set_argc_argv<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<[u8]>,
    {
        let mut map = ArrayMap::with_hasher(RandomState::new());
        let mut n = 0isize;
        for arg in args {
            map.insert(
                n.to_string(),
                Value::String(Cow::Owned(arg.as_ref().to_vec())),
            );
            n += 1;
        }
        self.argc = Value::Int(n);
        self.argv = Value::Array(Rc::new(RefCell::new(map)));
    }

    #[inline(always)]
    pub(super) fn user_mut(&mut self, var: UserNonLocal) -> &mut Value<'a> {
        self.user.get_index_mut(var).unwrap()
    }

    #[inline(always)]
    pub(super) fn user(&self, var: UserNonLocal) -> &Value<'a> {
        self.user.get_index(var).unwrap()
    }

    #[inline(always)]
    pub fn register_user_var(&mut self, var: &Identifier, bump: &'a Bump) -> UserNonLocal {
        self.user.register(var, Value::Untyped, bump)
    }

    #[inline(always)]
    pub fn register_user_var_with(&mut self, var: &Identifier, val: &str, bump: &'a Bump) {
        let ident = Identifier {
            namespace: bump.alloc_str(var.namespace),
            literal: bump.alloc_str(var.literal),
        };
        self.user.insert(
            ident,
            if let Ok(n) = val.parse() {
                // TODO: use strnum
                Value::Float(n)
            } else {
                Value::String(bump.alloc_str(val).as_bytes().into())
            },
        );
    }

    #[inline(always)]
    pub fn register_user_fun(
        &mut self,
        name: &Identifier,
        fun: Function,
        bump: &'a Bump,
    ) -> UserNonLocal {
        if let Some((nl, f)) = self.functions.lookup(name) {
            *f = Some(fun);
            nl
        } else {
            self.functions.register(name, Some(fun), bump)
        }
    }

    #[inline(always)]
    pub fn get_user_fun(&mut self, name: &Identifier, bump: &'a Bump) -> UserNonLocal {
        self.functions.register(name, None, bump)
    }

    pub fn get_btin(&self, var: BuiltInVar) -> &Value<'a> {
        match var {
            BuiltInVar::Nr => &self.nr,
            BuiltInVar::Fs => &self.fs,
            BuiltInVar::Rs => &self.rs,
            BuiltInVar::Ofs => &self.ofs,
            BuiltInVar::Ors => &self.ors,
            BuiltInVar::Filename => &self.filename,
            BuiltInVar::Argc => &self.argc,
            BuiltInVar::Argv => &self.argv,
            BuiltInVar::Subsep => &self.subsep,
            BuiltInVar::Fnr => &self.fnr,
            BuiltInVar::Argind => &self.argind,
            BuiltInVar::Ofmt => &self.ofmt,
            BuiltInVar::Rstart => &self.rstart,
            BuiltInVar::Rlength => &self.rlength,
            _ => todo!(),
        }
    }

    pub fn get_btin_mut(&mut self, var: BuiltInVar) -> &mut Value<'a> {
        match var {
            BuiltInVar::Nr => &mut self.nr,
            BuiltInVar::Fs => &mut self.fs,
            BuiltInVar::Rs => &mut self.rs,
            BuiltInVar::Ofs => &mut self.ofs,
            BuiltInVar::Ors => &mut self.ors,
            BuiltInVar::Filename => &mut self.filename,
            BuiltInVar::Argc => &mut self.argc,
            BuiltInVar::Argv => &mut self.argv,
            BuiltInVar::Subsep => &mut self.subsep,
            BuiltInVar::Fnr => &mut self.fnr,
            BuiltInVar::Argind => &mut self.argind,
            BuiltInVar::Ofmt => &mut self.ofmt,
            BuiltInVar::Rstart => &mut self.rstart,
            BuiltInVar::Rlength => &mut self.rlength,
            _ => todo!(),
        }
    }
}

impl Record {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn nf<'a>(
        &mut self,
        symbols: &mut SymbolTable<'a>,
        mode: ExecMode,
    ) -> Result<Value<'a>, RegexError> {
        let fields = self.split_fields_raw(symbols, mode)?;
        Ok(Value::Int(fields.len() as isize - 1))
    }

    /// Splits the fields if unsplit and grants access to the inner buffer of
    /// spans of the record.
    fn split_fields_raw(
        &mut self,
        symbols: &mut SymbolTable,
        mode: ExecMode,
    ) -> Result<&mut Vec<Span>, RegexError> {
        // TODO: trace FPAT/FIELDWIDTHS assignments.
        // TODO: string caching; non UTF-8 conversions
        match self.fields {
            Some(ref mut fields) => Ok(fields),
            None if false => {
                let fpat = symbols.fpat.to_string();
                self.fpat_regex_split(fpat.as_bytes(), mode)
            }
            None if false => {
                let _fieldwidths = symbols.fieldwidths.to_string();
                todo!()
            }
            None => {
                let fs = symbols.fs.to_string();
                let mut fs_chars = fs.chars();
                match &*fs {
                    " " => Ok(self.fs_whitespace_split()),
                    _ if let Some(char) = fs_chars.next()
                        && fs_chars.next().is_none() =>
                    {
                        Ok(self.fs_char_split(char))
                    }
                    "" => Ok(self.fs_all_split()),
                    s => self.fs_regex_split(s.as_bytes(), mode),
                }
            }
        }
    }

    fn init_fields(&mut self) -> (&mut Vec<u8>, &mut Vec<Span>) {
        let buf = self.fields.get_or_insert_default();
        buf.clear();
        buf.push(Span::from(0..self.raw.len())); // $0

        (&mut self.raw, buf)
    }

    fn fs_whitespace_split(&mut self) -> &mut Vec<Span> {
        let (raw, buf) = self.init_fields();
        buf.extend(
            memchr3_iter(b' ', b'\t', b'\n', raw)
                .map(to_span)
                .coalesce(|a, b| (a.end == b.start).then_some(b.since(a.start)).ok_or((a, b)))
                .split_by(raw.len()),
        );
        buf
    }

    fn fs_char_split(&mut self, c: char) -> &mut Vec<Span> {
        let mut bytes = [0; 4];
        let bytes = c.encode_utf8(&mut bytes).as_bytes();
        let last_i = bytes.len() - 1;
        let last = bytes[last_i];
        let (raw, buf) = self.init_fields();

        buf.extend(
            memchr_iter(last, raw)
                .map(|i| Span::from(i.saturating_sub(last_i)..i + 1))
                .filter(|&span| raw[span].ends_with(bytes))
                .split_by(raw.len()),
        );
        buf
    }

    /// Splits the record into fields via the regex engine with `FS` as the
    /// value separator.
    //
    /// # Note:
    /// GNU appears to finish after the first null match. This is undefined by
    /// POSIX and gawk behaves the same in POSIX mode. Essentially, this means
    /// that for `FS = "ab|()"`:
    ///   * `$0 = "abc def"` is split as `$1 = ""`, `$2 = "c def"`.
    ///   * `$0 = "12 34"` is split as `$1 = "12 34"`.
    ///
    /// This may be a bug and is historically inconsistent; we encapsulate this
    /// in the `let matches = ...` statement below. Removing it matches just
    /// like the empty `FS`, except for a possible leading null match.
    fn fs_regex_split(&mut self, fs: &[u8], mode: ExecMode) -> Result<&mut Vec<Span>, RegexError> {
        let (raw, buf) = self.init_fields();
        regex::automaton(fs, mode, false)?
            .find_iter(&raw)
            .map(|m| m.map(Span::from))
            .process_results(|matches| {
                let matches = matches.take_while(|s| !s.is_empty());
                buf.extend(split_by(matches, raw.len()));
            })?;
        Ok(buf)
    }

    fn fs_all_split(&mut self) -> &mut Vec<Span> {
        let (raw, buf) = self.init_fields();
        buf.extend((0..raw.len()).map(to_span));
        buf
    }

    fn fpat_regex_split(
        &mut self,
        fpat: &[u8],
        mode: ExecMode,
    ) -> Result<&mut Vec<Span>, RegexError> {
        let (raw, buf) = self.init_fields();
        regex::automaton(fpat, mode, false)?
            .find_iter(&raw)
            .map(|m| m.map(Span::from))
            .process_results(|matches| buf.extend(matches))?;
        Ok(buf)
    }

    /// Gets access to the byte span of the `n`th field. Splits if unsplit.
    fn get_raw(
        &mut self,
        n: usize,
        symbols: &mut SymbolTable,
        mode: ExecMode,
    ) -> Result<Option<&[u8]>, RegexError> {
        let span = self.split_fields_raw(symbols, mode)?.get(n).copied();
        Ok(span.and_then(|span| self.raw.get(span)))
    }

    /// Materializes the value of the `n`th field. Splits the fields if unsplit.
    pub fn get_val<'a>(
        &mut self,
        n: usize,
        symbols: &mut SymbolTable<'a>,
        mode: ExecMode,
    ) -> Result<Value<'a>, RegexError> {
        self.get_raw(n, symbols, mode).map(|val| match val {
            Some(val) => Value::String(val.to_vec().into()),
            None => Value::Unassigned,
        })
    }

    /// Overwrites the `n`th field with `val`, and reconstructs the record with
    /// `OFS` as the field separator. Also updates the inner field spans; this
    /// is load-bearing since field-splitting isn't necessarily idempotent.
    pub fn write_field(
        &mut self,
        val: Value<'_>,
        n: usize,
        symbols: &mut SymbolTable,
        mode: ExecMode,
    ) -> Result<(), RegexError> {
        if n == 0 {
            self.write_record_raw(val);
            Ok(())
        } else {
            self.write_field_raw(&val, n, symbols, mode)
        }
    }

    /// Rewrites the entire record and invalidates the field splits.
    fn write_record_raw(&mut self, val: Value<'_>) {
        self.fields = None;
        val.move_string_into(&mut self.raw);
    }

    /// Writes to a field and reconstructs the record. Check the doc comment of
    /// the public function for more details.
    fn write_field_raw(
        &mut self,
        val: &Value<'_>,
        n: usize,
        symbols: &mut SymbolTable,
        mode: ExecMode,
    ) -> Result<(), RegexError> {
        let fields = take(self.split_fields_raw(symbols, mode)?);
        self.reconstruct(n, Some(val), false, symbols, fields);

        Ok(())
    }

    /// Truncates or extends the record when writing to `NF`.
    pub fn resize(
        &mut self,
        n: usize,
        symbols: &mut SymbolTable,
        mode: ExecMode,
    ) -> Result<(), RegexError> {
        let fields = take(self.split_fields_raw(symbols, mode)?);
        self.reconstruct(n, None, true, symbols, fields);

        Ok(())
    }

    /// Rebuilds the record with `OFS` as field separator, truncating or
    /// extending on demand and updating the field spans. Also takes a new
    /// value to be written. Writes the data to [`Self`] at the end.
    fn reconstruct(
        &mut self,
        at: usize,
        new: Option<&Value>,
        truncate: bool,
        symbols: &mut SymbolTable,
        mut fields: Vec<Span>,
    ) {
        // If writing out-of-bounds, we grow the record by one OFS each and
        // materialize empty fields. This is partly why re-splitting isn't
        // idempotent, as a regex engine could eat up the consecutive FSs.
        if at >= fields.len() {
            let raw_len = self.raw.len();
            fields.resize(at + 1, Span::from(raw_len..raw_len));
        } else if truncate {
            fields.truncate(at + 1);
        }

        let mut ofs = SmallVec::<[u8; 16]>::new();
        let _ = write!(ofs, "{}", symbols.ofs);
        let val_size = new.map(Value::string_size_hint).unwrap_or_default();
        let mut buf = Vec::with_capacity(self.raw.len() + val_size + ofs.len());

        for (i, span) in fields.iter_mut().enumerate().skip(1) {
            if i > 1 {
                buf.extend_from_slice(&ofs);
            }

            let start = buf.len();
            match new {
                Some(val) if i == at => val.write_string(&mut buf),
                _ => buf.extend_from_slice(&self.raw()[*span]),
            }
            *span = Span::from(start..buf.len());
        }
        fields[0] = Span::from(0..buf.len());

        self.raw = buf;
        self.fields = Some(fields);
    }

    /// Grants access to the record's byte slice.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn clear(&mut self) {
        self.raw.clear();
        self.fields = None;
    }

    pub fn invalidate(&mut self) {
        self.fields = None;
    }

    pub fn write_new(&mut self) -> &mut Vec<u8> {
        self.clear();
        &mut self.raw
    }
}

trait SplitByExt {
    fn split_by(self, len: usize) -> impl Iterator<Item = Span>;
}

impl<T> SplitByExt for T
where
    T: Iterator<Item = Span>,
{
    fn split_by(self, len: usize) -> impl Iterator<Item = Span> {
        split_by(self, len)
    }
}

fn split_by(iter: impl Iterator<Item = Span>, len: usize) -> impl Iterator<Item = Span> {
    once(Span::from(0..0))
        .chain(iter)
        .chain(once(Span::from(len..len)))
        .tuple_windows()
        .map(|(prev, next)| Span::from(prev.end..next.start))
}

#[inline]
const fn to_span(start: usize) -> Span {
    Span { start, end: start + 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_table_builtin_defaults() {
        let arena = Bump::new();
        let st = SymbolTable::new_in(&arena);

        assert_eq!(st.argc, Value::Int(0));
        assert_eq!(st.argind, Value::Int(0));
        assert!(matches!(st.argv, Value::Array(_)));
        assert_eq!(st.binmode, Value::Int(0));
        assert_eq!(st.convfmt, Value::String(b"%.6g".into()));
        assert_eq!(st.errno, Value::String(b"".into()));
        assert_eq!(st.fieldwidths, Value::String(b"".into()));
        assert_eq!(st.filename, Value::String(b"".into()));
        assert_eq!(st.fnr, Value::Int(0));
        assert_eq!(st.fpat, Value::String(b"[^[:space:]]+".into()));
        assert_eq!(st.fs, Value::String(b" ".into()));
        assert_eq!(st.ignorecase, Value::Int(0));
        assert_eq!(st.lint, Value::Int(0));
        assert_eq!(st.nr, Value::Int(0));
        assert_eq!(st.ofmt, Value::String(b"%.6g".into()));
        assert_eq!(st.ofs, Value::String(b" ".into()));
        assert_eq!(st.ors, Value::String(b"\n".into()));
        assert_eq!(st.prec, Value::Int(53));
        assert_eq!(st.roundmode, Value::String(b"N".into()));
        assert_eq!(st.rs, Value::String(b"\n".into()));
        assert_eq!(st.rt, Value::String(b"".into()));
        assert_eq!(st.rstart, Value::Int(0));
        assert_eq!(st.rlength, Value::Int(0));
        assert_eq!(st.subsep, Value::String(b"\x1c".into()));
        assert_eq!(st.textdomain, Value::String(b"messages".into()));
    }

    #[test]
    fn set_argc_argv_populates_argc_and_argv() {
        let arena = Bump::new();
        let mut st = SymbolTable::new_in(&arena);
        st.set_argc_argv([b"awk".as_slice(), b"a.txt", b"b.txt"]);

        assert_eq!(st.argc, Value::Int(3));
        let Value::Array(argv) = &st.argv else {
            panic!("expected ARGV array");
        };
        let argv = argv.borrow();
        assert_eq!(argv.get("0"), Some(&Value::String(b"awk".into())));
        assert_eq!(argv.get("1"), Some(&Value::String(b"a.txt".into())));
        assert_eq!(argv.get("2"), Some(&Value::String(b"b.txt".into())));
        assert_eq!(argv.len(), 3);
    }
}
