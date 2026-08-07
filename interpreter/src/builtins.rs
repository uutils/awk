// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! Built-in function implementations for the VM.
//!
//! Call sites lower to [`crate::ir::Instruction::IntrinsicCall`]; the VM passes
//! argument registers as a slice via [`Registers::get_range`](crate::vm::Registers)
//! and dispatches through [`Interpreter::call_builtin`]. Arity overloading
//! (e.g. `length`, `substr`, `and`) is handled per function.
//!
//! Behavior follows the [gawk built-in function](https://www.gnu.org/software/gawk/manual/html_node/Built_002din.html)
//! documentation. Non-trivial builtins are stubbed with `todo!` until implemented.

use parser::{AriadneSpan, BuiltinFunction};

use crate::{
    InterpreterError,
    vm::{Interpreter, types::Value},
};

/// Bit width used by gawk bitwise ops on ordinary (non-MPFR) numbers.
const BIT_MASK: u64 = (1u64 << 53) - 1;

#[derive(Debug)]
pub(crate) enum BuiltinError {
    /// Wrong number of arguments; `expected` is the arity bound shown to the user.
    Arity { expected: u8, given: u8 },
}

impl BuiltinError {
    pub(crate) fn into_interpreter_error(self, span: AriadneSpan) -> InterpreterError {
        match self {
            Self::Arity { expected, given } => {
                InterpreterError::ArityMismatch(span, expected, given)
            }
        }
    }
}

impl<'a> Interpreter<'a> {
    /// Dispatch a built-in function over `args`.
    ///
    /// Kept on [`Interpreter`] so future builtins can use `ExecMode`, I/O, and
    /// other VM state without reshaping the call site. Currently `&self` is
    /// enough; switch to `&mut self` (and copy/own args at the call site) when
    /// a builtin needs to mutate the VM.
    pub(crate) fn call_builtin(
        &self,
        fun: BuiltinFunction,
        args: &[Value<'a>],
    ) -> Result<Value<'a>, BuiltinError> {
        match fun {
            BuiltinFunction::Int => Ok(Value::Float(require_args(args, 1, 1)?[0].to_int() as f64)),
            BuiltinFunction::Sqrt => Ok(Value::Float(require_args(args, 1, 1)?[0].to_num().sqrt())),
            BuiltinFunction::Exp => Ok(Value::Float(require_args(args, 1, 1)?[0].to_num().exp())),
            BuiltinFunction::Log => Ok(Value::Float(require_args(args, 1, 1)?[0].to_num().ln())),
            BuiltinFunction::Sin => Ok(Value::Float(require_args(args, 1, 1)?[0].to_num().sin())),
            BuiltinFunction::Cos => Ok(Value::Float(require_args(args, 1, 1)?[0].to_num().cos())),
            BuiltinFunction::Atan2 => {
                let args = require_args(args, 2, 2)?;
                Ok(Value::Float(args[0].to_num().atan2(args[1].to_num())))
            }
            BuiltinFunction::Length => self.builtin_length(args),
            BuiltinFunction::Index => index(args),
            BuiltinFunction::Substr => substr(args),
            BuiltinFunction::Toupper => Ok(map_string(require_args(args, 1, 1)?, |b| {
                b.to_ascii_uppercase()
            })),
            BuiltinFunction::Tolower => Ok(map_string(require_args(args, 1, 1)?, |b| {
                b.to_ascii_lowercase()
            })),
            BuiltinFunction::And => bitwise_variadic(args, |a, b| a & b),
            BuiltinFunction::Or => bitwise_variadic(args, |a, b| a | b),
            BuiltinFunction::Xor => bitwise_variadic(args, |a, b| a ^ b),
            BuiltinFunction::Compl => {
                let n = to_bits(&require_args(args, 1, 1)?[0]);
                Ok(Value::Float((BIT_MASK ^ n) as f64))
            }
            BuiltinFunction::Lshift => shift(args, true),
            BuiltinFunction::Rshift => shift(args, false),
            BuiltinFunction::Strtonum => Ok(Value::Float(strtonum(&require_args(args, 1, 1)?[0]))),
            BuiltinFunction::Typeof => Ok(typeof_value(&require_args(args, 1, 1)?[0])),
            BuiltinFunction::Isarray => {
                let v = &require_args(args, 1, 1)?[0];
                Ok(Value::Int(matches!(v, Value::Array(_)) as isize))
            }
            // Placeholders — call glue and dispatch exist; bodies come later.
            BuiltinFunction::Split
            | BuiltinFunction::Sub
            | BuiltinFunction::Gsub
            | BuiltinFunction::Match
            | BuiltinFunction::Sprintf
            | BuiltinFunction::Gensub
            | BuiltinFunction::Patsplit
            | BuiltinFunction::Close
            | BuiltinFunction::Fflush
            | BuiltinFunction::System
            | BuiltinFunction::Rand
            | BuiltinFunction::Srand
            | BuiltinFunction::Systime
            | BuiltinFunction::Mktime
            | BuiltinFunction::Strftime
            | BuiltinFunction::Asort
            | BuiltinFunction::Asorti => todo!("built-in {fun}"),
        }
    }

