# scpify

A Rust library for sending and receiving **SCPI** (Standard Commands for Programmable Instruments / IEEE 488.2) messages over TCP and other transports.

Use it to **control instruments from a PC** (connect to a scope, DMM, or power supply and send queries) or to **implement a SCPI server** (expose your own device over the network).

---

## Features

| Feature | Description |
|---|---|
| **`TcpClient`** *(feature `tcp`)* | Connect from a PC to any SCPI instrument over TCP; send commands and read responses |
| **`TcpServer`** *(feature `tcp`)* | Host a SCPI server so other programs can control your device over TCP |
| **Message parser** | Tokenise and parse SCPI strings into typed `Command` structs, including compound messages (`"*RST;*IDN?"`) |
| **Mnemonic matching** | Both short form (`MEAS`) and long form (`MEASure`) accepted, case-insensitively |
| **Response types** | Strongly-typed `Response` values formatted to the SCPI standard |
| **IEEE 488.2 built-ins** | `*IDN?`, `*RST`, `*CLS`, `*ESE[?]`, `*ESR?`, `*OPC[?]`, `*SRE[?]`, `*STB?`, `*TST?`, `*WAI` |
| **Error queue** | SCPI-standard FIFO error queue with standard error codes |

---

## Installation

Add `scpify` to your `Cargo.toml` with the `tcp` feature enabled:

```toml
[dependencies]
scpify = { version = "0.1", features = ["tcp"] }
```

If you only need the message parser and don't need network I/O (e.g. for an embedded device), omit the feature:

```toml
[dependencies]
scpify = "0.1"
```

---

## Quick start — connecting to a scope from a PC

This is the most common use case: your PC connects to an oscilloscope (or any SCPI instrument) over TCP and sends queries.

```rust
use scpify::transport::TcpClient;

fn main() {
    // Connect to your scope — use its IP address and SCPI port (usually 5025).
    let mut scope = TcpClient::connect("192.168.1.100:5025")
        .expect("could not connect to scope");

    // Ask for its identification string.
    let idn = scope.query("*IDN?").expect("query failed");
    println!("Connected to: {}", idn);
    // → "RIGOL TECHNOLOGIES,DS1054Z,DS1ZA…,00.04.04"

    // Send a non-query command (no response expected).
    scope.send(":RUN").expect("send failed");

    // Read a measurement and parse it as a number.
    let volts = scope.query_f64(":MEASure:VOLTage:DC?")
        .expect("measurement failed");
    println!("DC voltage: {} V", volts);
}
```

> **Timeout protection:** `TcpClient` sets a **10-second read timeout** by
> default, so `query()` returns an error instead of hanging if the instrument
> sends no response.  Override it with `scope.set_read_timeout(Some(Duration::from_secs(30)))`,
> or disable it entirely with `scope.set_read_timeout(None)`.

### Checking the connection works

The quickest way to verify your instrument is reachable — no Rust code needed:

**Using `nc` (netcat) — Linux and macOS:**

```bash
echo '*IDN?' | nc 192.168.1.100 5025
# → "RIGOL TECHNOLOGIES,DS1054Z,…"
```

**Using Python** (cross-platform, no extra packages):

```python
import socket

def scpi(host, port, command):
    with socket.create_connection((host, port)) as s:
        s.sendall((command + '\n').encode())
        return s.recv(4096).decode().strip()

print(scpi('192.168.1.100', 5025, '*IDN?'))
print(scpi('192.168.1.100', 5025, ':MEASure:VOLTage:DC?'))
```

**Using PowerShell** (Windows):

```powershell
$client = [System.Net.Sockets.TcpClient]::new('192.168.1.100', 5025)
$stream = $client.GetStream()
$writer = [System.IO.StreamWriter]::new($stream)
$reader = [System.IO.StreamReader]::new($stream)

$writer.WriteLine('*IDN?'); $writer.Flush()
$reader.ReadLine()

$client.Close()
```

> **Tip:** Non-query commands (e.g. `*RST`, `:RUN`, `:STOP`) produce no response
> line. Only commands ending in `?` return a value.

---

## Quick start — hosting a SCPI server

Use `TcpServer` when *your application* needs to behave like an instrument so
that other programs (or physical controllers) can talk to it over SCPI.

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

    // Register custom command handlers.
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

For multi-client support use `serve_concurrent` (the `Device` is shared
automatically via `Arc<Mutex<_>>`):

```rust
server.serve_concurrent(device).expect("server error");
```

---

## Quick start — in-process (no network)

Parse and dispatch SCPI messages entirely in memory — no sockets, no threads:

```rust
use scpify::{Device, Identification, Response};
use scpify::command::Command;

let mut device = Device::new(Identification {
    manufacturer: "ACME".into(),
    model: "XT1".into(),
    serial: "SN001".into(),
    version: "1.0".into(),
});

device.register(|cmd: &Command| {
    if cmd.matches_header("MEASure:VOLTage:DC") && cmd.is_query {
        Some(Response::Float(3.3))
    } else {
        None
    }
});

let responses = device.process("*IDN?;:MEASure:VOLTage:DC?");
assert_eq!(responses.len(), 2);
// responses[0] → Response::Str("ACME,XT1,SN001,1.0")
// responses[1] → Response::Float(3.3)
```

---

## Protocol reference

`scpify` uses the **SCPI-RAW** framing convention (IANA port 5025):

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
PC / test script                        Instrument / server
──────────────────                      ───────────────────
TcpClient::connect()  ←─── TCP ───→    TcpServer::bind()
TcpClient::query()    ─── "*IDN?" ─→   Device::process()
                      ←─ response ──   registered handlers / ieee488
TcpClient::query_f64()─── "MEAS?" ─→
                      ←── "3.3E0" ──
```

The `transport` module (feature `tcp`) connects the network layer to the
`Device` dispatcher. The `Device` parses each line, matches it against
registered handlers and IEEE 488.2 built-ins, and returns typed `Response`
values that are written back on the wire.

---

## License

MIT — see [LICENSE](LICENSE).

