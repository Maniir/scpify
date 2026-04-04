//! SCPI message tokenizer.
//!
//! A raw SCPI message such as `":MEASure:VOLTage:DC? 10;*IDN?"` is broken
//! into a sequence of [`Token`]s that the parser then uses to build a command
//! dispatch list.
//!
//! Grammar reference: SCPI-1999 §7 "Program Messages".
//!
//! ## Design notes
//!
//! The iterator uses an explicit loop (no recursion) so it always terminates.
//! Every branch that does not return a token **must** advance `self.pos` to
//! guarantee progress.  The `*` prefix for IEEE 488 common commands is handled
//! inside `read_mnemonic`.  The header → parameter boundary is detected by a
//! one-shot lookahead after each mnemonic token is produced.

/// A single token produced by the SCPI lexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token<'a> {
    /// A header mnemonic segment, e.g. `"MEASure"`, `"VOLTage"`, `"*IDN"`.
    Mnemonic(&'a str),
    /// The `?` suffix that marks a query.
    Query,
    /// A numeric suffix immediately after a mnemonic with no intervening
    /// whitespace, e.g. the `2` in `"CHANnel2"`.
    NumericSuffix(u32),
    /// A string parameter (content between double-quotes, without quotes).
    StringParam(&'a str),
    /// An unquoted character-data / numeric parameter token.
    CharParam(&'a str),
    /// `;` separates compound messages.
    MessageTerminator,
    /// `,` separates parameters.
    ParameterSeparator,
}

/// Tokenize a raw SCPI message string into an iterator of [`Token`]s.
pub fn tokenize(input: &str) -> Tokenizer<'_> {
    Tokenizer::new(input)
}

/// Iterator that produces [`Token`]s from a SCPI message string.
pub struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
    /// `true` once we have crossed into parameter territory (after `?` or
    /// after whitespace that follows the last header mnemonic).
    in_params: bool,
    /// `true` when the most-recently consumed non-whitespace separator was
    /// `:`.  Used to distinguish the header/parameter boundary.
    last_was_colon: bool,
}

impl<'a> Tokenizer<'a> {
    fn new(src: &'a str) -> Self {
        Tokenizer { src, pos: 0, in_params: false, last_was_colon: true }
    }

    // ------------------------------------------------------------------
    // Low-level helpers
    // ------------------------------------------------------------------

    /// Skip ASCII whitespace.  Returns `true` if any whitespace was consumed.
    fn skip_whitespace(&mut self) -> bool {
        let start = self.pos;
        while self.pos < self.src.len()
            && self.src.as_bytes()[self.pos].is_ascii_whitespace()
        {
            self.pos += 1;
        }
        self.pos > start
    }

    fn peek_byte(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    /// Read a mnemonic token.
    ///
    /// Accepts an optional leading `*` (for IEEE 488 common commands such as
    /// `*IDN`).  For non-star mnemonics, trailing digits are **not** consumed
    /// here; instead `self.pos` is left pointing at the first trailing digit
    /// so that the caller's loop will emit a [`Token::NumericSuffix`] on the
    /// next iteration.
    fn read_mnemonic(&mut self) -> &'a str {
        let start = self.pos;
        // Accept leading `*` for IEEE 488 common commands.
        if self.pos < self.src.len() && self.src.as_bytes()[self.pos] == b'*' {
            self.pos += 1;
        }
        // Consume alphanumeric chars and underscores.
        while self.pos < self.src.len() {
            let b = self.src.as_bytes()[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let raw = &self.src[start..self.pos];

        // For non-star mnemonics, strip trailing digits so they are emitted
        // as a separate NumericSuffix token.
        if !raw.starts_with('*') {
            let alpha_len =
                raw.trim_end_matches(|c: char| c.is_ascii_digit()).len();
            if alpha_len < raw.len() {
                // Back up so digits are processed on the next iteration.
                self.pos = start + alpha_len;
                return &self.src[start..self.pos];
            }
        }
        raw
    }

    /// Read a double-quoted string, returning its content (without quotes).
    /// `self.pos` must be pointing at the opening `"` on entry.
    fn read_string(&mut self) -> &'a str {
        self.pos += 1; // consume opening `"`
        let start = self.pos;
        while self.pos < self.src.len() {
            if self.src.as_bytes()[self.pos] == b'"' {
                let content = &self.src[start..self.pos];
                self.pos += 1; // consume closing `"`
                return content;
            }
            self.pos += 1;
        }
        &self.src[start..self.pos]
    }

    /// Read an unquoted parameter token, terminated by `,`, `;`, or
    /// end-of-input.
    fn read_char_param(&mut self) -> &'a str {
        let start = self.pos;
        while self.pos < self.src.len() {
            let b = self.src.as_bytes()[self.pos];
            if b == b',' || b == b';' {
                break;
            }
            self.pos += 1;
        }
        self.src[start..self.pos].trim_end()
    }

