// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{
    borrow::Cow,
    cell::RefCell,
    cmp::Ordering,
    fmt::Display,
    hash::{Hash, Hasher},
    hint::cold_path,
    ops::{Add, BitXor, Deref, Div, Mul, Rem, Sub},
    rc::Rc,
};

use ahash::RandomState;
use derive_more::{Debug, Deref, DerefMut, Display, From};
use hashbrown::HashMap;
use minrx::Regex;
use rc_vec::{RcVec, unique_rc::UniqRc};
use smallvec::SmallVec;

use crate::{ExecMode, vm::regex};

#[inline(always)]
const fn likely(b: bool) -> bool {
    if !b {
        cold_path();
    }
    b
}

/// Newtype wrapping the actual type representation. This is so we can ensure
/// we aren't relying on implementation details across the codebase, so
/// changing the type representation details does not have much fallout.
#[derive(Clone, Debug, Display, PartialEq, PartialOrd)]
pub struct Value<'a>(AwkValue<'a>);

#[derive(Clone, Debug)]
#[allow(dead_code)]
enum AwkValue<'a> {
    Float(AwkNum),
    String(Rc<AwkStr>),
    Str(&'a AwkStr),
    Regex(Rc<AwkRegex>),
    StrNum(Rc<AwkStrNum>),
    Array(Rc<RefCell<ArrayMap<'a>>>),
    Bool(AwkBool),
    Int(AwkInt),
    Untyped,
    Unassigned,
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Display, From)]
pub struct AwkNum(f64);

#[repr(transparent)]
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AwkStr([u8]);

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct AwkStrNum {
    num: AwkNum,
    str: AwkStr,
}

#[derive(Debug)]
struct AwkRegex {
    #[debug(skip)]
    automaton: RefCell<Option<Regex>>,
    src: Rc<AwkStr>,
}

#[repr(transparent)]
#[derive(Clone, Debug, Default, Deref, DerefMut)]
pub struct ArrayMap<'a>(HashMap<Vec<u8>, Value<'a>, RandomState>);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Deref, Display, PartialEq, Eq, PartialOrd, Ord, Hash, From)]
pub struct AwkInt(i32);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Deref, Display, PartialEq, Eq, PartialOrd, Ord, Hash, From)]
struct AwkBool(bool);

impl<'a> Value<'a> {
    #[inline(always)]
    pub fn new_num(f: impl Into<AwkNum>) -> Self {
        Self(AwkValue::Float(f.into()))
    }

    #[inline(always)]
    pub fn new_int(i: impl Into<AwkInt>) -> Self {
        Self(AwkValue::Int(i.into()))
    }

    #[inline(always)]
    pub const fn new_str(slice: &'a [u8]) -> Self {
        Self(AwkValue::Str(AwkStr::new(slice)))
    }

    #[inline(always)]
    pub fn new_string(vec: RcVec<u8>) -> Self {
        Self(AwkValue::String(
            AwkStr::new_rc(RcVec::into_uniq_slice(vec)).into(),
        ))
    }

    pub fn new_regex(vec: RcVec<u8>) -> Self {
        Self(AwkValue::Regex(Rc::new(AwkRegex {
            automaton: RefCell::new(None),
            src: AwkStr::new_rc(RcVec::into_uniq_slice(vec)).into(),
        })))
    }

    #[inline(always)]
    pub fn new_array(arr: ArrayMap<'a>) -> Self {
        Self(AwkValue::Array(Rc::new(RefCell::new(arr))))
    }

    #[inline(always)]
    pub const fn new_untyped() -> Self {
        Self(AwkValue::Untyped)
    }

    #[inline(always)]
    pub const fn new_unassigned() -> Self {
        Self(AwkValue::Unassigned)
    }

    #[inline(always)]
    pub fn empty_array() -> Self {
        Self(AwkValue::empty_array())
    }

    #[inline(always)]
    pub fn to_int(&self) -> i32 {
        self.0.to_int()
    }

