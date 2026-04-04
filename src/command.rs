//! SCPI command and response types.
//!
//! This module defines the structures that represent a parsed SCPI command
//! ready for dispatch, together with the response value types.

use std::fmt;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// A single SCPI parameter value, extracted from the parameter section of a
/// message.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    /// Character data / mnemonic parameter (e.g. `ON`, `OFF`, `MIN`, `MAX`).
    Character(String),
    /// Numeric (integer) parameter.
    Integer(i64),
    /// Numeric (floating-point) parameter.
    Float(f64),
    /// String parameter (double-quoted content with quotes removed).
    Str(String),
    /// Boolean shorthand: `ON` / `1` → `true`, `OFF` / `0` → `false`.
    Bool(bool),
}

impl Param {
    /// Try to interpret the parameter as an integer.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Param::Integer(n) => Some(*n),
            Param::Float(f) => Some(*f as i64),
            Param::Character(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Try to interpret the parameter as a float.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Param::Float(f) => Some(*f),
            Param::Integer(n) => Some(*n as f64),
            Param::Character(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Try to interpret the parameter as a boolean (`ON`/`1` = true,
    /// `OFF`/`0` = false).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Param::Bool(b) => Some(*b),
            Param::Integer(n) => Some(*n != 0),
            Param::Character(s) => match s.to_ascii_uppercase().as_str() {
                "ON" | "1" => Some(true),
                "OFF" | "0" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// Try to interpret the parameter as a string slice.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Param::Str(s) | Param::Character(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

impl fmt::Display for Param {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Param::Character(s) => write!(f, "{}", s),
            Param::Integer(n) => write!(f, "{}", n),
            Param::Float(v) => write!(f, "{}", v),
            Param::Str(s) => write!(f, "\"{}\"", s),
            Param::Bool(b) => write!(f, "{}", if *b { "1" } else { "0" }),
        }
    }
}

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

/// A fully-parsed SCPI command ready for dispatch.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// The command header as a colon-joined path, e.g.
    /// `"MEASure:VOLTage:DC"`.
    pub header: String,
    /// Whether this is a query (`?`).
    pub is_query: bool,
    /// Optional numeric suffix (e.g. `2` for `CHANnel2`).
    pub suffix: Option<u32>,
    /// Parameters provided after the header.
    pub params: Vec<Param>,
}

impl Command {
    /// Return `true` if this command's header (case-insensitively) matches
    /// `pattern`.
    ///
    /// Both the short form (uppercase letters only) and long form (all
    /// letters) are accepted.  For example, the pattern `"MEASure"` matches
    /// both `"MEAS"` and `"MEASure"`.
    pub fn matches_header(&self, pattern: &str) -> bool {
        header_matches(&self.header, pattern)
    }
}

/// Check whether a received header string matches a SCPI mnemonic pattern.
///
/// Rules:
/// * The mandatory part of the mnemonic is the *uppercase* prefix.
/// * The full long form is the complete string.
/// * Matching is case-insensitive.
/// * Both must have the same number of `:` delimited segments.
///
/// Example: `"MEASure:VOLTage"` matches `"MEAS"`, `"MEASure"`, `"measure"`.
pub fn header_matches(received: &str, pattern: &str) -> bool {
    let recv_segs: Vec<&str> = received.split(':').collect();
    let patt_segs: Vec<&str> = pattern.split(':').collect();
    if recv_segs.len() != patt_segs.len() {
        return false;
    }
    recv_segs.iter().zip(patt_segs.iter()).all(|(r, p)| mnemonic_matches(r, p))
}

/// Check whether a single mnemonic token `received` matches `pattern`.
///
/// The uppercase prefix of `pattern` is the short form; the full `pattern`
/// is the long form.  Both are accepted (case-insensitively).
pub fn mnemonic_matches(received: &str, pattern: &str) -> bool {
    // Derive the short form: all uppercase letters in `pattern`.
    let short: String = pattern.chars().filter(|c| c.is_ascii_uppercase()).collect();

    let recv_upper = received.to_ascii_uppercase();
    let short_upper = short.to_ascii_uppercase();
    let long_upper = pattern.to_ascii_uppercase();

    recv_upper == short_upper || recv_upper == long_upper
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// A SCPI response value returned by a query handler.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// No response (non-query commands).
    Empty,
    /// Integer response.
    Integer(i64),
    /// Floating-point response.
    Float(f64),
    /// String response (will be double-quoted in the output).
    Str(String),
    /// Character data / mnemonic response (not quoted).
    Character(String),
    /// Boolean response: formatted as `1` or `0`.
    Bool(bool),
    /// Compound response: multiple values separated by `,`.
    Compound(Vec<Response>),
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Response::Empty => Ok(()),
            Response::Integer(n) => write!(f, "{}", n),
            Response::Float(v) => write!(f, "{:.6E}", v),
            Response::Str(s) => write!(f, "\"{}\"", s),
            Response::Character(s) => write!(f, "{}", s.to_ascii_uppercase()),
            Response::Bool(b) => write!(f, "{}", if *b { 1 } else { 0 }),
            Response::Compound(items) => {
                let parts: Vec<String> = items.iter().map(|r| r.to_string()).collect();
                write!(f, "{}", parts.join(","))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mnemonic_short_form() {
        assert!(mnemonic_matches("MEAS", "MEASure"));
        assert!(mnemonic_matches("measure", "MEASure"));
        assert!(mnemonic_matches("MEASure", "MEASure"));
    }

    #[test]
    fn mnemonic_no_match() {
        assert!(!mnemonic_matches("VOLT", "MEASure"));
        assert!(!mnemonic_matches("MEASU", "MEASure")); // partial long form
    }

    #[test]
    fn header_segment_count() {
        assert!(!header_matches("MEASure", "MEASure:VOLTage"));
    }

    #[test]
    fn header_full_path() {
        assert!(header_matches("MEAS:VOLT:DC", "MEASure:VOLTage:DC"));
    }

    #[test]
    fn response_display_integer() {
        assert_eq!(Response::Integer(42).to_string(), "42");
    }

    #[test]
    fn response_display_bool() {
        assert_eq!(Response::Bool(true).to_string(), "1");
        assert_eq!(Response::Bool(false).to_string(), "0");
    }

    #[test]
    fn response_display_string() {
        assert_eq!(Response::Str("hello".into()).to_string(), "\"hello\"");
    }

    #[test]
    fn response_display_compound() {
        let r = Response::Compound(vec![
            Response::Integer(1),
            Response::Integer(2),
            Response::Integer(3),
        ]);
        assert_eq!(r.to_string(), "1,2,3");
    }

    #[test]
    fn param_as_bool() {
        assert_eq!(Param::Character("ON".into()).as_bool(), Some(true));
        assert_eq!(Param::Character("OFF".into()).as_bool(), Some(false));
        assert_eq!(Param::Integer(0).as_bool(), Some(false));
        assert_eq!(Param::Integer(1).as_bool(), Some(true));
    }
}