    /// Peek ahead (without advancing `self.pos`) to decide whether we have
    /// crossed the header → parameter boundary.
    ///
    /// Rules:
    /// * If the next non-whitespace character is `:`, `?`, `;`, or
    ///   end-of-input → still in header territory.
    /// * If a digit follows **immediately** (no whitespace) → it is a
    ///   numeric suffix, still in header territory.
    /// * Anything else (including a digit after whitespace) → parameter
    ///   territory; set `self.in_params = true`.
    fn detect_param_boundary(&mut self) {
        let save = self.pos;
        let ws_skipped = self.skip_whitespace();
        match self.peek_byte() {
            None | Some(b':') | Some(b'?') | Some(b';') => {
                // Still header or end of message.
            }
            Some(b) if b.is_ascii_digit() && !ws_skipped => {
                // Numeric suffix immediately follows — still in header.
            }
            _ => {
                self.in_params = true;
            }
        }
        self.pos = save; // restore; whitespace will be re-skipped in next()
    }
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Token<'a>;

    /// Produce the next token.  Uses an explicit loop to guarantee that
    /// `self.pos` always advances and no recursion / stack overflow occurs.
    fn next(&mut self) -> Option<Token<'a>> {
        loop {
            let ws_skipped = self.skip_whitespace();

            // If whitespace was consumed after a non-colon token, we have
            // crossed into parameter territory.
            if ws_skipped && !self.in_params && !self.last_was_colon {
                self.in_params = true;
            }

            let b = self.peek_byte()?; // returns None at end of input

            match b {
                // `;` — compound message separator; reset to header state.
                b';' => {
                    self.pos += 1;
                    self.in_params = false;
                    self.last_was_colon = true;
                    return Some(Token::MessageTerminator);
                }

                // `,` — parameter separator.
                b',' => {
                    self.pos += 1;
                    return Some(Token::ParameterSeparator);
                }

                // `?` — query suffix; enter parameter territory.
                b'?' => {
                    self.pos += 1;
                    self.in_params = true;
                    self.last_was_colon = false;
                    return Some(Token::Query);
                }

                // `:` — path separator; consume and continue the loop.
                b':' => {
                    self.pos += 1;
                    self.last_was_colon = true;
                    // Continue the loop (no token emitted for `:` itself).
                    continue;
                }

                // Double-quoted string parameter.
                b'"' => {
                    self.last_was_colon = false;
                    return Some(Token::StringParam(self.read_string()));
                }

                // Alphabetic or `*` — mnemonic or character parameter.
                b if b.is_ascii_alphabetic() || b == b'*' => {
                    self.last_was_colon = false;
                    if self.in_params {
                        return Some(Token::CharParam(self.read_char_param()));
                    }
                    let mnem = self.read_mnemonic();
                    // After emitting a mnemonic, peek ahead to detect whether
                    // the next token begins parameter territory.
                    self.detect_param_boundary();
                    return Some(Token::Mnemonic(mnem));
                }

                // Digit — numeric suffix (header territory) or numeric
                // parameter (parameter territory).
                b if b.is_ascii_digit() => {
                    self.last_was_colon = false;
                    if self.in_params {
                        return Some(Token::CharParam(self.read_char_param()));
                    }
                    // Numeric suffix: consume only ASCII digits.
                    let start = self.pos;
                    while self.pos < self.src.len()
                        && self.src.as_bytes()[self.pos].is_ascii_digit()
                    {
                        self.pos += 1;
                    }
                    let digits = &self.src[start..self.pos];
                    let n: u32 = digits.parse().unwrap_or(0);
                    return Some(Token::NumericSuffix(n));
                }

                // `+`, `-`, `.` — can only appear in parameter values.
                b'+' | b'-' | b'.' => {
                    self.last_was_colon = false;
                    if self.in_params {
                        return Some(Token::CharParam(self.read_char_param()));
                    }
                    // In header context these are unexpected; skip and continue.
                    self.pos += 1;
                }

                // Any other byte — skip it and continue.  `self.pos` is
                // incremented here to guarantee progress.
                _ => {
                    self.pos += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(s: &str) -> Vec<Token<'_>> {
        tokenize(s).collect()
    }

    #[test]
    fn simple_idn_query() {
        let t = tokens("*IDN?");
        assert_eq!(t, vec![Token::Mnemonic("*IDN"), Token::Query]);
    }

    #[test]
    fn simple_rst() {
        let t = tokens("*RST");
        assert_eq!(t, vec![Token::Mnemonic("*RST")]);
    }

    #[test]
    fn subsystem_query() {
        let t = tokens(":MEASure:VOLTage:DC?");
        assert_eq!(
            t,
            vec![
                Token::Mnemonic("MEASure"),
                Token::Mnemonic("VOLTage"),
                Token::Mnemonic("DC"),
                Token::Query,
            ]
        );
    }

    #[test]
    fn command_with_bool_param() {
        let t = tokens(":OUTPut:STATe ON");
        assert_eq!(
            t,
            vec![
                Token::Mnemonic("OUTPut"),
                Token::Mnemonic("STATe"),
                Token::CharParam("ON"),
            ]
        );
    }

    #[test]
    fn compound_message() {
        let t = tokens("*RST;*IDN?");
        assert_eq!(
            t,
            vec![
                Token::Mnemonic("*RST"),
                Token::MessageTerminator,
                Token::Mnemonic("*IDN"),
                Token::Query,
            ]
        );
    }

    #[test]
    fn numeric_suffix_no_space() {
        // `2` immediately follows `CHANnel` — it is a suffix, not a param.
        let t = tokens(":CHANnel2:DISPlay ON");
        assert_eq!(
            t,
            vec![
                Token::Mnemonic("CHANnel"),
                Token::NumericSuffix(2),
                Token::Mnemonic("DISPlay"),
                Token::CharParam("ON"),
            ]
        );
    }

    #[test]
    fn numeric_param_after_whitespace() {
        // `32` is separated by whitespace — it is a parameter.
        let t = tokens("*ESE 32");
        assert_eq!(
            t,
            vec![Token::Mnemonic("*ESE"), Token::CharParam("32")],
        );
    }

    #[test]
    fn string_parameter() {
        let t = tokens(":SYSTem:LABel \"Hello\"");
        assert_eq!(
            t,
            vec![
                Token::Mnemonic("SYSTem"),
                Token::Mnemonic("LABel"),
                Token::StringParam("Hello"),
            ]
        );
    }

    #[test]
    fn float_parameter() {
        let t = tokens(":FREQuency 1.5e3");
        assert_eq!(
            t,
            vec![Token::Mnemonic("FREQuency"), Token::CharParam("1.5e3")],
        );
    }

    #[test]
    fn multiple_params_with_comma() {
        let t = tokens(":DATA:POINTS 100,200,300");
        assert_eq!(
            t,
            vec![
                Token::Mnemonic("DATA"),
                Token::Mnemonic("POINTS"),
                Token::CharParam("100"),
                Token::ParameterSeparator,
                Token::CharParam("200"),
                Token::ParameterSeparator,
                Token::CharParam("300"),
            ]
        );
    }

    #[test]
    fn query_with_param() {
        // Some instruments accept params on queries, e.g. `*ESE? 32` (rare).
        let t = tokens(":CHANnel1:MEASure? RMS");
        assert_eq!(
            t,
            vec![
                Token::Mnemonic("CHANnel"),
                Token::NumericSuffix(1),
                Token::Mnemonic("MEASure"),
                Token::Query,
                Token::CharParam("RMS"),
            ]
        );
    }
}
