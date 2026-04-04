//! SCPI message parser.
//!
//! Converts a raw SCPI string into a list of [`Command`]s ready for
//! dispatch by a [`crate::Device`].
//!
//! The parser uses the token stream from [`crate::token`] and reconstructs
//! the hierarchical header path, splitting on `;` compound-message
//! separators.

use crate::command::{Command, Param};
use crate::token::{Token, tokenize};

/// Parse a raw SCPI message string into a `Vec<Command>`.
///
/// Compound messages (`"*RST;*IDN?"`) are split on `;` and each sub-message
/// is returned as a separate [`Command`].
///
/// Currently infallible — malformed tokens are silently skipped.
pub fn parse(message: &str) -> Vec<Command> {
    let mut commands: Vec<Command> = Vec::new();

    let mut segments: Vec<String> = Vec::new();
    let mut suffix: Option<u32> = None;
    let mut is_query = false;
    let mut params: Vec<Param> = Vec::new();
    let mut in_params = false;

    for token in tokenize(message) {
        match token {
            Token::Mnemonic(m) => {
                if in_params {
                    // A mnemonic appearing in param position is character data.
                    params.push(parse_char_param(m));
                } else {
                    segments.push(m.to_string());
                    // Note: do NOT reset `suffix` here — a NumericSuffix that
                    // appeared just before this mnemonic is kept until the
                    // command is finalised.
                }
            }

            Token::NumericSuffix(n) => {
                suffix = Some(n);
            }

            Token::Query => {
                is_query = true;
                in_params = true;
            }

            Token::CharParam(s) => {
                params.push(parse_char_param(s));
                in_params = true;
            }

            Token::StringParam(s) => {
                params.push(Param::Str(s.to_string()));
                in_params = true;
            }

            Token::ParameterSeparator => {
                // Nothing to do; just continue collecting params.
            }

            Token::MessageTerminator => {
                if !segments.is_empty() {
                    commands.push(Command {
                        header: segments.join(":"),
                        is_query,
                        suffix,
                        params: std::mem::take(&mut params),
                    });
                }
                segments.clear();
                suffix = None;
                is_query = false;
                in_params = false;
            }
        }
    }

    // Flush the last command.
    if !segments.is_empty() {
        commands.push(Command {
            header: segments.join(":"),
            is_query,
            suffix,
            params,
        });
    }

    commands
}

/// Convert a raw character-data string into the most specific [`Param`]
/// variant possible.
fn parse_char_param(s: &str) -> Param {
    // Boolean keyword?
    match s.to_ascii_uppercase().as_str() {
        "ON" => return Param::Bool(true),
        "OFF" => return Param::Bool(false),
        _ => {}
    }

    // Integer (1 and 0 are boolean shorthands per SCPI spec)?
    if let Ok(n) = s.parse::<i64>() {
        if n == 1 {
            return Param::Bool(true);
        }
        if n == 0 {
            return Param::Bool(false);
        }
        return Param::Integer(n);
    }

    // Float?
    if let Ok(f) = s.parse::<f64>() {
        return Param::Float(f);
    }

    // Character data / mnemonic.
    Param::Character(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Param;

    #[test]
    fn parse_idn_query() {
        let cmds = parse("*IDN?");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].header, "*IDN");
        assert!(cmds[0].is_query);
        assert!(cmds[0].params.is_empty());
    }

    #[test]
    fn parse_rst_command() {
        let cmds = parse("*RST");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].header, "*RST");
        assert!(!cmds[0].is_query);
    }

    #[test]
    fn parse_subsystem_query() {
        let cmds = parse(":MEASure:VOLTage:DC?");
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].is_query);
        assert_eq!(cmds[0].header, "MEASure:VOLTage:DC");
    }

    #[test]
    fn parse_command_with_bool_param() {
        let cmds = parse(":OUTPut:STATe ON");
        assert_eq!(cmds.len(), 1);
        assert!(!cmds[0].is_query);
        assert_eq!(cmds[0].params, vec![Param::Bool(true)]);
    }

    #[test]
    fn parse_compound_message() {
        let cmds = parse("*RST;*IDN?");
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].header, "*RST");
        assert_eq!(cmds[1].header, "*IDN");
        assert!(cmds[1].is_query);
    }

    #[test]
    fn parse_integer_param() {
        let cmds = parse(":FREQuency 1000");
        assert_eq!(cmds[0].params, vec![Param::Integer(1000)]);
    }

    #[test]
    fn parse_float_param() {
        let cmds = parse(":FREQuency 1.5e3");
        assert_eq!(cmds[0].params, vec![Param::Float(1500.0)]);
    }

    #[test]
    fn parse_string_param() {
        let cmds = parse(":SYSTem:LABel \"scope 1\"");
        assert_eq!(cmds[0].params, vec![Param::Str("scope 1".into())]);
    }

    #[test]
    fn parse_multiple_params() {
        let cmds = parse(":DATA:POINTS 100,200,300");
        assert_eq!(cmds[0].params.len(), 3);
    }

    #[test]
    fn parse_channel_suffix() {
        let cmds = parse(":CHANnel2:DISPlay ON");
        assert_eq!(cmds[0].suffix, Some(2));
        assert_eq!(cmds[0].header, "CHANnel:DISPlay");
    }

    #[test]
    fn parse_ese_with_integer_param() {
        let cmds = parse("*ESE 32");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].header, "*ESE");
        assert!(!cmds[0].is_query);
        assert_eq!(cmds[0].params, vec![Param::Integer(32)]);
    }
}
