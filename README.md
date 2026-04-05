# scpify

A Rust library for sending and receiving **SCPI** (Standard Commands for Programmable Instruments / IEEE 488.2) messages.

Embed it in instrument firmware, desktop drivers, or test-automation frameworks to add complete SCPI support with zero external dependencies.

---

## Features

| Feature | Description |
|---|---|
| **Message parser** | Tokenise and parse SCPI strings into typed `Command` structs, including compound messages (`"*RST;*IDN?"`) |
| **Mnemonic matching** | Both short form (`MEAS`) and long form (`MEASure`) accepted, case-insensitively |
| **Response types** | Strongly-typed `Response` values formatted to the SCPI standard |
| **IEEE 488.2 built-ins** | `*IDN?`, `*RST`, `*CLS`, `*ESE[?]`, `*ESR?`, `*OPC[?]`, `*SRE[?]`, `*STB?`, `*TST?`, `*WAI` |
| **Error queue** | SCPI-standard FIFO error queue with standard error codes |
| **`Device` dispatcher** | Routes messages to registered handlers or IEEE 488.2 built-ins |
| **TCP transport** *(feature `tcp`)* | `TcpServer` serves SCPI-RAW over TCP (port 5025), sequentially or concurrently |

---

## Installation

Add `scpify` to your `Cargo.toml`.

**Without network transport** (embedded / parse-only):
```toml
[dependencies]
scpify = "0.1"
```

**With TCP transport** (desktop server / test rack):
```toml
[dependencies]
scpify = { version = "0.1", features = ["tcp"] }
```

---

## Quick start — in-process (no network)

```rust
use scpify::{Device, Identification, Response};
use scpify::command::Command;

// 1. Build a device with its identification string.
let mut device = Device::new(Identification {
    manufacturer: "ACME".into(),
    model: "XT1".into(),
    serial: "SN001".into(),
    version: "1.0".into(),
});

// 2. Register a custom query handler.
device.register(|cmd: &Command| {
    if cmd.matches_header("MEASure:VOLTage:DC") && cmd.is_query {
        Some(Response::Float(3.3))
    } else {
        None
    }
});

// 3. Process a compound message.
let responses = device.process("*IDN?;:MEASure:VOLTage:DC?");
assert_eq!(responses.len(), 2);
// responses[0] → Response::Str("ACME,XT1,SN001,1.0")
// responses[1] → Response::Float(3.3)
```

---

## Quick start — TCP server (SCPI-RAW over the network)

Enable the `tcp` feature (see [Installation](#installation)) then start a server with a few lines:

```rust
use scpify::{Device, Identification, Response};
use scpify::command::Command;
use scpify::transport::TcpServer;

fn main() {
    let mut device = Device::new(Identification {
        manufacturer: "ACME".into(),
        model: "XT1".into(),
        serial: "SN001".into(),
        version: "1.0".into(),
    });

    // Register your instrument handlers.
    device.register(|cmd: &Command| {
        if cmd.matches_header("MEASure:VOLTage:DC") && cmd.is_query {
            Some(Response::Float(3.3))
        } else {
            None
        }
    });

    // Bind on the standard SCPI-RAW port and serve clients one at a time.
    let server = TcpServer::bind("0.0.0.0:5025").expect("failed to bind");
    println!("Listening on port 5025…");
    server.serve(&mut device).expect("server error");
}
```

For multi-client support, swap `serve` for `serve_concurrent` (the `Device` is
shared across threads automatically via `Arc<Mutex<_>>`):

```rust
server.serve_concurrent(device).expect("server error");
```

### Checking that it works

Once the server is running you can verify it from any terminal — no special
tools required.

**Using `nc` (netcat) — available on Linux and macOS:**

```bash
# Ask for the instrument identification string
echo '*IDN?' | nc 127.0.0.1 5025
# → "ACME,XT1,SN001,1.0"

# Send a measurement query
echo ':MEASure:VOLTage:DC?' | nc 127.0.0.1 5025
# → 3.300000E0

# Check the error queue
echo 'SYSTem:ERRor?' | nc 127.0.0.1 5025
```

**Using Python** (cross-platform, no extra packages needed):

```python
import socket

def scpi(host, port, command):
    with socket.create_connection((host, port)) as s:
        s.sendall((command + '\n').encode())
        return s.recv(4096).decode().strip()

print(scpi('127.0.0.1', 5025, '*IDN?'))
# → "ACME,XT1,SN001,1.0"

print(scpi('127.0.0.1', 5025, ':MEASure:VOLTage:DC?'))
# → 3.300000E0
```

**Using PowerShell** (Windows):

```powershell
$client = [System.Net.Sockets.TcpClient]::new('127.0.0.1', 5025)
$stream = $client.GetStream()
$writer = [System.IO.StreamWriter]::new($stream)
$reader = [System.IO.StreamReader]::new($stream)

$writer.WriteLine('*IDN?'); $writer.Flush()
$reader.ReadLine()   # → "ACME,XT1,SN001,1.0"

$client.Close()
```

> **Tip:** Non-query commands (e.g. `*RST`, `*CLS`) produce no output on the
> wire. Only query commands (ending in `?`) receive a response line.

---

## Protocol reference

`scpify` implements the **SCPI-RAW** framing convention:

* Each message is a UTF-8 string terminated by `\n` (or `\r\n`).
* Multiple sub-commands may be separated by `;` in a single message line.
* Query responses are returned one per line, in message order.

### IEEE 488.2 common commands (built-in)

| Command | Description |
|---|---|
| `*IDN?` | Identification query |
| `*RST` | Reset device state |
| `*CLS` | Clear status registers and error queue |
| `*ESE <val>` / `*ESE?` | Event Status Enable register |
| `*ESR?` | Event Status Register (clears on read) |
| `*OPC` / `*OPC?` | Operation Complete |
| `*SRE <val>` / `*SRE?` | Service Request Enable register |
| `*STB?` | Status Byte |
| `*TST?` | Self-test (returns `0` by default) |
| `*WAI` | Wait-to-continue |

### Error codes

Standard SCPI error codes are defined in `scpify::error`:

| Constant | Code | Description |
|---|---|---|
| `NO_ERROR` | 0 | No error |
| `COMMAND_ERROR` | −100 | Command error |
| `UNDEFINED_HEADER` | −113 | Undefined header |
| `MISSING_PARAMETER` | −109 | Missing parameter |
| `EXECUTION_ERROR` | −200 | Execution error |
| `DATA_OUT_OF_RANGE` | −222 | Data out of range |
| `QUERY_ERROR` | −400 | Query error |

---

## Architecture

```
┌────────────────────────────────────────────┐
│               Your application             │
│         (firmware / driver / test)         │
└────────────────────┬───────────────────────┘
                     │ raw SCPI string
                     ▼
              ┌─────────────┐
              │  tokeniser  │  src/token.rs
              └──────┬──────┘
                     │ token stream
                     ▼
              ┌─────────────┐
              │   parser    │  src/parser.rs
              └──────┬──────┘
                     │ Vec<Command>
                     ▼
              ┌─────────────┐
              │   Device    │  src/lib.rs
              │  dispatcher │
              └──────┬──────┘
            ┌────────┴────────┐
            ▼                 ▼
    IEEE 488.2 built-ins   User handlers
    src/ieee488.rs         (closures you register)
            │                 │
            └────────┬────────┘
                     │ Vec<Response>
                     ▼
          (returned / sent on wire)
```

The `transport` module (feature `tcp`) sits *above* the `Device` — it reads
lines from a TCP socket, calls `device.process()`, and writes response lines
back.

---

## License

MIT — see [LICENSE](LICENSE).

