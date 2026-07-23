// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

use std::{
    borrow::Cow,
    cell::RefCell,
    fmt::Display,
    hash::Hash,
    io::Write,
    mem::discriminant,
    ops::{Add, BitXor, Div, Mul, Rem, Sub},
    rc::Rc,
};

use ahash::RandomState;
use hashbrown::HashMap;

/// Shared array storage. AWK arrays are reference-counted (assignment of array
/// names is not a deep copy); `Rc`/`RefCell` is enough while the VM is single-threaded.
pub type ArrayMap<'a> = HashMap<String, Value<'a>, RandomState>;

#[derive(Debug, Clone)]
pub enum Value<'a> {
    Int(isize),
    Float(f64),
    String(Cow<'a, [u8]>),
    Regex(Cow<'a, [u8]>),
    Array(Rc<RefCell<ArrayMap<'a>>>),
    Bool(bool),
    Untyped,
    Unassigned,
}

impl Value<'_> {
    pub fn empty_array() -> Self {
        Self::Array(Rc::new(RefCell::new(HashMap::with_hasher(
            RandomState::new(),
        ))))
    }
}

impl Value<'_> {
    /// Called when loading a variable's value. Forces subsequent uses to be
    /// typed as an AWK scalar (anything that's not an array, basically).
    pub fn scalar_context(&mut self) -> &mut Self {
        // TODO: Exit "nicely" on Self::Array(_).
        match self {
            Self::Untyped => *self = Self::Unassigned,
            Self::Array(_) => panic!("Attempted to use array in scalar context!"),
            _ => {}
        }
        self
    }

    pub fn array_context(&mut self) -> &mut Self {
        // TODO: Exit "nicely" on non-array scalars.
        match self {
            Self::Untyped => *self = Self::empty_array(),
            Self::Array(_) => {}
            _ => panic!("Attempted to use scalar as array!"),
        }
        self
    }

    pub fn to_bool(&self) -> bool {
        match self {
            &Self::Float(f) => f != 0.,
            &Self::Int(n) => n != 0,
            &Self::Bool(b) => b,
            Self::String(str) => !str.is_empty(),
            _ => false,
        }
    }

    pub fn to_num(&self) -> f64 {
        match self {
            &Self::Float(f) => f,
            &Self::Int(n) => n as f64,
            &Self::Bool(b) => b as usize as f64,
            Self::String(s) => str::from_utf8(s)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.),
            _ => 0.,
        }
    }

    pub fn to_int(&self) -> isize {
        if let &Self::Int(int) = self {
            return int;
        }
        self.to_num().trunc() as isize
    }

    pub fn b2f(b: bool) -> Self {
        Self::Float(b as usize as f64)
    }

    // FIXME: gawk supports regex extensions beyond the `regex` crate; compile patterns
    // once and route matching through a dedicated layer (likely regex-automata).
    pub(crate) fn matches_regex(&self, pattern: &[u8]) -> bool {
        let mut subject = Vec::with_capacity(self.string_size_hint());
        self.write_string(&mut subject);
        let Ok(subject) = str::from_utf8(&subject) else {
            return false;
        };
        let Ok(pattern) = str::from_utf8(pattern) else {
            return false;
        };
        regex::Regex::new(pattern).is_ok_and(|re| re.is_match(subject))
    }

    pub fn write_string(&self, f: &mut Vec<u8>) {
        match self {
            Self::String(s) | Self::Regex(s) => f.extend_from_slice(s),
            Self::Float(n) => write!(f, "{n}").unwrap(),
            Self::Int(n) => write!(f, "{n}").unwrap(),
            &Self::Bool(false) => f.push(b'0'),
            &Self::Bool(true) => f.push(b'1'),
            Self::Array(_) => panic!("Attempted to use array in scalar context!"),
            _ => {}
        }
    }

    pub fn string_size_hint(&self) -> usize {
        match self {
            Self::String(s) | Self::Regex(s) => s.len(),
            Self::Float(_) | Self::Int(_) => 8,
            Self::Bool(_) => 1,
            _ => 0,
        }
    }
}

