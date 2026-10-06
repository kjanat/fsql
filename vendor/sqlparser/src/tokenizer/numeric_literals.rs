//! Local numeric-literal extensions, enabled exclusively through dialect hooks.

use super::{Location, State, Token, Tokenizer, TokenizerError, peeking_take_while};
#[cfg(not(feature = "std"))]
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

impl Tokenizer<'_> {
    pub(super) fn tokenize_octal_literal(
        &self,
        chars: &mut State,
        start: Location,
    ) -> Result<Option<Token>, TokenizerError> {
        let prefix = chars.next().expect("octal prefix");
        // Consume the entire identifier-like tail so malformed digits cannot become aliases.
        let digits = peeking_take_while(chars, |ch| {
            ch.is_ascii_alphanumeric() || ch == '_' || self.dialect.is_identifier_part(ch)
        });
        if digits.is_empty() {
            return self.tokenizer_error(start, format!("Expected octal digits after 0{prefix}"));
        }
        let mut previous_digit = false;
        for ch in digits.chars() {
            if matches!(ch, '0'..='7') {
                previous_digit = true;
            } else if ch == '_'
                && previous_digit
                && self.dialect.supports_numeric_literal_underscores()
            {
                previous_digit = false;
            } else {
                return self
                    .tokenizer_error(start, format!("Invalid octal literal: 0{prefix}{digits}"));
            }
        }
        if !previous_digit {
            return self
                .tokenizer_error(start, format!("Invalid octal literal: 0{prefix}{digits}"));
        }
        let cleaned: String = digits.chars().filter(|ch| *ch != '_').collect();
        match u32::from_str_radix(&cleaned, 8) {
            Ok(value) => Ok(Some(Token::Number(value.to_string(), false))),
            Err(_) => self.tokenizer_error(
                start,
                format!("Octal literal out of range: 0{prefix}{digits}"),
            ),
        }
    }
}

/// Inspect a byte-unit suffix without consuming it, returning its length and multiplier.
pub(super) fn peek_byte_unit_suffix(chars: &State) -> Option<(usize, u128)> {
    let mut clone = chars.peekable.clone();
    let mut suffix = String::new();
    while let Some(&ch) = clone.peek() {
        if ch.is_ascii_alphabetic() {
            suffix.push(ch.to_ascii_lowercase());
            clone.next();
        } else {
            break;
        }
    }
    let multiplier = match suffix.as_str() {
        "b" => 1u128,
        "k" | "kib" => 1u128 << 10,
        "m" | "mib" => 1u128 << 20,
        "g" | "gib" => 1u128 << 30,
        "t" | "tib" => 1u128 << 40,
        "p" | "pib" => 1u128 << 50,
        "kb" => 1_000u128,
        "mb" => 1_000_000u128,
        "gb" => 1_000_000_000u128,
        "tb" => 1_000_000_000_000u128,
        "pb" => 1_000_000_000_000_000u128,
        _ => return None,
    };
    Some((suffix.len(), multiplier))
}

/// Scale exactly in base ten, then round half up to whole bytes. The decimal
/// coefficient can be arbitrarily long; multiplication needs only one carry per
/// digit, and exponents never cause allocation proportional to their magnitude.
pub(super) fn scale_by_byte_unit(literal: &str, multiplier: u128) -> Option<String> {
    let cleaned: String = literal.chars().filter(|ch| *ch != '_').collect();
    let (mantissa, exponent) = match cleaned.find(['e', 'E']) {
        Some(index) => (&cleaned[..index], parse_exponent(&cleaned[index + 1..])?),
        None => (cleaned.as_str(), 0),
    };
    let fractional_digits = mantissa
        .find('.')
        .map_or(0, |index| mantissa.len() - index - 1);
    let mut digits = Vec::with_capacity(mantissa.len() + 16);
    let mut carry = 0;
    for ch in mantissa.bytes().rev().filter(|ch| *ch != b'.') {
        if !ch.is_ascii_digit() {
            return None;
        }
        let product = u128::from(ch - b'0') * multiplier + carry;
        digits.push((product % 10) as u8);
        carry = product / 10;
    }
    while carry != 0 {
        digits.push((carry % 10) as u8);
        carry /= 10;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        return Some("0".into());
    }
    digits.reverse();

    let decimal_point = digits.len() as i128 + i128::from(exponent) - fractional_digits as i128;
    if decimal_point > 20 {
        return None;
    }
    if decimal_point <= 0 {
        return Some(
            if decimal_point == 0 && digits[0] >= 5 {
                "1"
            } else {
                "0"
            }
            .into(),
        );
    }
    let decimal_point = decimal_point as usize;
    let mut whole = 0u64;
    for index in 0..decimal_point {
        whole = whole
            .checked_mul(10)?
            .checked_add(u64::from(*digits.get(index).unwrap_or(&0)))?;
    }
    let fraction = digits.get(decimal_point..).unwrap_or_default();
    // Reject a raw value above the bound, even when rounding would hide the excess.
    if whole == u64::MAX && fraction.iter().any(|digit| *digit != 0) {
        return None;
    }
    if fraction.first().is_some_and(|digit| *digit >= 5) {
        whole = whole.checked_add(1)?;
    }
    Some(whole.to_string())
}

