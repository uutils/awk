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
use minrx::RegexError;
use parser::{Identifier, Span};
use smallvec::SmallVec;

use crate::{
    ExecMode,
    ir::NonLocal,
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
    /// Field count for the current record. Updated on record read or when `$0` /
    /// fields change.
    pub(super) nf: Value<'a>,
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

    pub(super) fn register(&mut self, ident: &Identifier, value: T, bump: &'a Bump) -> NonLocal {
        if let Some(index) = self.0.get_index_of(ident) {
            NonLocal(index.try_into().unwrap())
        } else {
            let ident = Identifier {
                namespace: bump.alloc_str(ident.namespace),
                literal: bump.alloc_str(ident.literal),
            };
            NonLocal(self.0.insert_full(ident, value).0.try_into().unwrap())
        }
    }

    #[inline(always)]
    pub(super) fn get_index(&self, var: NonLocal) -> Option<&T> {
        self.0.get_index(var.0 as usize).map(|x| x.1)
    }

    #[inline(always)]
    pub(super) fn get_index_mut(&mut self, var: NonLocal) -> Option<&mut T> {
        self.0.get_index_mut(var.0 as usize).map(|x| x.1)
    }

    #[inline(always)]
    pub(super) fn insert(&mut self, ident: Identifier<'a>, value: T) -> Option<T> {
        self.0.insert(ident, value)
    }

    #[inline(always)]
    pub(super) fn lookup(&mut self, ident: &Identifier) -> Option<(NonLocal, &mut T)> {
        self.0
            .get_index_of(ident)
            .map(|ix| (NonLocal(ix.try_into().unwrap()), &mut self.0[ix]))
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
            nf: Value::Int(0),
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
    pub(super) fn user_mut(&mut self, var: NonLocal) -> &mut Value<'a> {
        self.user.get_index_mut(var).unwrap()
    }

    #[inline(always)]
    pub(super) fn user(&self, var: NonLocal) -> &Value<'a> {
        self.user.get_index(var).unwrap()
    }

    #[inline(always)]
    pub fn register_user_var(&mut self, var: &Identifier, bump: &'a Bump) -> NonLocal {
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
    ) -> NonLocal {
        if let Some((nl, f)) = self.functions.lookup(name) {
            *f = Some(fun);
            nl
        } else {
            self.functions.register(name, Some(fun), bump)
        }
    }

    #[inline(always)]
    pub fn get_user_fun(&mut self, name: &Identifier, bump: &'a Bump) -> NonLocal {
        self.functions.register(name, None, bump)
    }
}

impl Record {
    pub fn new() -> Self {
        Self::default()
    }

    /// Splits the fields if unsplit and grants access to the inner buffer of
    /// spans of the record.
    fn split_fields_raw(
        &mut self,
        symbols: &mut SymbolTable<'_>,
        mode: ExecMode,
    ) -> Result<&mut Vec<Span>, RegexError> {
        // TODO: wire in FPAT and non-regex split, etc.
        match self.fields {
            Some(ref mut fields) => Ok(fields),
            None => self.fs_regex_split(symbols, mode),
        }
    }

    /// Splits the record into fields via the regex engine with `FS` as the
    /// value separator.
    fn fs_regex_split(
        &mut self,
        symbols: &mut SymbolTable<'_>,
        mode: ExecMode,
    ) -> Result<&mut Vec<Span>, RegexError> {
        let len = self.raw.len();
        let buf = self.fields.get_or_insert_default();
        buf.clear();
        buf.push(Span::from(0..len)); // $0

        // Aren't iterators beautiful?
        regex::automaton(symbols.fs.to_string().as_bytes(), mode, false)?
            .find_iter(&self.raw)
            .map(|m| m.map(Span::from))
            .process_results(|matches| {
                buf.extend(
                    once(Span::from(0..0))
                        .chain(matches)
                        .chain(once(Span::from(len..len)))
                        .tuple_windows()
                        .map(|(prev, next)| Span::from(prev.end..next.start)),
                );
            })?;

        symbols.nf = Value::Int(buf.len() as isize - 1);
        Ok(buf)
    }

    /// Gets access to the byte span of the `n`th field. Splits if unsplit.
    fn get_raw(
        &mut self,
        n: usize,
        symbols: &mut SymbolTable<'_>,
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
        symbols: &mut SymbolTable<'_>,
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
        symbols: &mut SymbolTable<'_>,
        mode: ExecMode,
    ) -> Result<(), RegexError> {
        let mut fields = take(self.split_fields_raw(symbols, mode)?);
        // If writing out-of-bounds, we grow the record by one OFS each and
        // materialize empty fields. This is partly why re-splitting isn't
        // idempotent, as a regex engine could eat up the consecutive FSs.
        if n >= fields.len() {
            let len = self.raw.len();
            fields.resize(n + 1, Span::from(len..len));
        }

        let mut ofs = SmallVec::<[u8; 16]>::new();
        let _ = write!(ofs, "{}", symbols.ofs);
        let mut buf = Vec::with_capacity(self.raw.len() + val.string_size_hint() + ofs.len());

        for (i, span) in fields.iter_mut().enumerate().skip(1) {
            if i > 1 {
                buf.extend_from_slice(&ofs);
            }

            let start = buf.len();
            if i == n {
                val.write_string(&mut buf);
            } else {
                buf.extend_from_slice(&self.raw()[*span]);
            }
            *span = Span::from(start..buf.len());
        }
        fields[0] = Span::from(0..buf.len());
        symbols.nf = Value::Int(fields.len() as isize - 1);

        self.raw = buf;
        self.fields = Some(fields);
        Ok(())
    }

    /// Grants access to the record's byte slice.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn clear(&mut self) {
        self.raw.clear();
        self.fields = None;
    }

    pub fn write_new(&mut self) -> &mut Vec<u8> {
        self.clear();
        &mut self.raw
    }
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
        assert_eq!(st.nf, Value::Int(0));
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