impl<'a> Add for &'_ Value<'a> {
    type Output = Value<'a>;

    fn add(self, rhs: Self) -> Self::Output {
        Value::Float(self.to_num() + rhs.to_num())
    }
}

impl<'a> Sub for &'_ Value<'a> {
    type Output = Value<'a>;

    fn sub(self, rhs: Self) -> Self::Output {
        Value::Float(self.to_num() - rhs.to_num())
    }
}

impl<'a> Mul for &'_ Value<'a> {
    type Output = Value<'a>;

    fn mul(self, rhs: Self) -> Self::Output {
        Value::Float(self.to_num() * rhs.to_num())
    }
}

impl<'a> Div for &'_ Value<'a> {
    type Output = Value<'a>;

    fn div(self, rhs: Self) -> Self::Output {
        let rhs = rhs.to_num();
        // TODO: panic "nicely" on div by zero.
        assert!(rhs != 0., "Division by zero attempted in '/'!");
        Value::Float(self.to_num() / rhs)
    }
}

impl<'a> BitXor for &'_ Value<'a> {
    type Output = Value<'a>;

    fn bitxor(self, rhs: Self) -> Self::Output {
        Value::Float(self.to_num().powf(rhs.to_num()))
    }
}

impl<'a> Rem for &'_ Value<'a> {
    type Output = Value<'a>;

    fn rem(self, rhs: Self) -> Self::Output {
        let (lhs, rhs) = (self.to_num(), rhs.to_num());
        // TODO: panic "nicely" on div by zero.
        assert!(lhs != 0. || rhs != 0., "Division by zero attempted in '%'!");
        Value::Float(lhs % rhs)
    }
}

impl PartialEq for Value<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            // Numeric comparisons
            (&Self::Float(lhs), &Self::Float(rhs)) => lhs == rhs,
            (&Self::Int(lhs), &Self::Int(rhs)) => lhs == rhs,
            (&Self::Int(lhs), &Self::Float(rhs)) | (&Self::Float(rhs), &Self::Int(lhs)) => {
                rhs == lhs as f64
            }
            (&Self::Bool(lhs), &Self::Bool(rhs)) => lhs == rhs,
            (&Self::Float(f), &Self::Bool(b)) | (&Self::Bool(b), &Self::Float(f)) => b && f == 1.,
            (&Self::Int(f), &Self::Bool(b)) | (&Self::Bool(b), &Self::Int(f)) => b && f == 1,
            // String-based comparisons
            (Self::String(lhs) | Self::Regex(lhs), Self::String(rhs) | Self::Regex(rhs)) => {
                lhs == rhs
            }
            (&Self::Float(f), Self::String(s) | Self::Regex(s))
            | (Self::String(s) | Self::Regex(s), &Self::Float(f)) => {
                f.to_string().as_bytes() == s.as_ref()
            }
            (&Self::Int(f), Self::String(s) | Self::Regex(s))
            | (Self::String(s) | Self::Regex(s), &Self::Int(f)) => {
                f.to_string().as_bytes() == s.as_ref()
            }
            (&Self::Bool(b), Self::String(s) | Self::Regex(s))
            | (Self::String(s) | Self::Regex(s), &Self::Bool(b)) => {
                (if b { b"1" } else { b"0" }) == s.as_ref()
            }
            // True on empty string value.
            (Self::Untyped | Self::Unassigned, Self::String(s) | Self::Regex(s))
            | (Self::String(s) | Self::Regex(s), Self::Untyped | Self::Unassigned) => s.is_empty(),
            (Self::Untyped | Self::Unassigned, Self::Untyped | Self::Unassigned) => true,
            (Self::Untyped | Self::Unassigned, _) | (_, Self::Untyped | Self::Unassigned) => false,
            (Self::Array(_), _) | (_, Self::Array(_)) => {
                panic!("Attempted to use array in scalar context!")
            }
        }
    }
}