/// Saturation is sufficient: exponents outside i64 cannot be balanced by an
/// input mantissa's length. Zero is handled before applying the exponent.
fn parse_exponent(exponent: &str) -> Option<i64> {
    let (negative, digits) = match exponent.as_bytes().first() {
        Some(b'-') => (true, &exponent[1..]),
        Some(b'+') => (false, &exponent[1..]),
        _ => (false, exponent),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value = 0i64;
    for digit in digits.bytes() {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'));
    }
    Some(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::{Dialect, GenericDialect};
    use crate::tokenizer::{TokenWithSpan, Whitespace};
    #[cfg(not(feature = "std"))]
    use alloc::vec;

    #[derive(Debug)]
    struct NumericDialect {
        bytes: bool,
        octal: bool,
        underscores: bool,
    }

    impl Dialect for NumericDialect {
        fn is_identifier_start(&self, ch: char) -> bool {
            ch.is_ascii_alphabetic() || ch == '_'
        }
        fn is_identifier_part(&self, ch: char) -> bool {
            ch.is_ascii_alphanumeric() || ch == '_'
        }
        fn supports_byte_unit_suffixes(&self) -> bool {
            self.bytes
        }
        fn supports_octal_prefix(&self) -> bool {
            self.octal
        }
        fn supports_numeric_literal_underscores(&self) -> bool {
            self.underscores
        }
    }

    const DIALECT: NumericDialect = NumericDialect {
        bytes: true,
        octal: true,
        underscores: true,
    };

    fn assert_number(literal: &str, expected: &str) {
        let tokens = Tokenizer::new(&DIALECT, literal)
            .tokenize_with_location()
            .unwrap();
        assert_eq!(
            tokens,
            vec![TokenWithSpan::at(
                Token::Number(expected.into(), false),
                (1, 1).into(),
                (1, literal.len() as u64 + 1).into(),
            )],
            "{literal}"
        );
    }

    #[test]
    fn byte_units_and_case_variants() {
        for (suffix, expected) in [
            ("b", "1"),
            ("k", "1024"),
            ("m", "1048576"),
            ("g", "1073741824"),
            ("t", "1099511627776"),
            ("p", "1125899906842624"),
            ("kib", "1024"),
            ("mib", "1048576"),
            ("gib", "1073741824"),
            ("tib", "1099511627776"),
            ("pib", "1125899906842624"),
            ("kb", "1000"),
            ("mb", "1000000"),
            ("gb", "1000000000"),
            ("tb", "1000000000000"),
            ("pb", "1000000000000000"),
        ] {
            assert_number(&format!("1{suffix}"), expected);
            assert_number(&format!("1{}", suffix.to_ascii_uppercase()), expected);
        }
    }

    #[test]
    fn decimal_and_exponent_arithmetic_is_exact() {
        for (literal, expected) in [
            ("9007199254740993b", "9007199254740993"),
            ("9007199254740993.0b", "9007199254740993"),
            ("9.007199254740993e15b", "9007199254740993"),
            ("1.5mib", "1572864"),
            ("1_024k", "1048576"),
            ("1.2_5KiB", "1280"),
            (".5b", "1"),
            ("1.b", "1"),
            ("0.499999999999999999999999999999999999b", "0"),
            ("0.5b", "1"),
            ("1.499999999999999999999999999999999999b", "1"),
            ("1.5b", "2"),
            ("5e-1b", "1"),
            ("4.9999999999999999999e-1b", "0"),
            ("1.25E+3b", "1250"),
            ("1e-324g", "0"),
            ("18446744073709551615b", "18446744073709551615"),
            ("18446744073709551615.0b", "18446744073709551615"),
            ("1.8446744073709551615e19b", "18446744073709551615"),
            ("18446744073709551614.5b", "18446744073709551615"),
            ("0e999999999999999999999999999999999999999b", "0"),
            ("1e-999999999999999999999999999999999999999b", "0"),
        ] {
            assert_number(literal, expected);
        }
        assert_number(&format!("1{}e-100b", "0".repeat(100)), "1");
        assert_number(&format!("0.{}5e101b", "0".repeat(100)), "5");
    }

    #[test]
    fn byte_rounding_matches_integer_ratios() {
        for coefficient in 0..1000u128 {
            for fractional_digits in 0..=4u32 {
                for multiplier in [1, 1000, 1024, 1 << 50] {
                    let denominator = 10u128.pow(fractional_digits);
                    let expected = (coefficient * multiplier + denominator / 2) / denominator;
                    assert_eq!(
                        scale_by_byte_unit(
                            &format!("{coefficient}e-{fractional_digits}"),
                            multiplier
                        ),
                        Some(expected.to_string()),
                    );
                }
            }
        }
    }

    #[test]
    fn byte_overflow_reports_literal_start() {
        for literal in [
            "18446744073709551616b",
            "18446744073709551616.0b",
            "18446744073709551615.000000000000000000001b",
            "1.8446744073709551616e19b",
            "99999999pib",
            "1e309b",
            "1e999999999999999999999999999999999999999b",
        ] {
            let sql = format!("SELECT\n  {literal}");
            let error = Tokenizer::new(&DIALECT, &sql).tokenize().unwrap_err();
            assert_eq!(error.location, (2, 3).into(), "{literal}");
            assert!(
                error.message.starts_with("Byte size literal out of range:"),
                "{error}"
            );
        }
    }

    #[test]
    fn octal_prefixes_separators_and_bounds() {
        for (literal, expected) in [
            ("0o0", "0"),
            ("0O755", "493"),
            ("0o7_55", "493"),
            ("0O7_5_5", "493"),
            ("0o000755", "493"),
            ("0o37777777777", "4294967295"),
        ] {
            assert_number(literal, expected);
        }
    }

    #[test]
    fn malformed_octal_reports_literal_start() {
        for literal in [
            "0o",
            "0O",
            "0o8",
            "0o78",
            "0o755suffix",
            "0O755b",
            "0o_755",
            "0o755_",
            "0o7__55",
            "0o7_85",
            "0o40000000000",
        ] {
            let sql = format!("SELECT\n  {literal}");
            let error = Tokenizer::new(&DIALECT, &sql).tokenize().unwrap_err();
            assert_eq!(error.location, (2, 3).into(), "{literal}");
            assert!(
                error.message.contains("octal") || error.message.contains("Octal"),
                "{error}"
            );
        }
    }

    #[test]
    fn multiline_spans_cover_original_literal_and_preserve_neighbors() {
        let tokens = Tokenizer::new(&DIALECT, "1.5MiB,\n 0O7_55+2e3kb")
            .tokenize_with_location()
            .unwrap()
            .into_iter()
            .filter(|token| !matches!(token.token, Token::Whitespace(_)))
            .collect::<Vec<_>>();
        assert_eq!(
            tokens,
            vec![
                TokenWithSpan::at(
                    Token::Number("1572864".into(), false),
                    (1, 1).into(),
                    (1, 7).into()
                ),
                TokenWithSpan::at(Token::Comma, (1, 7).into(), (1, 8).into()),
                TokenWithSpan::at(
                    Token::Number("493".into(), false),
                    (2, 2).into(),
                    (2, 8).into()
                ),
                TokenWithSpan::at(Token::Plus, (2, 8).into(), (2, 9).into()),
                TokenWithSpan::at(
                    Token::Number("2000000".into(), false),
                    (2, 9).into(),
                    (2, 14).into()
                ),
            ]
        );
    }

    #[test]
    fn strings_comments_and_quoted_identifiers_are_untouched() {
        let sql = "'1g 0o755' \"1g\" -- 0O7_55\n/* 1.5mib */ $$0o8 1g$$";
        let extended = Tokenizer::new(&DIALECT, sql)
            .tokenize_with_location()
            .unwrap();
        let generic = Tokenizer::new(&GenericDialect, sql)
            .tokenize_with_location()
            .unwrap();
        assert_eq!(extended, generic);
        assert!(extended.iter().any(|token| matches!(
            token.token,
            Token::Whitespace(Whitespace::MultiLineComment(_))
        )));
    }

    #[test]
    fn extensions_are_disabled_by_default() {
        let tokens = Tokenizer::new(&GenericDialect, "1g 0o755 0O755")
            .tokenize()
            .unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Number("1".into(), false),
                Token::make_word("g", None),
                Token::Whitespace(Whitespace::Space),
                Token::Number("0".into(), false),
                Token::make_word("o755", None),
                Token::Whitespace(Whitespace::Space),
                Token::Number("0".into(), false),
                Token::make_word("O755", None),
            ]
        );
    }

    #[test]
    fn dialect_flags_are_independent() {
        let bytes_only = NumericDialect {
            bytes: true,
            octal: false,
            underscores: false,
        };
        assert_eq!(
            Tokenizer::new(&bytes_only, "1g").tokenize().unwrap(),
            vec![Token::Number("1073741824".into(), false)]
        );
        assert_eq!(
            Tokenizer::new(&bytes_only, "0o755").tokenize().unwrap(),
            vec![
                Token::Number("0".into(), false),
                Token::make_word("o755", None)
            ]
        );
        let octal_only = NumericDialect {
            bytes: false,
            octal: true,
            underscores: false,
        };
        assert_eq!(
            Tokenizer::new(&octal_only, "0O755").tokenize().unwrap(),
            vec![Token::Number("493".into(), false)]
        );
        assert_eq!(
            Tokenizer::new(&octal_only, "1g").tokenize().unwrap(),
            vec![
                Token::Number("1".into(), false),
                Token::make_word("g", None)
            ]
        );
        assert!(Tokenizer::new(&octal_only, "0o7_55").tokenize().is_err());
    }
}
