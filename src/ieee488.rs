//! IEEE 488.2 mandatory common commands.
//!
//! Every SCPI instrument must support the IEEE 488.2 common commands.  This
//! module provides default implementations that can be embedded inside a
//! [`crate::Device`].
//!
//! ## Supported commands
//!
//! | Command  | Description                         |
//! |----------|-------------------------------------|
//! | `*IDN?`  | Identification query                |
//! | `*RST`   | Reset                               |
//! | `*CLS`   | Clear status                        |
//! | `*ESE`   | Event Status Enable (write)         |
//! | `*ESE?`  | Event Status Enable (query)         |
//! | `*ESR?`  | Event Status Register (query)       |
//! | `*OPC`   | Operation Complete (set bit)        |
//! | `*OPC?`  | Operation Complete (query)          |
//! | `*SRE`   | Service Request Enable (write)      |
//! | `*SRE?`  | Service Request Enable (query)      |
//! | `*STB?`  | Status Byte (query)                 |
//! | `*TST?`  | Self-Test (query)                   |
//! | `*WAI`   | Wait-to-continue                    |

use crate::command::{Command, Response};
use crate::error::{ErrorQueue, ScpiError, MISSING_PARAMETER, UNDEFINED_HEADER};

/// Identification fields embedded in the instrument.
#[derive(Debug, Clone)]
pub struct Identification {
    /// Manufacturer name.
    pub manufacturer: String,
    /// Model name.
    pub model: String,
    /// Serial number.
    pub serial: String,
    /// Firmware / software version.
    pub version: String,
}

impl Default for Identification {
    fn default() -> Self {
        Identification {
            manufacturer: "Unknown".into(),
            model: "Unknown".into(),
            serial: "0".into(),
            version: "0.1.0".into(),
        }
    }
}

impl std::fmt::Display for Identification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{},{},{}", self.manufacturer, self.model, self.serial, self.version)
    }
}

// ---------------------------------------------------------------------------
// Status registers (IEEE 488.2 §11)
// ---------------------------------------------------------------------------

/// Status Byte Register (STB) bits.
pub mod stb {
    /// Error/Event Queue not empty.
    pub const ERR_QUEUE: u8 = 0x04;
    /// Questionable Status summary.
    pub const QUESTIONABLE: u8 = 0x08;
    /// Message Available (MAV).
    pub const MAV: u8 = 0x10;
    /// Event Status Bit (ESB).
    pub const ESB: u8 = 0x20;
    /// Master Summary Status / Request for Service (RQS).
    pub const MSS_RQS: u8 = 0x40;
    /// Operation Status summary.
    pub const OPERATION: u8 = 0x80;
}

/// Event Status Register (ESR) bits.
pub mod esr {
    /// Operation Complete.
    pub const OPC: u8 = 0x01;
    /// Request Control.
    pub const RQC: u8 = 0x02;
    /// Query Error.
    pub const QYE: u8 = 0x04;
    /// Device Dependent Error.
    pub const DDE: u8 = 0x08;
    /// Execution Error.
    pub const EXE: u8 = 0x10;
    /// Command Error.
    pub const CME: u8 = 0x20;
    /// User Request.
    pub const URQ: u8 = 0x40;
    /// Power On.
    pub const PON: u8 = 0x80;
}

// ---------------------------------------------------------------------------
// Ieee488State
// ---------------------------------------------------------------------------

/// Holds the mutable state required by the IEEE 488.2 common commands.
#[derive(Debug)]
pub struct Ieee488State {
    /// Identification string parts.
    pub identification: Identification,
    /// Event Status Enable register.
    pub ese: u8,
    /// Event Status Register.
    pub esr: u8,
    /// Service Request Enable register.
    pub sre: u8,
    /// Status Byte register (augmented dynamically in `compute_stb`).
    pub stb: u8,
    /// Last self-test result (0 = pass, non-zero = fail).
    pub self_test_result: i32,
}

impl Ieee488State {
    /// Create state with the given identification and a clean register set.
    pub fn new(identification: Identification) -> Self {
        Ieee488State {
            identification,
            ese: 0,
            esr: esr::PON, // Power-on bit set at startup (IEEE 488.2 §11.5.1).
            sre: 0,
            stb: 0,
            self_test_result: 0,
        }
    }

    /// Compute the live Status Byte value.
    pub fn compute_stb(&self, error_queue: &ErrorQueue) -> u8 {
        let mut val = self.stb;
        if !error_queue.is_empty() {
            val |= stb::ERR_QUEUE;
        }
        if self.esr & self.ese != 0 {
            val |= stb::ESB;
        }
        if val & self.sre != 0 {
            val |= stb::MSS_RQS;
        } else {
            val &= !stb::MSS_RQS;
        }
        val
    }
}

// ---------------------------------------------------------------------------
// Command handler
// ---------------------------------------------------------------------------