    #[inline(always)]
    pub fn to_num(&self) -> f64 {
        self.0.to_num()
    }

    #[inline(always)]
    pub fn to_bool(&self) -> bool {
        self.0.to_bool()
    }

    #[inline(always)]
    pub fn as_str(&self) -> Option<&[u8]> {
        self.0.as_str()
    }

    #[inline(always)]
    pub fn scalar_context(&mut self) -> Option<&mut Self> {
        self.0.scalar_context().map(Self::from_ref_mut)
    }

    #[inline(always)]
    pub fn array_context(&mut self) -> Option<&mut Self> {
        self.0.array_context().map(Self::from_ref_mut)
    }

    #[inline(always)]
    pub const fn type_of(&self) -> Self {
        Self(self.0.type_of())
    }

    #[inline(always)]
    pub fn write_string(&self, out: &mut impl VecAdaptor<u8>) {
        self.0.write_string(out);
    }

    #[inline(always)]
    pub fn string_size_hint(&self) -> usize {
        self.0.string_size_hint()
    }

    pub fn to_bytes(&self) -> Cow<'_, [u8]> {
        if let Some(s) = self.as_str() {
            s.into()
        } else {
            let mut buf = Vec::with_capacity(self.string_size_hint());
            self.write_string(&mut buf);
            buf.into()
        }
    }

    pub fn matches_regex(&self, pattern: &Self, mode: ExecMode) -> bool {
        self.0.matches_regex(&pattern.0, mode)
    }

    pub fn array_len(&self) -> Option<usize> {
        self.0.array_len()
    }

    pub const fn is_array(&self) -> bool {
        matches!(self, Self(AwkValue::Array(_)))
    }

    pub fn array_insert(&mut self, key: Vec<u8>, val: Self) -> Option<()> {
        self.0.array_insert(key, val.0)
    }

    pub fn array_remove(&mut self, key: &[u8]) -> Option<()> {
        self.0.array_remove(key)
    }

    pub fn reset_array(&mut self) -> Option<()> {
        self.0.reset_array()
    }

    pub fn get_array_elem(&mut self, key: &[u8]) -> Option<Self> {
        self.0.get_array_elem(key)
    }

    pub fn has_array_elem(&mut self, key: &[u8]) -> Option<bool> {
        self.0.has_array_elem(key)
    }

    pub fn array_elem_aoa(&mut self, key: Vec<u8>) -> Option<Self> {
        self.0.array_elem_aoa(key)
    }

    fn from_ref_mut<'r>(mut val: &'r mut AwkValue<'a>) -> &'r mut Self {
        unsafe { &mut **(&raw mut val).cast::<&'r mut Self>() }
    }
}

impl<'a> AwkValue<'a> {
    /// Called when loading a variable's value. Forces subsequent uses to be
    /// typed as an AWK scalar (anything that's not an array, basically).
    #[inline(always)]
    fn scalar_context(&mut self) -> Option<&mut Self> {
        match self {
            Self::Untyped => *self = Self::Unassigned,
            Self::Array(_) => return None,
            _ => {}
        }
        Some(self)
    }

    #[inline(always)]
    fn array_context(&mut self) -> Option<&mut Self> {
        match self {
            Self::Untyped => *self = Self::empty_array(),
            Self::Array(_) => {}
            _ => return None,
        }
        Some(self)
    }

    pub fn empty_array() -> Self {
        Self::Array(Rc::new(RefCell::new(ArrayMap(HashMap::with_hasher(
            RandomState::new(),
        )))))
    }

    fn to_bool(&self) -> bool {
        match self {
            &Self::Float(AwkNum(f)) => f != 0.,
            &Self::Int(AwkInt(n)) => n != 0,
            &Self::Bool(AwkBool(b)) => b,
            Self::String(str) => !str.is_empty(),
            _ => false,
        }
    }

