//! # scpify
//!
//! `scpify` is a Rust library for sending and receiving SCPI (Standard
//! Commands for Programmable Instruments) protocol messages.  It is designed
//! as an importable crate that you can embed in your own instrument firmware,
//! simulation, or test-automation project to add complete SCPI support.
//!
//! ## Features
//!
//! * **Message parser** — tokenise and parse SCPI command strings into typed
//!   [`Command`] structures, including compound messages (`"*RST;*IDN?"`).
//! * **Mnemonic matching** — both the short form (`MEAS`) and long form
//!   (`MEASure`) of every mnemonic are accepted, case-insensitively.
//! * **Response types** — strongly-typed [`Response`] values that format
//!   themselves according to the SCPI standard.
//! * **IEEE 488.2 common commands** — built-in handlers for `*IDN?`, `*RST`,
//!   `*CLS`, `*ESE`, `*ESR?`, `*OPC`, `*SRE`, `*STB?`, `*TST?`, and
//!   `*WAI`.
//! * **Error queue** — SCPI-standard FIFO error queue with standard error
//!   codes.
//! * **`Device` struct** — a ready-to-use dispatcher that routes incoming
//!   messages to registered command handlers or the IEEE 488.2 built-ins.
//! * **TCP transport** *(feature `tcp`)* — [`transport::TcpServer`] serves
//!   SCPI-RAW over TCP (port 5025), either sequentially or concurrently.
//!
//! ## Quick start
//!
//! ```rust
//! use scpify::{Device, Response, Identification};
//! use scpify::command::Command;
//!
//! // Build a device with its identification string.
//! let mut device = Device::new(Identification {
//!     manufacturer: "ACME".into(),
//!     model: "XT1".into(),
//!     serial: "SN001".into(),
//!     version: "1.0".into(),
//! });
//!
//! // Register a custom query handler.
//! device.register(|cmd: &Command| {
//!     if cmd.matches_header("MEASure:VOLTage:DC") && cmd.is_query {
//!         Some(Response::Float(3.3))
//!     } else {
//!         None
//!     }
//! });
//!
//! // Process a compound message.
//! let responses = device.process("*IDN?;:MEASure:VOLTage:DC?");
//! assert_eq!(responses.len(), 2);
//! ```

pub mod command;
pub mod error;
pub mod ieee488;
pub mod parser;
pub mod token;
#[cfg(feature = "tcp")]
pub mod transport;

pub use command::{Command, Param, Response, header_matches, mnemonic_matches};
pub use error::{ErrorQueue, ScpiError};
pub use ieee488::{Identification, Ieee488State, esr, stb};
pub use parser::parse;

use crate::error::{COMMAND_ERROR, UNDEFINED_HEADER};
use ieee488::handle_common_command;

// ---------------------------------------------------------------------------
// Handler type alias
// ---------------------------------------------------------------------------

/// A SCPI command handler: a boxed closure that receives a [`Command`] and
/// returns `Some(Response)` if it handles the command, or `None` to pass to
/// the next handler.
type Handler = Box<dyn Fn(&Command) -> Option<Response> + Send + Sync>;

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// A SCPI device that dispatches incoming messages to registered handlers.
///
/// `Device` owns:
/// * an [`Ieee488State`] for IEEE 488.2 common commands,
/// * an [`ErrorQueue`] that accumulates errors,
/// * a list of user-registered command handlers tried in registration order.
///
/// Call [`Device::process`] to handle a raw SCPI message string.  The method
/// returns one [`Response`] per parsed command (empty responses for
/// non-query commands, or an error response if no handler matched).
pub struct Device {
    /// IEEE 488.2 state (registers + identification).
    pub state: Ieee488State,
    /// SCPI error queue.
    pub error_queue: ErrorQueue,
    /// User-registered handlers, tried in order.
    handlers: Vec<Handler>,
}

impl Device {
    /// Create a new `Device` with the given identification.
    pub fn new(identification: Identification) -> Self {
        Device {
            state: Ieee488State::new(identification),
            error_queue: ErrorQueue::new(),
            handlers: Vec::new(),
        }
    }

    /// Register a command handler.
    ///
    /// Handlers are tried in the order they were registered.  The first
    /// handler that returns `Some(response)` "wins".
    ///
    /// # Example
    ///
    /// ```rust
    /// use scpify::{Device, Response, Identification};
    /// use scpify::command::Command;
    ///
    /// let mut device = Device::new(Identification::default());
    /// device.register(|cmd: &Command| {
    ///     if cmd.matches_header("SYSTem:ERRor") && cmd.is_query {
    ///         Some(Response::Str("0,\"No error\"".into()))
    ///     } else {
    ///         None
    ///     }
    /// });
    /// ```
    pub fn register<F>(&mut self, handler: F)
    where
        F: Fn(&Command) -> Option<Response> + Send + Sync + 'static,
    {
        self.handlers.push(Box::new(handler));
    }