    fn builtin_length(&self, args: &[Value<'a>]) -> Result<Value<'a>, BuiltinError> {
        match args {
            [] => {
                // `length()` — length of `$0`. Unassigned/`$0` before input → 0.
                Ok(Value::Float(
                    value_length(self.symbols.record(Value::Int(0))) as f64,
                ))
            }
            [v] => Ok(Value::Float(value_length(v) as f64)),
            _ => Err(BuiltinError::Arity { expected: 1, given: args.len() as u8 }),
        }
    }
}

fn require_args<'a, 'b>(
    args: &'b [Value<'a>],
    min: u8,
    max: u8,
) -> Result<&'b [Value<'a>], BuiltinError> {
    let given = args.len() as u8;
    if given < min || given > max {
        let expected = if given > max { max } else { min };
        return Err(BuiltinError::Arity { expected, given });
    }
    Ok(args)
}

fn value_length(v: &Value<'_>) -> usize {
    match v {
        Value::Array(arr) => arr.borrow().len(),
        other => {
            let mut buf = Vec::new();
            other.write_string(&mut buf);
            buf.len()
        }
    }
}

fn index<'a>(args: &[Value<'a>]) -> Result<Value<'a>, BuiltinError> {
    let args = require_args(args, 2, 2)?;
    let hay = value_bytes(&args[0]);
    let needle = value_bytes(&args[1]);
    if needle.is_empty() {
        return Ok(Value::Float(1.));
    }
    let pos = hay
        .windows(needle.len())
        .position(|w| w == needle.as_slice())
        .map_or(0, |i| i + 1);
    Ok(Value::Float(pos as f64))
}

fn substr<'a>(args: &[Value<'a>]) -> Result<Value<'a>, BuiltinError> {
    let args = require_args(args, 2, 3)?;
    let s = value_bytes(&args[0]);
    let start = args[1].to_int();
    // gawk/POSIX: start < 1 is treated as 1.
    let start_idx = if start <= 0 {
        0
    } else {
        (start as usize).saturating_sub(1)
    };
    if start_idx >= s.len() {
        return Ok(Value::String(b"".into()));
    }
    let end = if let Some(n) = args.get(2) {
        let n = n.to_int();
        if n <= 0 {
            start_idx
        } else {
            (start_idx + n as usize).min(s.len())
        }
    } else {
        s.len()
    };
    Ok(Value::String(s[start_idx..end].to_vec().into()))
}

fn map_string<'a>(args: &[Value<'a>], map: impl Fn(u8) -> u8) -> Value<'a> {
    let mut buf = value_bytes(&args[0]);
    for b in &mut buf {
        *b = map(*b);
    }
    Value::String(buf.into())
}

fn bitwise_variadic<'a>(
    args: &[Value<'a>],
    op: impl Fn(u64, u64) -> u64,
) -> Result<Value<'a>, BuiltinError> {
    let args = require_args(args, 2, u8::MAX)?;
    let mut acc = to_bits(&args[0]);
    for arg in &args[1..] {
        acc = op(acc, to_bits(arg)) & BIT_MASK;
    }
    Ok(Value::Float(acc as f64))
}

fn shift<'a>(args: &[Value<'a>], left: bool) -> Result<Value<'a>, BuiltinError> {
    let args = require_args(args, 2, 2)?;
    let shift = args[1].to_int();
    if shift < 0 {
        // FIXME: gawk fatals on negative shift counts; wire a proper runtime error.
        return Ok(Value::Float(0.));
    }
    let shift = shift as u32;
    let n = to_bits(&args[0]);
    let result = if shift >= 64 {
        0
    } else if left {
        (n << shift) & BIT_MASK
    } else {
        n >> shift
    };
    Ok(Value::Float(result as f64))
}

fn to_bits(v: &Value<'_>) -> u64 {
    let n = v.to_num();
    // FIXME: gawk fatals on negative bitwise operands; do not coerce to 0.
    if !n.is_finite() || n < 0. {
        return 0;
    }
    (n.trunc() as u64) & BIT_MASK
}

fn strtonum(v: &Value<'_>) -> f64 {
    let bytes = value_bytes(v);
    let Ok(s) = std::str::from_utf8(&bytes) else {
        return 0.;
    };
    let s = s.trim_start();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).map_or(0., |n| n as f64);
    }
    if s.len() > 1 && s.starts_with('0') && s.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        // Leading zero and only octal digits → octal, as in gawk `strtonum`.
        return u64::from_str_radix(s, 8).map_or(0., |n| n as f64);
    }
    s.parse().unwrap_or(0.)
}