    fn to_num(&self) -> f64 {
        match self {
            &Self::Float(AwkNum(f)) => f,
            &Self::Int(AwkInt(n)) => n as f64,
            &Self::Bool(AwkBool(b)) => b as usize as f64,
            Self::String(s) => str::from_utf8(s)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.),
            _ => 0.,
        }
    }

    fn to_int(&self) -> i32 {
        if let &Self::Int(AwkInt(int)) = self {
            return int;
        }
        self.to_num().trunc() as i32
    }

    fn as_str(&self) -> Option<&[u8]> {
        match self {
            Self::String(s) => Some(s),
            Self::Str(s) => Some(s),
            Self::Regex(r) => Some(&r.src),
            Self::StrNum(s) => Some(&s.str),
            _ => None,
        }
    }

    fn matches_regex(&self, pattern: &Self, mode: ExecMode) -> bool {
        let mut subject = Vec::with_capacity(self.string_size_hint());
        self.write_string(&mut subject);
        let Self::Regex(pattern) = pattern else {
            todo!("Conversion via lexer!")
        };
        // TODO: icase wiring
        let mut r = pattern.automaton.borrow_mut();
        let r = r.get_or_insert_with(|| regex::automaton(pattern, mode, false).unwrap());
        r.is_match(&subject).unwrap()
    }

    fn as_array(&mut self) -> Option<Rc<RefCell<ArrayMap<'a>>>> {
        let Self::Array(arr) = self.array_context()? else {
            unreachable!("array_context() leaves an Array variant on success")
        };
        Some(Rc::clone(arr))
    }

    fn array_len(&self) -> Option<usize> {
        if let Self::Array(arr) = self {
            Some(arr.borrow().len())
        } else {
            None
        }
    }

    fn array_insert(&mut self, key: Vec<u8>, val: Self) -> Option<()> {
        self.as_array()?.borrow_mut().insert(key, Value(val));
        Some(())
    }

    fn array_remove(&mut self, key: &[u8]) -> Option<()> {
        self.as_array()?.borrow_mut().remove(key);
        Some(())
    }

    fn reset_array(&mut self) -> Option<()> {
        *self.as_array()?.borrow_mut() = ArrayMap::default();
        Some(())
    }

    fn get_array_elem(&mut self, key: &[u8]) -> Option<Value<'a>> {
        self.as_array().map(|arr| {
            arr.borrow()
                .get(key)
                .cloned()
                .unwrap_or(Value::new_untyped())
        })
    }

    fn has_array_elem(&mut self, key: &[u8]) -> Option<bool> {
        self.as_array().map(|arr| arr.borrow().get(key).is_some())
    }

    fn array_elem_aoa(&mut self, key: Vec<u8>) -> Option<Value<'a>> {
        self.as_array().map(|arr| {
            arr.borrow_mut()
                .entry(key)
                .and_modify(|x| {
                    x.array_context();
                })
                .or_insert_with(|| Value::new_array(ArrayMap::default()))
                .clone()
        })
    }

    fn write_string(&self, out: &mut impl VecAdaptor<u8>) {
        if let Some(s) = self.as_str() {
            out.extend_from_slice(s);
            return;
        }
        match self {
            // TODO: NaN and infinities.
            Self::Float(f) => {
                let mut buf = zmij::Buffer::new();
                let s = buf.format(f.0);
                out.extend_from_slice(s.strip_suffix(".0").unwrap_or(s).as_bytes());
            }
            Self::Int(i) => {
                let mut buf = itoa::Buffer::new();
                out.extend_from_slice(buf.format(i.0).as_bytes());
            }
            &Self::Bool(AwkBool(false)) => out.push(b'0'),
            &Self::Bool(AwkBool(true)) => out.push(b'1'),
            Self::Array(_) => panic!("Attempted to use array in scalar context!"),
            _ => {}
        }
    }

    fn string_size_hint(&self) -> usize {
        match self {
            _ if let Some(s) = self.as_str() => s.len(),
            Self::Regex(r) => r.len(),
            Self::Float(_) | Self::Int(_) => 8,
            Self::Bool(_) => 1,
            _ => 0,
        }
    }

    const fn type_of(&self) -> Self {
        let name: &'static [u8] = match self {
            Self::Int(_) | Self::Float(_) | Self::Bool(_) => b"number",
            Self::String(_) | Self::Str(_) => b"string",
            Self::StrNum(_) => b"strnum",
            Self::Regex(_) => b"regexp",
            Self::Array(_) => b"array",
            Self::Untyped => b"untyped",
            Self::Unassigned => b"unassigned",
        };
        Self::Str(AwkStr::new(name))
    }
}