/// Dispatch a single IEEE 488.2 common command.
///
/// Returns `Ok(Response)` on success, or `Err(UNDEFINED_HEADER)` when the
/// command is not a common command (so the caller can try device-specific
/// handlers).
pub fn handle_common_command(
    cmd: &Command,
    state: &mut Ieee488State,
    error_queue: &mut ErrorQueue,
) -> Result<Response, ScpiError> {
    let h = cmd.header.to_ascii_uppercase();

    match h.as_str() {
        "*IDN" if cmd.is_query => {
            Ok(Response::Str(state.identification.to_string()))
        }

        "*RST" if !cmd.is_query => {
            state.ese = 0;
            state.esr = esr::PON;
            state.sre = 0;
            state.stb = 0;
            Ok(Response::Empty)
        }

        "*CLS" if !cmd.is_query => {
            state.esr = 0;
            error_queue.clear();
            Ok(Response::Empty)
        }

        "*ESE" if !cmd.is_query => {
            let param = cmd.params.first().ok_or(MISSING_PARAMETER)?;
            let val = param.as_integer().ok_or(MISSING_PARAMETER)?;
            state.ese = (val & 0xFF) as u8;
            Ok(Response::Empty)
        }

        "*ESE" if cmd.is_query => Ok(Response::Integer(state.ese as i64)),

        "*ESR" if cmd.is_query => {
            let val = state.esr;
            state.esr = 0; // Reading clears the register.
            Ok(Response::Integer(val as i64))
        }

        "*OPC" if !cmd.is_query => {
            state.esr |= esr::OPC;
            Ok(Response::Empty)
        }

        "*OPC" if cmd.is_query => Ok(Response::Integer(1)),

        "*SRE" if !cmd.is_query => {
            let param = cmd.params.first().ok_or(MISSING_PARAMETER)?;
            let val = param.as_integer().ok_or(MISSING_PARAMETER)?;
            state.sre = (val & 0xFF) as u8;
            Ok(Response::Empty)
        }

        "*SRE" if cmd.is_query => Ok(Response::Integer(state.sre as i64)),

        "*STB" if cmd.is_query => {
            Ok(Response::Integer(state.compute_stb(error_queue) as i64))
        }

        "*TST" if cmd.is_query => {
            Ok(Response::Integer(state.self_test_result as i64))
        }

        "*WAI" if !cmd.is_query => {
            // Synchronous no-op in a single-threaded implementation.
            Ok(Response::Empty)
        }

        _ => Err(UNDEFINED_HEADER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn make_state() -> Ieee488State {
        Ieee488State::new(Identification {
            manufacturer: "ACME".into(),
            model: "XT1".into(),
            serial: "SN001".into(),
            version: "1.0".into(),
        })
    }

    #[test]
    fn idn_query() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        let cmds = parse("*IDN?");
        let resp = handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        assert_eq!(resp, Response::Str("ACME,XT1,SN001,1.0".into()));
    }

    #[test]
    fn rst_clears_registers() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        state.ese = 0xFF;
        state.sre = 0xFF;
        let cmds = parse("*RST");
        handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        assert_eq!(state.ese, 0);
        assert_eq!(state.sre, 0);
    }

    #[test]
    fn cls_clears_esr_and_error_queue() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        state.esr = 0xFF;
        eq.push(crate::error::COMMAND_ERROR);
        let cmds = parse("*CLS");
        handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        assert_eq!(state.esr, 0);
        assert!(eq.is_empty());
    }

    #[test]
    fn ese_set_and_query() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        let cmds = parse("*ESE 32");
        handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        let cmds = parse("*ESE?");
        let resp = handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        assert_eq!(resp, Response::Integer(32));
    }

    #[test]
    fn esr_query_clears_register() {
        let mut state = make_state();
        state.esr = esr::PON | esr::CME;
        let mut eq = ErrorQueue::new();
        let cmds = parse("*ESR?");
        let resp = handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        assert_eq!(resp, Response::Integer((esr::PON | esr::CME) as i64));
        assert_eq!(state.esr, 0);
    }

    #[test]
    fn opc_command_sets_bit() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        state.esr = 0;
        let cmds = parse("*OPC");
        handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        assert_ne!(state.esr & esr::OPC, 0);
    }

    #[test]
    fn opc_query_returns_1() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        let cmds = parse("*OPC?");
        let resp = handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        assert_eq!(resp, Response::Integer(1));
    }

    #[test]
    fn sre_set_and_query() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        let cmds = parse("*SRE 16");
        handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        let cmds = parse("*SRE?");
        let resp = handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        assert_eq!(resp, Response::Integer(16));
    }

    #[test]
    fn stb_reflects_error_queue() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        eq.push(crate::error::COMMAND_ERROR);
        let cmds = parse("*STB?");
        let resp = handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        if let Response::Integer(v) = resp {
            assert_ne!(v as u8 & stb::ERR_QUEUE, 0);
        } else {
            panic!("Expected integer response");
        }
    }

    #[test]
    fn tst_query_returns_zero_by_default() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        let cmds = parse("*TST?");
        let resp = handle_common_command(&cmds[0], &mut state, &mut eq).unwrap();
        assert_eq!(resp, Response::Integer(0));
    }

    #[test]
    fn unknown_common_command_returns_undefined_header_err() {
        let mut state = make_state();
        let mut eq = ErrorQueue::new();
        let cmds = parse("*XYZ?");
        let result = handle_common_command(&cmds[0], &mut state, &mut eq);
        assert_eq!(result, Err(UNDEFINED_HEADER));
    }
}