impl PartialOrd for Value<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            // Numeric comparisons
            (&Self::Float(lhs), Self::Float(rhs)) => lhs.partial_cmp(rhs),
            (&Self::Int(lhs), Self::Int(rhs)) => lhs.partial_cmp(rhs),
            (&Self::Int(lhs), Self::Float(rhs)) => (lhs as f64).partial_cmp(rhs),
            (Self::Float(lhs), &Self::Int(rhs)) => lhs.partial_cmp(&(rhs as f64)),
            (&Self::Bool(lhs), Self::Bool(rhs)) => lhs.partial_cmp(rhs),
            (&Self::Float(f), &Self::Bool(b)) => f.partial_cmp(&(b as usize as f64)),
            (&Self::Bool(b), Self::Float(f)) => (b as usize as f64).partial_cmp(f),
            (&Self::Int(n), &Self::Bool(b)) => n.partial_cmp(&(b as isize)),
            (&Self::Bool(b), Self::Int(f)) => (b as isize).partial_cmp(f),
            // String-based comparisons
            (Self::String(lhs) | Self::Regex(lhs), Self::String(rhs) | Self::Regex(rhs)) => {
                lhs.as_ref().partial_cmp(rhs)
            }
            (&Self::Float(f), Self::String(s) | Self::Regex(s)) => {
                f.to_string().as_bytes().partial_cmp(s)
            }
            (&Self::Int(n), Self::String(s) | Self::Regex(s)) => {
                n.to_string().as_bytes().partial_cmp(s)
            }
            (Self::String(s) | Self::Regex(s), &Self::Float(f)) => {
                s.as_ref().partial_cmp(f.to_string().as_bytes())
            }
            (Self::String(s) | Self::Regex(s), &Self::Int(n)) => {
                s.as_ref().partial_cmp(n.to_string().as_bytes())
            }
            (&Self::Bool(b), Self::String(s) | Self::Regex(s)) => {
                (if b { b"1" } else { b"0" }).as_ref().partial_cmp(s)
            }
            (Self::String(s) | Self::Regex(s), &Self::Bool(b)) => {
                s.as_ref().partial_cmp(if b { b"1" } else { b"0" })
            }
            (Self::Untyped | Self::Unassigned, Self::String(s) | Self::Regex(s)) => {
                b"".as_ref().partial_cmp(s)
            }
            (Self::String(s) | Self::Regex(s), Self::Untyped | Self::Unassigned) => {
                s.as_ref().partial_cmp(b"")
            }
            (Self::Array(_), _) | (_, Self::Array(_)) => {
                panic!("Attempted to use array in scalar context!")
            }
            (Self::Untyped | Self::Unassigned, Self::Untyped | Self::Unassigned) => {
                b"".partial_cmp(b"")
            }
            // Copyful comparisons
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

impl Eq for Value<'_> {}
impl Hash for Value<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        discriminant(self).hash(state);
        match self {
            &Self::Int(n) => state.write_isize(n),
            Self::Float(f) => state.write_u64(f.to_bits()),
            Self::String(s) => s.hash(state),
            Self::Bool(b) => b.hash(state),
            _ => {}
        }
    }
}

impl Display for Value<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int(n) => <_ as Display>::fmt(n, f),
            Value::Float(n) => <_ as Display>::fmt(n, f),
            Value::String(s) => write!(f, "{}", String::from_utf8_lossy(s)),
            Value::Regex(s) => write!(f, "/{}/", String::from_utf8_lossy(s)),
            &Value::Bool(b) => write!(f, "{}", b as usize),
            Value::Array(_) | Value::Untyped | Value::Unassigned => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_clone_shares_backing_storage() {
        let a = Value::empty_array();
        let b = a.clone();
        match (&a, &b) {
            (Value::Array(left), Value::Array(right)) => {
                left.borrow_mut().insert("k".into(), Value::Int(1));
                assert_eq!(right.borrow().get("k"), Some(&Value::Int(1)));
            }
            _ => panic!("expected Array values"),
        }
    }
}