impl AwkStr {
    const fn new<'a>(slice: &'a [u8]) -> &'a Self {
        // SAFETY: it is repr(transparent). There's no safe way around it :/
        unsafe { &*std::ptr::from_ref(&slice).cast::<&'a Self>() }
    }

    fn new_rc(ptr: UniqRc<[u8]>) -> UniqRc<Self> {
        // SAFETY: it is repr(transparent). There's no safe way around it :/
        unsafe { std::mem::transmute::<UniqRc<[u8]>, UniqRc<Self>>(ptr) }
    }
}

impl<'a> Add for &'_ Value<'a> {
    type Output = Value<'a>;

    fn add(self, rhs: Self) -> Self::Output {
        Value::new_num(AwkNum(self.to_num() + rhs.to_num()))
    }
}

impl<'a> Sub for &'_ Value<'a> {
    type Output = Value<'a>;

    fn sub(self, rhs: Self) -> Self::Output {
        Value::new_num(AwkNum(self.to_num() - rhs.to_num()))
    }
}

impl<'a> Mul for &'_ Value<'a> {
    type Output = Value<'a>;

    fn mul(self, rhs: Self) -> Self::Output {
        Value::new_num(AwkNum(self.to_num() * rhs.to_num()))
    }
}

impl<'a> Div for &'_ Value<'a> {
    type Output = Option<Value<'a>>;

    fn div(self, rhs: Self) -> Self::Output {
        let rhs = rhs.to_num();
        likely(rhs != 0.).then(|| Value::new_num(AwkNum(self.to_num() / rhs)))
    }
}

impl<'a> BitXor for &'_ Value<'a> {
    type Output = Value<'a>;

    fn bitxor(self, rhs: Self) -> Self::Output {
        Value::new_num(AwkNum(self.to_num().powf(rhs.to_num())))
    }
}

impl<'a> Rem for &'_ Value<'a> {
    type Output = Option<Value<'a>>;

    fn rem(self, rhs: Self) -> Self::Output {
        let rhs = rhs.to_num();
        likely(rhs != 0.).then(|| Value::new_num(AwkNum(self.to_num() % rhs)))
    }
}

