//! SCPI error types and error queue.
//!
//! SCPI defines a standardised set of error codes.  Negative codes are
//! instrument-defined; the ranges used here follow the IEEE 488.2 / SCPI-1999
//! standard appendix.

use std::fmt;

/// A single SCPI error: a numeric code plus a human-readable description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScpiError {
    /// Numeric error code (negative = instrument error, 0 = no error).
    pub code: i16,
    /// Short description string as defined by the standard.
    pub message: &'static str,
}

impl ScpiError {
    /// Create a custom (non-standard) error with `code < 0`.
    pub fn custom(code: i16, message: &'static str) -> Self {
        ScpiError { code, message }
    }
}

impl fmt::Display for ScpiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{:?}", self.code, self.message)
    }
}

// ---------------------------------------------------------------------------
// Standard SCPI error constants (SCPI-1999 §21.8.9)
// ---------------------------------------------------------------------------

/// No error.
pub const NO_ERROR: ScpiError = ScpiError { code: 0, message: "No error" };

// Command errors (−100 to −199)
/// Command error — generic.
pub const COMMAND_ERROR: ScpiError = ScpiError { code: -100, message: "Command error" };
/// Invalid character.
pub const INVALID_CHARACTER: ScpiError =
    ScpiError { code: -101, message: "Invalid character" };
/// Syntax error.
pub const SYNTAX_ERROR: ScpiError = ScpiError { code: -102, message: "Syntax error" };
/// Invalid separator.
pub const INVALID_SEPARATOR: ScpiError =
    ScpiError { code: -103, message: "Invalid separator" };
/// Data type error.
pub const DATA_TYPE_ERROR: ScpiError =
    ScpiError { code: -104, message: "Data type error" };
/// GET not allowed.
pub const GET_NOT_ALLOWED: ScpiError =
    ScpiError { code: -105, message: "GET not allowed" };
/// Undefined header.
pub const UNDEFINED_HEADER: ScpiError =
    ScpiError { code: -113, message: "Undefined header" };
/// Header suffix out of range.
pub const HEADER_SUFFIX_OUT_OF_RANGE: ScpiError =
    ScpiError { code: -114, message: "Header suffix out of range" };
/// Unexpected number of parameters.
pub const UNEXPECTED_NUMBER_OF_PARAMETERS: ScpiError =
    ScpiError { code: -115, message: "Unexpected number of parameters" };
/// Header not allowed.
pub const HEADER_NOT_ALLOWED: ScpiError =
    ScpiError { code: -116, message: "Header not allowed" };
/// Missing parameter.
pub const MISSING_PARAMETER: ScpiError =
    ScpiError { code: -109, message: "Missing parameter" };
/// Parameter not allowed.
pub const PARAMETER_NOT_ALLOWED: ScpiError =
    ScpiError { code: -108, message: "Parameter not allowed" };

// Execution errors (−200 to −299)
/// Execution error — generic.
pub const EXECUTION_ERROR: ScpiError =
    ScpiError { code: -200, message: "Execution error" };
/// Data out of range.
pub const DATA_OUT_OF_RANGE: ScpiError =
    ScpiError { code: -222, message: "Data out of range" };
/// Hardware error.
pub const HARDWARE_ERROR: ScpiError =
    ScpiError { code: -240, message: "Hardware error" };

// Device-specific errors (−300 to −399)
/// Device-specific error — generic.
pub const DEVICE_SPECIFIC_ERROR: ScpiError =
    ScpiError { code: -300, message: "Device-specific error" };
/// Storage fault.
pub const STORAGE_FAULT: ScpiError =
    ScpiError { code: -350, message: "Storage fault" };

// Query errors (−400 to −499)
/// Query error — generic.
pub const QUERY_ERROR: ScpiError = ScpiError { code: -400, message: "Query error" };
/// Query interrupted.
pub const QUERY_INTERRUPTED: ScpiError =
    ScpiError { code: -410, message: "Query INTERRUPTED" };
/// Query unterminated.
pub const QUERY_UNTERMINATED: ScpiError =
    ScpiError { code: -420, message: "Query UNTERMINATED" };
/// Query deadlocked.
pub const QUERY_DEADLOCKED: ScpiError =
    ScpiError { code: -430, message: "Query DEADLOCKED" };
/// Query unterminated after indefinite response.
pub const QUERY_UNTERMINATED_AFTER_INDEFINITE_RESPONSE: ScpiError = ScpiError {
    code: -440,
    message: "Query UNTERMINATED after indefinite response",
};

// ---------------------------------------------------------------------------
// Error queue
// ---------------------------------------------------------------------------

/// FIFO error queue as specified by SCPI (maximum depth 15+).
///
/// Errors are pushed onto the back and pulled from the front with
/// [`ErrorQueue::pop`].  When the queue is empty [`NO_ERROR`] is returned.
#[derive(Debug, Default)]
pub struct ErrorQueue {
    errors: std::collections::VecDeque<ScpiError>,
}

impl ErrorQueue {
    /// Create an empty error queue.
    pub fn new() -> Self {
        ErrorQueue { errors: std::collections::VecDeque::new() }
    }

    /// Push an error into the queue.
    pub fn push(&mut self, error: ScpiError) {
        self.errors.push_back(error);
    }

    /// Remove and return the oldest error, or [`NO_ERROR`] if empty.
    pub fn pop(&mut self) -> ScpiError {
        self.errors.pop_front().unwrap_or(NO_ERROR)
    }

    /// Return the number of errors currently queued.
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Return `true` if no errors are queued.
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// Discard all queued errors.
    pub fn clear(&mut self) {
        self.errors.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queue_returns_no_error() {
        let mut q = ErrorQueue::new();
        assert_eq!(q.pop(), NO_ERROR);
    }

    #[test]
    fn fifo_order() {
        let mut q = ErrorQueue::new();
        q.push(COMMAND_ERROR);
        q.push(DATA_OUT_OF_RANGE);
        assert_eq!(q.pop().code, -100);
        assert_eq!(q.pop().code, -222);
        assert_eq!(q.pop(), NO_ERROR);
    }

    #[test]
    fn clear_empties_queue() {
        let mut q = ErrorQueue::new();
        q.push(COMMAND_ERROR);
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.pop(), NO_ERROR);
    }

    #[test]
    fn display_format() {
        let err = COMMAND_ERROR;
        assert_eq!(err.to_string(), "-100,\"Command error\"");
    }
}