    /// Process a raw SCPI message string and return responses.
    ///
    /// Each parsed sub-command produces exactly one entry in the returned
    /// `Vec`:
    /// * Query commands produce a [`Response`] value.
    /// * Non-query commands produce [`Response::Empty`] on success.
    /// * Unrecognised commands produce [`Response::Empty`] and push an
    ///   error onto the error queue.
    pub fn process(&mut self, message: &str) -> Vec<Response> {
        let commands = parse(message);
        let mut responses = Vec::with_capacity(commands.len());
        for cmd in &commands {
            let response = self.dispatch(cmd);
            responses.push(response);
        }
        responses
    }

    /// Dispatch a single [`Command`] through IEEE 488.2 common handlers and
    /// then user-registered handlers.
    fn dispatch(&mut self, cmd: &Command) -> Response {
        // 1. Try IEEE 488.2 common commands.
        match handle_common_command(cmd, &mut self.state, &mut self.error_queue) {
            Ok(response) => return response,
            Err(ref e) if *e == UNDEFINED_HEADER => {} // not a common command
            Err(e) => {
                self.error_queue.push(e);
                return Response::Empty;
            }
        }

        // 2. Try user-registered handlers.
        for handler in &self.handlers {
            if let Some(response) = handler(cmd) {
                return response;
            }
        }

        // 3. No handler matched — push command error.
        self.error_queue.push(COMMAND_ERROR);
        Response::Empty
    }

    /// Pop the oldest error from the error queue (returns `NO_ERROR` if
    /// empty).
    pub fn next_error(&mut self) -> ScpiError {
        self.error_queue.pop()
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("state", &self.state)
            .field("error_queue", &self.error_queue)
            .field("handlers", &format!("[{} handler(s)]", self.handlers.len()))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> Device {
        Device::new(Identification {
            manufacturer: "TestCo".into(),
            model: "M1".into(),
            serial: "001".into(),
            version: "0.1".into(),
        })
    }

    #[test]
    fn idn_query_via_device() {
        let mut d = device();
        let responses = d.process("*IDN?");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0], Response::Str("TestCo,M1,001,0.1".into()));
    }

    #[test]
    fn rst_is_non_query() {
        let mut d = device();
        let responses = d.process("*RST");
        assert_eq!(responses[0], Response::Empty);
    }

    #[test]
    fn compound_message() {
        let mut d = device();
        let responses = d.process("*RST;*IDN?");
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0], Response::Empty);
        assert!(matches!(&responses[1], Response::Str(_)));
    }

    #[test]
    fn custom_handler() {
        let mut d = device();
        d.register(|cmd: &Command| {
            if cmd.matches_header("MEASure:VOLTage:DC") && cmd.is_query {
                Some(Response::Float(3.3))
            } else {
                None
            }
        });
        let responses = d.process(":MEASure:VOLTage:DC?");
        assert_eq!(responses[0], Response::Float(3.3));
    }

    #[test]
    fn undefined_command_pushes_error() {
        let mut d = device();
        d.process(":FAKE:CMD?");
        let err = d.next_error();
        assert_ne!(err.code, 0);
    }

    #[test]
    fn error_queue_clears_on_cls() {
        let mut d = device();
        // Generate an error by sending an unknown command.
        d.process(":FAKE?");
        assert!(!d.error_queue.is_empty());
        d.process("*CLS");
        assert!(d.error_queue.is_empty());
    }

    #[test]
    fn handler_short_form_match() {
        let mut d = device();
        d.register(|cmd: &Command| {
            if cmd.matches_header("MEASure") && cmd.is_query {
                Some(Response::Float(1.23))
            } else {
                None
            }
        });
        // Short form.
        let r1 = d.process(":MEAS?");
        // Long form.
        let r2 = d.process(":MEASure?");
        assert_eq!(r1[0], Response::Float(1.23));
        assert_eq!(r2[0], Response::Float(1.23));
    }

    #[test]
    fn ese_round_trip_via_device() {
        let mut d = device();
        d.process("*ESE 32");
        let responses = d.process("*ESE?");
        assert_eq!(responses[0], Response::Integer(32));
    }

    #[test]
    fn device_debug_impl() {
        let mut d = device();
        d.register(|_| None);
        let s = format!("{:?}", d);
        assert!(s.contains("Device"));
        assert!(s.contains("1 handler"));
    }
}