impl PartialEq for AwkValue<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            // Numeric comparisons
            (&Self::Float(AwkNum(lhs)), &Self::Float(AwkNum(rhs))) => lhs == rhs,
            (&Self::Int(AwkInt(lhs)), &Self::Int(AwkInt(rhs))) => lhs == rhs,
            (&Self::Int(AwkInt(lhs)), &Self::Float(AwkNum(rhs)))
            | (&Self::Float(AwkNum(rhs)), &Self::Int(AwkInt(lhs))) => rhs == lhs as f64,
            (&Self::Bool(AwkBool(lhs)), &Self::Bool(AwkBool(rhs))) => lhs == rhs,
            (&Self::Float(AwkNum(f)), &Self::Bool(AwkBool(b)))
            | (&Self::Bool(AwkBool(b)), &Self::Float(AwkNum(f))) => b && f == 1.,
            (&Self::Int(AwkInt(f)), &Self::Bool(AwkBool(b)))
            | (&Self::Bool(AwkBool(b)), &Self::Int(AwkInt(f))) => b && f == 1,
            // String-based comparisons
            _ if let (Some(lhs), Some(rhs)) = (self.as_str(), other.as_str()) => lhs == rhs,
            (&Self::Float(AwkNum(f)), _) if let Some(s) = other.as_str() => {
                f.to_string().as_bytes() == s
            }
            (_, &Self::Float(AwkNum(f))) if let Some(s) = self.as_str() => {
                f.to_string().as_bytes() == s
            }
            (&Self::Int(AwkInt(i)), _) if let Some(s) = other.as_str() => {
                i.to_string().as_bytes() == s
            }
            (_, &Self::Int(AwkInt(i))) if let Some(s) = self.as_str() => {
                i.to_string().as_bytes() == s
            }
            (&Self::Bool(AwkBool(b)), _) if let Some(s) = other.as_str() => {
                (if b { b"1" } else { b"0" }) == s
            }
            (_, &Self::Bool(AwkBool(b))) if let Some(s) = self.as_str() => {
                (if b { b"1" } else { b"0" }) == s
            }
            // True on empty string value.
            (Self::Untyped | Self::Unassigned, _) if let Some(s) = other.as_str() => s.is_empty(),
            (_, Self::Untyped | Self::Unassigned) if let Some(s) = other.as_str() => s.is_empty(),
            (Self::Untyped | Self::Unassigned, Self::Untyped | Self::Unassigned) => true,
            (Self::Untyped | Self::Unassigned, _) | (_, Self::Untyped | Self::Unassigned) => false,
            (Self::Array(_), _) | (_, Self::Array(_)) => {
                panic!("Attempted to use array in scalar context!")
            }
            _ => unreachable!(),
        }
    }
}

impl PartialOrd for AwkValue<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            // Numeric comparisons
            (&Self::Float(AwkNum(lhs)), Self::Float(AwkNum(rhs))) => lhs.partial_cmp(rhs),
            (&Self::Int(AwkInt(lhs)), Self::Int(AwkInt(rhs))) => lhs.partial_cmp(rhs),
            (&Self::Int(AwkInt(lhs)), Self::Float(AwkNum(rhs))) => (lhs as f64).partial_cmp(rhs),
            (Self::Float(AwkNum(lhs)), &Self::Int(AwkInt(rhs))) => lhs.partial_cmp(&(rhs as f64)),
            (&Self::Bool(AwkBool(lhs)), Self::Bool(AwkBool(rhs))) => lhs.partial_cmp(rhs),
            (&Self::Float(AwkNum(f)), &Self::Bool(AwkBool(b))) => {
                f.partial_cmp(&(b as usize as f64))
            }
            (&Self::Bool(AwkBool(b)), Self::Float(AwkNum(f))) => (b as usize as f64).partial_cmp(f),
            (&Self::Int(AwkInt(n)), &Self::Bool(AwkBool(b))) => n.partial_cmp(&(b as i32)),
            (&Self::Bool(AwkBool(b)), Self::Int(AwkInt(f))) => (b as i32).partial_cmp(f),
            // String-based comparisons
            (_, _) if let (Some(lhs), Some(rhs)) = (self.as_str(), other.as_str()) => {
                lhs.as_ref().partial_cmp(rhs)
            }
            (&Self::Float(AwkNum(f)), _) if let Some(s) = other.as_str() => {
                f.to_string().as_bytes().partial_cmp(s)
            }
            (_, &Self::Float(AwkNum(f))) if let Some(s) = self.as_str() => {
                s.partial_cmp(f.to_string().as_bytes())
            }
            (&Self::Int(AwkInt(i)), _) if let Some(s) = other.as_str() => {
                i.to_string().as_bytes().partial_cmp(s)
            }
            (_, &Self::Int(AwkInt(i))) if let Some(s) = self.as_str() => {
                s.partial_cmp(i.to_string().as_bytes())
            }
            (&Self::Bool(AwkBool(b)), _) if let Some(s) = other.as_str() => {
                (if b { b"1" } else { b"0" }).as_ref().partial_cmp(s)
            }
            (_, &Self::Bool(AwkBool(b))) if let Some(s) = self.as_str() => {
                s.as_ref().partial_cmp(if b { b"1" } else { b"0" })
            }
            (Self::Untyped | Self::Unassigned, _) if let Some(s) = other.as_str() => {
                b"".as_ref().partial_cmp(s)
            }
            (_, Self::Untyped | Self::Unassigned) if let Some(s) = self.as_str() => {
                s.as_ref().partial_cmp(b"")
            }
            (Self::Array(_), _) | (_, Self::Array(_)) => {
                panic!("Attempted to use array in scalar context!")
            }
            (Self::Untyped | Self::Unassigned, Self::Untyped | Self::Unassigned) => {
                b"".partial_cmp(b"")
            }
            // Copying comparisons
            (lhs, rhs) => {
                let mut str_buf: Vec<u8> = Vec::new();
                str_buf.reserve_exact(lhs.string_size_hint() + rhs.string_size_hint());
                lhs.write_string(&mut str_buf);
                let midpoint = str_buf.len();
                rhs.write_string(&mut str_buf);
                str_buf[0..midpoint].partial_cmp(&str_buf[midpoint..])
            }
        }
    }
}

