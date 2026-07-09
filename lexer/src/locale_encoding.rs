// This file is part of the uutils awk package.
//
// For the full copyright and license information, please view the LICENSE
// files that was distributed with this source code.

//! Locale-aware encoding for `\u` escape sequences, matching gawk behavior.
//!
//! See: <https://www.gnu.org/software/gawk/manual/html_node/Escape-Sequences.html>

use encoding_rs::{EncoderResult, Encoding, UTF_8};

/// Character encoding derived from the process locale (`LC_*` / `LANG`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocaleEncoding {
    encoding: &'static Encoding,
    /// `C` / `POSIX` locales only accept ASCII via `\u`.
    ascii_only: bool,
}

impl LocaleEncoding {
    pub fn utf8() -> Self {
        Self { encoding: UTF_8, ascii_only: false }
    }

    /// `C` / `POSIX` locale: `\u` only encodes code points in ASCII.
    pub fn ascii() -> Self {
        Self { encoding: UTF_8, ascii_only: true }
    }

    /// ISO-8859-1 (Latin-1).
    pub fn iso_8859_1() -> Self {
        Self {
            encoding: Encoding::for_label(b"iso-8859-1").unwrap_or(UTF_8),
            ascii_only: false,
        }
    }

    /// Detect encoding from `LC_ALL`, `LC_CTYPE`, or `LANG`.
    pub fn detect() -> Self {
        for var in ["LC_ALL", "LC_CTYPE", "LANG"] {
            if let Some(os) = std::env::var_os(var)
                && let Some(name) = os.to_str()
            {
                return Self::from_locale_name(name);
            }
        }
        Self::from_locale_name("C.UTF-8")
    }

    /// Parse a locale name such as `en_US.UTF-8` or `C`.
    pub fn from_locale_name(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        let extension = lower.rsplit_once('.').map(|(_, ext)| ext);
        let is_ascii = |c: &str| matches!(c, "c" | "posix");
        if is_ascii(&lower) || extension.is_some_and(is_ascii) {
            return Self::ascii();
        }

        let charset = name.rsplit('.').next().unwrap_or(name);
        let label = charset.to_ascii_lowercase().replace('_', "-");

        if label.contains("utf-8") || label == "utf8" {
            return Self::utf8();
        }

        if let Some(encoding) = Encoding::for_label(label.as_bytes()) {
            Self { encoding, ascii_only: false }
        } else {
            Self::utf8()
        }
    }

    /// Encode a Unicode scalar value for a `\u` escape in the current locale.
    ///
    /// Invalid code points and characters that cannot be represented in the
    /// locale encoding become `?`, matching gawk.
    pub fn encode_unicode_escape(self, codepoint: u32, out: &mut impl Extend<u8>) {
        let c = match char::try_from(codepoint) {
            Ok(c) if !self.ascii_only || c.is_ascii() => c,
            _ => {
                out.extend(*b"?");
                return;
            }
        };

        if self.encoding == UTF_8 && !self.ascii_only {
            let mut buf = [0u8; 4];
            out.extend(c.encode_utf8(&mut buf).bytes());
            return;
        }

        let mut encoder = self.encoding.new_encoder();
        let mut buf = [0u8; 8];
        let mut utf8 = [0u8; 4];
        let ch = c.encode_utf8(&mut utf8);
        match encoder.encode_from_utf8_without_replacement(ch, &mut buf, true) {
            (EncoderResult::InputEmpty, _, written) if written > 0 => {
                out.extend(buf[..written].iter().copied());
            }
            _ => out.extend(*b"?"),
        }
    }
}

impl Default for LocaleEncoding {
    fn default() -> Self {
        Self::utf8()
    }
}
