//! Transport adapters for SCPI over TCP network links.
//!
//! Enable the **`tcp`** Cargo feature to compile [`TcpClient`] and [`TcpServer`].
//!
//! # TCP (SCPI-RAW)
//!
//! Both [`TcpClient`] and [`TcpServer`] use the *SCPI-RAW* protocol: plain TCP
//! with one SCPI message per line (terminated by `\n` or `\r\n`).  Port
//! **5025** is the IANA-registered port for this protocol.
//!
//! ## Connecting to an instrument (scope, DMM, …) from a PC
//!
//! Use [`TcpClient`] when your PC is the *controller* and the instrument is
//! already running a SCPI server (e.g. a Rigol oscilloscope on port 5025):
//!
//! ```no_run
//! use scpify::transport::TcpClient;
//!
//! let mut scope = TcpClient::connect("192.168.1.100:5025").expect("connection failed");
//!
//! // Query the instrument identification.
//! let idn = scope.query("*IDN?").expect("query failed");
//! println!("Connected to: {}", idn);
//!
//! // Send a command (no response expected).
//! scope.send(":RUN").expect("send failed");
//!
//! // Query and parse a numeric measurement.
//! let volts = scope.query_f64(":MEASure:VOLTage:DC?").expect("measurement failed");
//! println!("DC voltage: {} V", volts);
//! ```
//!
//! ## Hosting a SCPI server (sequential)
//!
//! Use [`TcpServer`] when *this* application implements the instrument and
//! handles one client at a time:
//!
//! ```no_run
//! use scpify::{Device, Identification};
//! use scpify::transport::TcpServer;
//!
//! let mut device = Device::new(Identification::default());
//! let server = TcpServer::bind("127.0.0.1:5025").expect("bind failed");
//! server.serve(&mut device).expect("server error");
//! ```
//!
//! ## Hosting a SCPI server (concurrent)
//!
//! Spawns a thread per connection, sharing the `Device` behind an
//! `Arc<Mutex<_>>`:
//!
//! ```no_run
//! use scpify::{Device, Identification};
//! use scpify::transport::TcpServer;
//!
//! let device = Device::new(Identification::default());
//! let server = TcpServer::bind("0.0.0.0:5025").expect("bind failed");
//! server.serve_concurrent(device).expect("server error");
//! ```

#[cfg(feature = "tcp")]
mod tcp {
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::{Device, Response};

    /// Default read timeout applied to every new [`TcpClient`] connection.
    ///
    /// If an instrument sends no response within this window, [`TcpClient::query`]
    /// returns an [`io::Error`] with kind [`io::ErrorKind::TimedOut`] (or
    /// [`io::ErrorKind::WouldBlock`] on some platforms) instead of blocking
    /// forever.  Override it per-client with [`TcpClient::set_read_timeout`].
    pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

    // -----------------------------------------------------------------------
    // TcpServer
    // -----------------------------------------------------------------------

    /// A SCPI-RAW TCP server.
    ///
    /// Listens for incoming TCP connections and processes SCPI messages
    /// line by line.  Each `\n`- or `\r\n`-terminated line is treated as one
    /// SCPI message.  Query responses are sent back as `\n`-terminated UTF-8
    /// strings; non-query commands produce no output on the wire.
    ///
    /// Construct with [`TcpServer::bind`], then call [`TcpServer::serve`]
    /// (sequential) or [`TcpServer::serve_concurrent`] (one thread per
    /// connection).
    pub struct TcpServer {
        listener: TcpListener,
    }

    impl TcpServer {
        /// Bind to the given address.
        ///
        /// Pass any value accepted by [`std::net::TcpListener::bind`], e.g.
        /// `"0.0.0.0:5025"` or `"127.0.0.1:0"` (OS-assigned port, useful for
        /// tests).
        pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
            let listener = TcpListener::bind(addr)?;
            Ok(TcpServer { listener })
        }