impl Eq for AwkNum {}
#[allow(clippy::derive_ord_xor_partial_ord)]
impl Ord for AwkNum {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.partial_cmp(&other.0).unwrap_or(Ordering::Less)
    }
}

impl Hash for AwkNum {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.0.to_bits());
    }
}

impl Display for AwkValue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            AwkValue::Int(n) => <_ as Display>::fmt(n, f),
            AwkValue::Float(n) => <_ as Display>::fmt(n, f),
            AwkValue::String(s) => write!(f, "{}", String::from_utf8_lossy(s)),
            AwkValue::Str(s) => write!(f, "{}", String::from_utf8_lossy(s)),
            AwkValue::StrNum(s) => write!(f, "{}", String::from_utf8_lossy(&s.str)),
            AwkValue::Regex(s) => write!(f, "/{}/", String::from_utf8_lossy(s)),
            &AwkValue::Bool(AwkBool(b)) => write!(f, "{}", b as usize),
            AwkValue::Array(_) | AwkValue::Untyped | AwkValue::Unassigned => Ok(()),
        }
    }
}

impl Deref for AwkStr {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Deref for AwkRegex {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.src
    }
}

impl From<f64> for Value<'_> {
    fn from(value: f64) -> Self {
        Value::new_num(value)
    }
}

impl From<bool> for Value<'_> {
    fn from(value: bool) -> Self {
        Value::new_num(value as usize as f64)
    }
}

impl From<i32> for Value<'_> {
    fn from(value: i32) -> Self {
        Value::new_int(value)
    }
}

impl From<RcVec<u8>> for Value<'_> {
    fn from(value: RcVec<u8>) -> Self {
        Value::new_string(value)
    }
}

pub trait VecAdaptor<T> {
    fn push(&mut self, e: T);
    fn extend_from_slice(&mut self, slice: &[T]);
}

impl VecAdaptor<u8> for Vec<u8> {
    #[inline(always)]
    fn push(&mut self, e: u8) {
        self.push(e);
    }

    #[inline(always)]
    fn extend_from_slice(&mut self, slice: &[u8]) {
        self.extend_from_slice(slice);
    }
}

impl VecAdaptor<u8> for RcVec<u8> {
    #[inline(always)]
    fn push(&mut self, e: u8) {
        self.push(e);
    }
    #[inline(always)]
    fn extend_from_slice(&mut self, slice: &[u8]) {
        self.extend_from_slice(slice);
    }
}

impl<const N: usize> VecAdaptor<u8> for SmallVec<[u8; N]> {
    #[inline(always)]
    fn push(&mut self, e: u8) {
        self.push(e);
    }

    #[inline(always)]
    fn extend_from_slice(&mut self, slice: &[u8]) {
        self.extend_from_slice(slice);
    }
}