fn typeof_value<'a>(v: &Value<'_>) -> Value<'a> {
    let name: &[u8] = match v {
        Value::Int(_) | Value::Float(_) | Value::Bool(_) => b"number",
        Value::String(_) => b"string",
        Value::Regex(_) => b"regexp",
        Value::Array(_) => b"array",
        Value::Untyped => b"untyped",
        Value::Unassigned => b"unassigned",
    };
    Value::String(name.into())
}

fn value_bytes(v: &Value<'_>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(v.string_size_hint());
    v.write_string(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use bumpalo::Bump;
    use parser::MetadataStore;

    use super::*;
    use crate::{ExecMode, ir::lower::CodeGen, vm::types::Value};

    fn with_interp(f: impl FnOnce(&mut Interpreter<'_>)) {
        let arena = Bump::new();
        let cg = CodeGen::new(&arena);
        let mut interp = Interpreter::new(ExecMode::Uu, cg, MetadataStore::new());
        f(&mut interp);
    }

    #[test]
    fn int_truncates_toward_zero() {
        with_interp(|intrp| {
            assert_eq!(
                intrp
                    .call_builtin(BuiltinFunction::Int, &[Value::Float(3.7)])
                    .unwrap()
                    .to_num(),
                3.
            );
            assert_eq!(
                intrp
                    .call_builtin(BuiltinFunction::Int, &[Value::Float(-3.7)])
                    .unwrap()
                    .to_num(),
                -3.
            );
        });
    }

    #[test]
    fn length_of_string_and_empty_record() {
        with_interp(|intrp| {
            assert_eq!(
                intrp
                    .call_builtin(BuiltinFunction::Length, &[Value::String(b"abc".into())])
                    .unwrap()
                    .to_num(),
                3.
            );
            assert_eq!(
                intrp
                    .call_builtin(BuiltinFunction::Length, &[])
                    .unwrap()
                    .to_num(),
                0.
            );
        });
    }

    #[test]
    fn index_and_substr() {
        with_interp(|intrp| {
            assert_eq!(
                intrp
                    .call_builtin(
                        BuiltinFunction::Index,
                        &[
                            Value::String(b"foobar".into()),
                            Value::String(b"bar".into())
                        ],
                    )
                    .unwrap()
                    .to_num(),
                4.
            );
            let s = intrp
                .call_builtin(
                    BuiltinFunction::Substr,
                    &[
                        Value::String(b"abcdef".into()),
                        Value::Int(2),
                        Value::Int(3),
                    ],
                )
                .unwrap();
            let mut buf = Vec::new();
            s.write_string(&mut buf);
            assert_eq!(buf, b"bcd");
        });
    }

    #[test]
    fn bitwise_and_or_xor_compl() {
        with_interp(|intrp| {
            assert_eq!(
                intrp
                    .call_builtin(BuiltinFunction::And, &[Value::Int(7), Value::Int(3)])
                    .unwrap()
                    .to_num(),
                3.
            );
            assert_eq!(
                intrp
                    .call_builtin(
                        BuiltinFunction::Or,
                        &[Value::Int(1), Value::Int(2), Value::Int(4)],
                    )
                    .unwrap()
                    .to_num(),
                7.
            );
            assert_eq!(
                intrp
                    .call_builtin(BuiltinFunction::Xor, &[Value::Int(7), Value::Int(3)])
                    .unwrap()
                    .to_num(),
                4.
            );
            assert_eq!(
                intrp
                    .call_builtin(BuiltinFunction::Compl, &[Value::Int(0)])
                    .unwrap()
                    .to_num(),
                BIT_MASK as f64
            );
        });
    }

    #[test]
    fn arity_mismatch_is_reported() {
        with_interp(|intrp| {
            let err = intrp
                .call_builtin(BuiltinFunction::And, &[Value::Int(1)])
                .unwrap_err();
            assert!(matches!(err, BuiltinError::Arity { expected: 2, given: 1 }));
        });
    }

    #[test]
    fn builtin_error_converts_with_span() {
        let err = BuiltinError::Arity { expected: 2, given: 1 };
        let span = AriadneSpan(parser::FileCache(None), (0..1).into());
        assert!(matches!(
            err.into_interpreter_error(span),
            InterpreterError::ArityMismatch(_, 2, 1)
        ));
    }

    #[ignore = "FIXME: gawk fatals on negative shift counts"]
    #[test]
    fn negative_shift_is_fatal() {
        with_interp(|intrp| {
            let err = intrp.call_builtin(BuiltinFunction::Lshift, &[Value::Int(1), Value::Int(-1)]);
            assert!(
                err.is_err(),
                "expected fatal for negative shift, got {err:?}"
            );
        });
    }

    #[ignore = "FIXME: gawk fatals on negative bitwise operands"]
    #[test]
    fn negative_bitwise_operand_is_fatal() {
        with_interp(|intrp| {
            let err = intrp.call_builtin(BuiltinFunction::And, &[Value::Int(-1), Value::Int(1)]);
            assert!(
                err.is_err(),
                "expected fatal for negative bitwise operand, got {err:?}"
            );
        });
    }
}