        /// Return the local socket address the server is listening on.
        ///
        /// Useful when the server was bound to port `0` (OS-chosen port).
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.listener.local_addr()
        }

        /// Serve connections **sequentially**: one client is handled to
        /// completion before the next is accepted.
        ///
        /// This call blocks indefinitely (until an I/O error on the listener
        /// itself).  Use [`TcpServer::serve_concurrent`] when you need to
        /// handle multiple simultaneous clients.
        pub fn serve(&self, device: &mut Device) -> io::Result<()> {
            for stream in self.listener.incoming() {
                match stream {
                    Ok(s) => {
                        // Ignore per-client errors so the server keeps running.
                        let _ = serve_client(s, device);
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        /// Serve connections **concurrently** by spawning a thread per client.
        ///
        /// The [`Device`] is shared across threads behind an
        /// `Arc<Mutex<Device>>`.  This call blocks indefinitely.
        pub fn serve_concurrent(self, device: Device) -> io::Result<()> {
            let shared = Arc::new(Mutex::new(device));
            for stream in self.listener.incoming() {
                match stream {
                    Ok(s) => {
                        let dev = Arc::clone(&shared);
                        std::thread::spawn(move || {
                            let _ = serve_client_shared(s, dev);
                        });
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    impl std::fmt::Debug for TcpServer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TcpServer")
                .field("local_addr", &self.listener.local_addr().ok())
                .finish()
        }
    }

    // -----------------------------------------------------------------------
    // TcpClient
    // -----------------------------------------------------------------------

    /// A SCPI-RAW TCP client.
    ///
    /// Connect to an instrument (oscilloscope, multimeter, power supply, …)
    /// that is already running a SCPI server and send/receive SCPI messages.
    ///
    /// Construct with [`TcpClient::connect`], then use [`TcpClient::send`] for
    /// commands that produce no response and [`TcpClient::query`] (or
    /// [`TcpClient::query_f64`]) for query commands that return a value.
    ///
    /// A read timeout of [`DEFAULT_READ_TIMEOUT`] (10 s) is set automatically
    /// on every new connection so that [`TcpClient::query`] returns an error
    /// instead of hanging when an instrument sends no response.  Call
    /// [`TcpClient::set_read_timeout`] to adjust or disable the timeout.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use scpify::transport::TcpClient;
    ///
    /// let mut scope = TcpClient::connect("192.168.1.100:5025").unwrap();
    /// println!("{}", scope.query("*IDN?").unwrap());
    /// let v = scope.query_f64(":MEASure:VOLTage:DC?").unwrap();
    /// println!("DC voltage: {} V", v);
    /// ```
    pub struct TcpClient {
        writer: TcpStream,
        reader: BufReader<TcpStream>,
    }

    impl TcpClient {
        /// Connect to a SCPI instrument at `addr`.
        ///
        /// Pass any address accepted by [`std::net::TcpStream::connect`], e.g.
        /// `"192.168.1.100:5025"` or `"scope.local:5025"`.
        ///
        /// A read timeout of [`DEFAULT_READ_TIMEOUT`] is set on the connection
        /// automatically.  Use [`TcpClient::set_read_timeout`] to change it.
        pub fn connect(addr: impl ToSocketAddrs) -> io::Result<Self> {
            let stream = TcpStream::connect(addr)?;
            let reader_stream = stream.try_clone()?;
            // Set the default read timeout so that query() never hangs
            // indefinitely when the instrument sends no response.
            reader_stream.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
            let reader = BufReader::new(reader_stream);
            Ok(TcpClient {
                writer: stream,
                reader,
            })
        }

        /// Set the read timeout for query responses.
        ///
        /// The timeout applies to [`TcpClient::query`] and
        /// [`TcpClient::query_f64`].  When the timeout expires before a
        /// response line arrives, those methods return an [`io::Error`] with
        /// kind [`io::ErrorKind::TimedOut`] (or [`io::ErrorKind::WouldBlock`]
        /// on some platforms).
        ///
        /// Pass `None` to disable the timeout entirely.  **This is not
        /// recommended** because a non-responsive instrument will then block
        /// your thread indefinitely.
        pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            self.reader.get_ref().set_read_timeout(timeout)
        }

        /// Send a command that produces **no response** (e.g. `*RST`, `:RUN`).
        ///
        /// The command string must **not** end with `?`.  A newline is appended
        /// automatically.
        pub fn send(&mut self, command: &str) -> io::Result<()> {
            writeln!(self.writer, "{}", command)
        }

        /// Send a query command and return the instrument's response as a
        /// `String`.
        ///
        /// The command should end with `?` (e.g. `"*IDN?"`,
        /// `":MEASure:VOLTage:DC?"`).  Leading/trailing whitespace is stripped
        /// from the returned string.
        ///
        /// If the instrument does not respond within the read timeout (default
        /// [`DEFAULT_READ_TIMEOUT`], configurable with
        /// [`TcpClient::set_read_timeout`]), this method returns an
        /// [`io::Error`] with kind [`io::ErrorKind::TimedOut`] (or
        /// [`io::ErrorKind::WouldBlock`] on some platforms).
        pub fn query(&mut self, command: &str) -> io::Result<String> {
            writeln!(self.writer, "{}", command)?;
            let mut line = String::new();
            self.reader.read_line(&mut line)?;
            Ok(line.trim().to_string())
        }

        /// Send a query command and parse the response as an `f64`.
        ///
        /// Returns an [`io::Error`] with kind [`io::ErrorKind::InvalidData`] if
        /// the response cannot be parsed as a floating-point number.
        ///
        /// Inherits the read timeout from [`TcpClient::query`].
        pub fn query_f64(&mut self, command: &str) -> io::Result<f64> {
            let raw = self.query(command)?;
            raw.parse::<f64>().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected a numeric response, got {:?}: {}", raw, e),
                )
            })
        }

        /// Send a query command that supports the IEEE 488.2 binary block format
        /// Parser identifies the "#" header, caluclates the data lenghth and bypasses
        /// UTF-8 string conversion using a
        /// Bufreader to peak at the incoming stream to determine if
        /// the response is actually a binary block.
        pub fn query_raw(&mut self, command: &str) -> io::Result<Vec<u8>> {
            // 1. Send the command
            self.send(command)?;

            // 2. Read first byte
            let mut start_char = [0u8; 1];
            self.reader.read_exact(&mut start_char)?;
            let start_char = start_char[0];

            if start_char == b'#' {
                // 3. Read digit count
                let mut digit_buf = [0u8; 1];
                self.reader.read_exact(&mut digit_buf)?;
                let digit = digit_buf[0];

                let digit_count = (digit as char).to_digit(10).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Invalid block header digit")
                })? as usize;

                // Indefinite-length block (#0 ... \n)
                if digit_count == 0 {
                    let mut data = Vec::new();
                    self.reader.read_until(b'\n', &mut data)?;
                    return Ok(data);
                }

                // 4. Read length field
                let mut len_buf = vec![0u8; digit_count];
                self.reader.read_exact(&mut len_buf)?;

                let len_str = std::str::from_utf8(&len_buf).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Non-numeric length field")
                })?;

                let length = len_str.parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Failed to parse data length")
                })?;

                // 5. Read payload
                let mut payload = vec![0u8; length];
                self.reader.read_exact(&mut payload)?;

                // 6. Consume trailing newline (handle \n or \r\n)
                let mut trailing = [0u8; 1];
                if self.reader.read(&mut trailing).is_ok() {
                    if trailing[0] == b'\r' {
                        let _ = self.reader.read(&mut trailing); // consume '\n'
                    }
                }

                Ok(payload)
            } else {
                // FALLBACK: ASCII response
                let mut response = Vec::new();
                response.push(start_char);
                self.reader.read_until(b'\n', &mut response)?;
                Ok(response)
            }
        }
    }

    impl std::fmt::Debug for TcpClient {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TcpClient")
                .field("peer_addr", &self.writer.peer_addr().ok())
                .finish()
        }
    }

    // -----------------------------------------------------------------------
    // Client handlers (server-side helpers)
    // -----------------------------------------------------------------------

    /// Read lines from `stream`, process each as a SCPI message, and write
    /// any query responses back.
    fn serve_client(stream: TcpStream, device: &mut Device) -> io::Result<()> {
        let mut writer = stream.try_clone()?;
        let reader = BufReader::new(stream);
        process_lines(reader, &mut writer, |msg| device.process(msg))
    }

    /// Same as [`serve_client`] but acquires a `Mutex` lock per message so
    /// that the `Device` can be shared across threads.
    fn serve_client_shared(stream: TcpStream, device: Arc<Mutex<Device>>) -> io::Result<()> {
        let mut writer = stream.try_clone()?;
        let reader = BufReader::new(stream);
        process_lines(reader, &mut writer, |msg| {
            device
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .process(msg)
        })
    }

    /// Core line-processing loop shared by both client handlers.
    fn process_lines<R, W, F>(
        reader: BufReader<R>,
        writer: &mut W,
        mut dispatch: F,
    ) -> io::Result<()>
    where
        R: io::Read,
        W: Write,
        F: FnMut(&str) -> Vec<Response>,
    {
        for line in reader.lines() {
            let line = line?;
            let msg = line.trim();
            if msg.is_empty() {
                continue;
            }
            for response in dispatch(msg) {
                if response != Response::Empty {
                    writeln!(writer, "{}", response)?;
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::{Identification, Response};
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;

        fn test_device() -> Device {
            let mut dev = Device::new(Identification {
                manufacturer: "TestCo".into(),
                model: "T1".into(),
                serial: "001".into(),
                version: "0.1".into(),
            });
            // Custom handler: VOLT? → 3.3
            dev.register(|cmd| {
                if cmd.matches_header("MEASure:VOLTage") && cmd.is_query {
                    Some(Response::Float(3.3))
                } else {
                    None
                }
            });
            dev
        }

        /// Start a sequential server in a background thread; return the bound port.
        ///
        /// A channel is used to signal once the server has entered its accept loop,
        /// eliminating the race condition that a fixed sleep would introduce.
        fn start_sequential(device: Device) -> u16 {
            let server = TcpServer::bind("127.0.0.1:0").expect("bind");
            let port = server.local_addr().unwrap().port();
            // Clone the listener so we can probe readiness without sending through the channel.
            // The simplest race-free approach: connect once with a retry loop.
            std::thread::spawn(move || {
                let mut dev = device;
                let _ = server.serve(&mut dev);
            });
            wait_for_port(port);
            port
        }

        /// Start a concurrent server in a background thread; return the bound port.
        fn start_concurrent(device: Device) -> u16 {
            let server = TcpServer::bind("127.0.0.1:0").expect("bind");
            let port = server.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let _ = server.serve_concurrent(device);
            });
            wait_for_port(port);
            port
        }

        /// Spin until a TCP connection to `port` on loopback succeeds (meaning the
        /// server thread has entered its accept loop).
        fn wait_for_port(port: u16) {
            let addr = format!("127.0.0.1:{}", port);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if TcpStream::connect(&addr as &str).is_ok() {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "server did not start in time"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        fn connect(port: u16) -> (impl Write, impl BufRead) {
            let stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
            let reader = BufReader::new(stream.try_clone().unwrap());
            (stream, reader)
        }

        fn send_recv(writer: &mut impl Write, reader: &mut impl BufRead, msg: &str) -> String {
            writeln!(writer, "{}", msg).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            line.trim().to_string()
        }

        #[test]
        fn sequential_idn_query() {
            let port = start_sequential(test_device());
            let (mut w, mut r) = connect(port);
            let resp = send_recv(&mut w, &mut r, "*IDN?");
            assert_eq!(resp, "\"TestCo,T1,001,0.1\"");
        }

        #[test]
        fn sequential_custom_query() {
            let port = start_sequential(test_device());
            let (mut w, mut r) = connect(port);
            let resp = send_recv(&mut w, &mut r, ":MEASure:VOLTage?");
            // Response::Float formats as SCPI scientific notation; parse and compare numerically.
            let value: f64 = resp
                .parse()
                .expect("expected a numeric response, got: {resp}");
            assert!((value - 3.3).abs() < 1e-9, "expected ~3.3, got {}", value);
        }

        #[test]
        fn sequential_non_query_produces_no_output() {
            let port = start_sequential(test_device());
            let stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_millis(150)))
                .unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            writeln!(writer, "*RST").unwrap();
            let mut buf = String::new();
            // The server sends nothing for non-query commands — read_line should time out.
            let result = reader.read_line(&mut buf);
            assert!(
                result.is_err() || buf.is_empty(),
                "unexpected output: {:?}",
                buf
            );
        }

        #[test]
        fn sequential_compound_message() {
            let port = start_sequential(test_device());
            let (mut w, mut r) = connect(port);
            // Compound: RST (no output) then IDN? (one output line).
            writeln!(w, "*RST;*IDN?").unwrap();
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            assert!(line.contains("TestCo"), "{}", line);
        }

        #[test]
        fn concurrent_idn_query() {
            let port = start_concurrent(test_device());
            let (mut w, mut r) = connect(port);
            let resp = send_recv(&mut w, &mut r, "*IDN?");
            assert_eq!(resp, "\"TestCo,T1,001,0.1\"");
        }

        #[test]
        fn concurrent_multiple_clients() {
            let port = start_concurrent(test_device());

            let handles: Vec<_> = (0..4)
                .map(|_| {
                    std::thread::spawn(move || {
                        let (mut w, mut r) = connect(port);
                        send_recv(&mut w, &mut r, "*IDN?")
                    })
                })
                .collect();

            for h in handles {
                let resp = h.join().expect("thread panicked");
                assert!(resp.contains("TestCo"), "{}", resp);
            }
        }

        #[test]
        fn debug_impl() {
            let server = TcpServer::bind("127.0.0.1:0").unwrap();
            let s = format!("{:?}", server);
            assert!(s.contains("TcpServer"));
        }

        // --- TcpClient tests -----------------------------------------------

        #[test]
        fn client_query() {
            let port = start_sequential(test_device());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let resp = client.query("*IDN?").unwrap();
            assert!(resp.contains("TestCo"), "{}", resp);
        }

        #[test]
        fn client_query_f64() {
            let port = start_sequential(test_device());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let v = client.query_f64(":MEASure:VOLTage?").unwrap();
            assert!((v - 3.3).abs() < 1e-9, "expected ~3.3, got {}", v);
        }

        #[test]
        fn client_send_no_response() {
            let port = start_sequential(test_device());
            // send() should not block waiting for a response.
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client.send("*RST").unwrap();
            // Follow-up query should still work.
            let resp = client.query("*IDN?").unwrap();
            assert!(resp.contains("TestCo"), "{}", resp);
        }

        #[test]
        fn client_debug_impl() {
            let port = start_sequential(test_device());
            let client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let s = format!("{:?}", client);
            assert!(s.contains("TcpClient"));
        }

        /// Verify the default read timeout is applied and returns an error
        /// (rather than hanging) when the server never sends a response line.
        ///
        /// Uses a raw listener that accepts the connection but never writes
        /// back, and a very short timeout so the test completes quickly.
        #[test]
        fn client_query_times_out_when_no_response() {
            // Start a "silent" TCP server: accept connections but never reply.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                // Accept one connection and park it — never write anything back.
                let (_conn, _addr) = listener.accept().unwrap();
                // Drop _conn at end of test; keep thread alive just long enough.
                std::thread::sleep(Duration::from_secs(5));
            });

            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            // Use a short timeout so the test finishes quickly.
            client
                .set_read_timeout(Some(Duration::from_millis(150)))
                .unwrap();

            let err = client.query("*IDN?").unwrap_err();
            // Both TimedOut and WouldBlock are valid timeout error kinds
            // depending on the OS.
            assert!(
                err.kind() == io::ErrorKind::TimedOut || err.kind() == io::ErrorKind::WouldBlock,
                "expected a timeout error, got: {:?}",
                err
            );
        }

        /// Verify that set_read_timeout(None) disables the timeout (the socket
        /// option is cleared without error).
        #[test]
        fn client_set_read_timeout_none() {
            let port = start_sequential(test_device());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            // Disabling the timeout should not error.
            client.set_read_timeout(None).unwrap();
            // A normal query must still work with no timeout set.
            let resp = client.query("*IDN?").unwrap();
            assert!(resp.contains("TestCo"), "{}", resp);
        }
    }
}

#[cfg(feature = "tcp")]
pub use tcp::{TcpClient, TcpServer, DEFAULT_READ_TIMEOUT};
