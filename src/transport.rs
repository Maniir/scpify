//! Transport adapters for SCPI over TCP network links.
//!
//! Enable the **`tcp`** Cargo feature to compile [`TcpClient`] and [`TcpServer`].
//!
//! Enable the **`hislip`** Cargo feature to compile [`HislipClient`] and
//! [`HislipServer`], which implement the IVI-6.1 HiSLIP protocol with
//! credit-based flow control.
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

        /// Send a query command that returns an IEEE 488.2 binary block
        /// response.
        ///
        /// The parser identifies the `#` header, calculates the data length,
        /// and bypasses UTF-8 string conversion, using a `BufReader` to peek
        /// at the incoming stream to determine whether the response is a
        /// binary block.
        ///
        /// # Streaming (multiple sequential blocks)
        ///
        /// Some instruments (e.g. Keysight UXR with `:STReaming ON`) respond
        /// to a single query with **multiple consecutive** definite-length
        /// binary blocks (`#NL…L<data>#NL…L<data>…\n`).  This method
        /// transparently handles that case: after reading each block's
        /// payload it peeks at the next byte, and if another `#` header
        /// follows it reads the next block and concatenates the data.  The
        /// accumulated payload from all blocks is returned as a single
        /// `Vec<u8>`.
        pub fn query_raw(&mut self, command: &str) -> io::Result<Vec<u8>> {
            // Send the command
            self.send(command)?;

            // Read first byte
            let mut start_char = [0u8; 1];
            self.reader.read_exact(&mut start_char)?;
            let start_char = start_char[0];

            if start_char == b'#' {
                let mut all_data = Vec::new();

                // Loop handles the first block and any subsequent streaming
                // blocks that follow back-to-back.
                loop {
                    // Read digit count
                    let mut digit_buf = [0u8; 1];
                    self.reader.read_exact(&mut digit_buf)?;
                    let digit = digit_buf[0];

                    let digit_count = (digit as char).to_digit(10).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "Invalid block header digit")
                    })? as usize;

                    // Indefinite-length block (#0 ... \n) — terminates the
                    // stream; no further blocks can follow.
                    if digit_count == 0 {
                        self.reader.read_until(b'\n', &mut all_data)?;
                        return Ok(all_data);
                    }

                    // Read length field
                    let mut len_buf = vec![0u8; digit_count];
                    self.reader.read_exact(&mut len_buf)?;

                    let len_str = std::str::from_utf8(&len_buf).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "Non-numeric length field")
                    })?;

                    let length = len_str.parse::<usize>().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "Failed to parse data length")
                    })?;

                    // Read payload
                    let mut payload = vec![0u8; length];
                    self.reader.read_exact(&mut payload)?;
                    all_data.extend_from_slice(&payload);

                    // Peek at the next byte to decide whether another block
                    // follows (streaming) or the response is complete.
                    let buf = self.reader.fill_buf()?;
                    if buf.is_empty() {
                        // Connection closed or EOF — return what we have.
                        break;
                    }

                    match buf[0] {
                        b'#' => {
                            // Another block follows — consume the '#' and loop.
                            self.reader.consume(1);
                            continue;
                        }
                        b'\n' => {
                            self.reader.consume(1);
                            break;
                        }
                        b'\r' => {
                            self.reader.consume(1);
                            // Consume the '\n' that follows '\r'.
                            let buf = self.reader.fill_buf()?;
                            if !buf.is_empty() && buf[0] == b'\n' {
                                self.reader.consume(1);
                            }
                            break;
                        }
                        _ => {
                            // Unexpected byte after payload — stop reading.
                            break;
                        }
                    }
                }

                Ok(all_data)
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

        // --- query_raw tests -----------------------------------------------

        /// Bind a listener and spawn a thread that accepts one connection,
        /// reads one command line, writes `response`, then exits.
        ///
        /// The listener is already bound before spawning, so the OS will queue
        /// the client's connection in the backlog even before `accept()` is
        /// called — no explicit readiness barrier is needed.
        fn start_raw_server(response: Vec<u8>) -> u16 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line); // consume the query command
                    let _ = stream.write_all(&response);
                }
            });
            port
        }

        /// Definite-length binary block with a single-digit length field.
        ///
        /// Wire format: `#15HELLO\n`
        ///   `#`  – binary block marker
        ///   `1`  – one digit encodes the length
        ///   `5`  – length = 5
        ///   `HELLO` – 5-byte payload
        ///   `\n` – trailing terminator (must not appear in the returned data)
        #[test]
        fn client_query_raw_definite_block() {
            let port = start_raw_server(b"#15HELLO\n".to_vec());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(2000)))
                .unwrap();
            let data = client.query_raw("FETCH:TRACE?").unwrap();
            assert_eq!(data, b"HELLO");
        }

        /// Definite-length block whose trailing terminator is `\r\n` instead of
        /// just `\n`.  Both bytes must be consumed so they do not appear in the
        /// returned payload.
        #[test]
        fn client_query_raw_definite_block_crlf() {
            let port = start_raw_server(b"#15HELLO\r\n".to_vec());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(2000)))
                .unwrap();
            let data = client.query_raw("FETCH:TRACE?").unwrap();
            assert_eq!(data, b"HELLO");
        }

        /// Definite-length block with a two-digit length field (`#2NN…`).
        #[test]
        fn client_query_raw_definite_block_two_digit_length() {
            let payload = b"abcdefghijkl"; // 12 bytes
            let mut response = b"#212".to_vec(); // '#' + '2' digits + "12"
            response.extend_from_slice(payload);
            response.push(b'\n');
            let port = start_raw_server(response);
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(2000)))
                .unwrap();
            let data = client.query_raw("FETCH:TRACE?").unwrap();
            assert_eq!(data.as_slice(), payload.as_ref());
        }

        /// Indefinite-length block (`#0`): data is read until `\n`, and that
        /// terminating newline is included in the returned bytes.
        #[test]
        fn client_query_raw_indefinite_block() {
            let port = start_raw_server(b"#0binary data\n".to_vec());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(2000)))
                .unwrap();
            let data = client.query_raw("FETCH:TRACE?").unwrap();
            assert_eq!(data, b"binary data\n");
        }

        /// ASCII fallback: when the first response byte is not `#`, the entire
        /// line (including the trailing `\n`) is returned verbatim.
        #[test]
        fn client_query_raw_ascii_fallback() {
            let port = start_raw_server(b"3.14159\n".to_vec());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(2000)))
                .unwrap();
            let data = client.query_raw("MEAS:VOLT?").unwrap();
            assert_eq!(data, b"3.14159\n");
        }

        // --- streaming (multi-block) query_raw tests -----------------------

        /// Two back-to-back definite-length blocks followed by `\n`.
        ///
        /// Wire format: `#15HELLO#15WORLD\n`
        /// Expected: concatenated payload `HELLOWORLD`.
        #[test]
        fn client_query_raw_streaming_two_blocks() {
            let port = start_raw_server(b"#15HELLO#15WORLD\n".to_vec());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(2000)))
                .unwrap();
            let data = client.query_raw("FETCH:WAV?").unwrap();
            assert_eq!(data, b"HELLOWORLD");
        }

        /// Three blocks with different payload sizes, terminated by `\r\n`.
        ///
        /// Wire format: `#12AB#13CDE#11F\r\n`
        /// Expected: concatenated payload `ABCDEF`.
        #[test]
        fn client_query_raw_streaming_three_blocks_crlf() {
            let port = start_raw_server(b"#12AB#13CDE#11F\r\n".to_vec());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(2000)))
                .unwrap();
            let data = client.query_raw("FETCH:WAV?").unwrap();
            assert_eq!(data, b"ABCDEF");
        }

        /// Streaming blocks with a two-digit length field to exercise the
        /// multi-digit header path in the loop.
        ///
        /// Wire format: `#212abcdefghijkl#15HELLO\n`
        /// Expected: concatenated payloads (12 + 5 = 17 bytes).
        #[test]
        fn client_query_raw_streaming_multi_digit_length() {
            let response = b"#212abcdefghijkl#15HELLO\n".to_vec();
            let port = start_raw_server(response);
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(2000)))
                .unwrap();
            let data = client.query_raw("FETCH:WAV?").unwrap();
            assert_eq!(data, b"abcdefghijklHELLO");
        }

        /// A single block still works as before (no regression).
        #[test]
        fn client_query_raw_single_block_still_works() {
            let port = start_raw_server(b"#15HELLO\n".to_vec());
            let mut client = TcpClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(2000)))
                .unwrap();
            let data = client.query_raw("FETCH:TRACE?").unwrap();
            assert_eq!(data, b"HELLO");
        }
    }
}

#[cfg(feature = "tcp")]
pub use tcp::{TcpClient, TcpServer, DEFAULT_READ_TIMEOUT};

// =======================================================================
// HiSLIP (IVI-6.1)
// =======================================================================

#[cfg(feature = "hislip")]
mod hislip {
    use std::io::{self, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::{Device, Response};

    // -------------------------------------------------------------------
    // Constants
    // -------------------------------------------------------------------

    /// Default HiSLIP TCP port (IVI-6.1 §2).
    pub const DEFAULT_HISLIP_PORT: u16 = 4880;

    /// Default read timeout for HiSLIP connections.
    pub const DEFAULT_HISLIP_READ_TIMEOUT: Duration = Duration::from_secs(10);

    /// HiSLIP header prologue — the ASCII bytes `HS` (0x4853).
    const PROLOGUE: [u8; 2] = [b'H', b'S'];

    /// Protocol version advertised and expected.
    const PROTOCOL_VERSION_MAJOR: u8 = 1;
    const PROTOCOL_VERSION_MINOR: u8 = 0;

    /// Default number of initial credits granted by the server to the client
    /// in the `InitializeResponse` message (IVI-6.1 §3.1).  A value of 1
    /// lets the client send one Data/DataEnd message before waiting for a
    /// server response that replenishes credits.
    const DEFAULT_INITIAL_CREDITS: u8 = 1;

    // -------------------------------------------------------------------
    // Message types (IVI-6.1 Table 2)
    // -------------------------------------------------------------------

    /// HiSLIP message type codes (IVI-6.1 Table 2).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    enum MessageType {
        Initialize = 0,
        InitializeResponse = 1,
        Data = 2,
        DataEnd = 3,
        AsyncInitialize = 6,
        AsyncInitializeResponse = 7,
        AsyncMaximumMessageSize = 8,
        AsyncMaximumMessageSizeResponse = 9,
    }

    impl MessageType {
        fn from_u8(val: u8) -> io::Result<Self> {
            match val {
                0 => Ok(Self::Initialize),
                1 => Ok(Self::InitializeResponse),
                2 => Ok(Self::Data),
                3 => Ok(Self::DataEnd),
                6 => Ok(Self::AsyncInitialize),
                7 => Ok(Self::AsyncInitializeResponse),
                8 => Ok(Self::AsyncMaximumMessageSize),
                9 => Ok(Self::AsyncMaximumMessageSizeResponse),
                other => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown HiSLIP message type: {}", other),
                )),
            }
        }
    }

    // -------------------------------------------------------------------
    // Message
    // -------------------------------------------------------------------

    /// A single HiSLIP message (header + optional payload).
    ///
    /// Wire format (IVI-6.1 §2.2):
    /// ```text
    ///  0      1      2      3      4      5      6      7
    /// +------+------+------+------+------+------+------+------+
    /// | 'H'  | 'S'  | type |ctrl_c|   message_parameter      |
    /// +------+------+------+------+------+------+------+------+
    /// |              payload_length (u64)                      |
    /// +------+------+------+------+------+------+------+------+
    /// |              payload (variable)                        |
    /// +------+------+------+------+------+------+------+------+
    /// ```
    #[derive(Debug, Clone)]
    struct Message {
        message_type: MessageType,
        control_code: u8,
        message_parameter: u32,
        payload: Vec<u8>,
    }

    impl Message {
        /// Build a new [`Message`] with the given fields and an empty payload.
        fn new(message_type: MessageType, control_code: u8, message_parameter: u32) -> Self {
            Message {
                message_type,
                control_code,
                message_parameter,
                payload: Vec::new(),
            }
        }

        /// Build a new [`Message`] with a payload.
        fn with_payload(
            message_type: MessageType,
            control_code: u8,
            message_parameter: u32,
            payload: Vec<u8>,
        ) -> Self {
            Message {
                message_type,
                control_code,
                message_parameter,
                payload,
            }
        }

        /// Serialise the message into its wire representation.
        fn to_bytes(&self) -> Vec<u8> {
            let mut buf = Vec::with_capacity(16 + self.payload.len());
            buf.extend_from_slice(&PROLOGUE);
            buf.push(self.message_type as u8);
            buf.push(self.control_code);
            buf.extend_from_slice(&self.message_parameter.to_be_bytes());
            buf.extend_from_slice(&(self.payload.len() as u64).to_be_bytes());
            buf.extend_from_slice(&self.payload);
            buf
        }

        /// Read a [`Message`] from the given reader.
        fn read_from(reader: &mut impl Read) -> io::Result<Self> {
            let mut header = [0u8; 16];
            reader.read_exact(&mut header)?;

            // Validate prologue.
            if header[0] != PROLOGUE[0] || header[1] != PROLOGUE[1] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid HiSLIP prologue: expected 'HS', got '{}{}'",
                        header[0] as char, header[1] as char
                    ),
                ));
            }

            let message_type = MessageType::from_u8(header[2])?;
            let control_code = header[3];
            let message_parameter =
                u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
            let payload_length =
                u64::from_be_bytes([
                    header[8], header[9], header[10], header[11],
                    header[12], header[13], header[14], header[15],
                ]);

            let mut payload = vec![0u8; payload_length as usize];
            if payload_length > 0 {
                reader.read_exact(&mut payload)?;
            }

            Ok(Message {
                message_type,
                control_code,
                message_parameter,
                payload,
            })
        }

        /// Write this message to the given writer.
        fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
            writer.write_all(&self.to_bytes())
        }
    }

    // -------------------------------------------------------------------
    // Protocol helpers
    // -------------------------------------------------------------------

    /// Build an `Initialize` message (client → server).
    ///
    /// `message_parameter` layout (IVI-6.1 §3.1):
    ///   bits 31‒16: client protocol version (major << 8 | minor)
    ///   bits 15‒0 : client vendor ID (we use 0)
    fn init_request() -> Message {
        let version = ((PROTOCOL_VERSION_MAJOR as u32) << 8)
            | (PROTOCOL_VERSION_MINOR as u32);
        let param = version << 16; // vendor ID = 0
        Message::new(MessageType::Initialize, 0, param)
    }

    /// Build an `InitializeResponse` message (server → client).
    ///
    /// `control_code` carries the **initial credit grant** (IVI-6.1 §3.1,
    /// Table 4).  A non-zero value tells the client how many Data/DataEnd
    /// messages it may send before waiting for additional credits.
    ///
    /// `message_parameter` layout (IVI-6.1 §3.1):
    ///   bits 31‒16: overlap mode & session_id (overlap in bit 0 of upper
    ///               byte, session_id in lower 16 bits — packed as
    ///               `(overlap << 24) | (session_id << 16)`)
    ///               Actually per spec: bits 31‒16 = session_id,
    ///               bits 15‒8 = server protocol version major,
    ///               bits  7‒0 = server protocol version minor.
    fn init_response(session_id: u16, initial_credits: u8) -> Message {
        let overlap_mode = 0u8; // synchronised mode
        let response_param = ((session_id as u32) << 16)
            | ((PROTOCOL_VERSION_MAJOR as u32) << 8)
            | (PROTOCOL_VERSION_MINOR as u32);
        // Per IVI-6.1, control_code in InitializeResponse carries the
        // initial credit count.
        let _ = overlap_mode; // synchronised mode — encoded as 0 in bit 0 of control_code is separate from credits
        // The overlap mode is encoded in the upper bit of control_code (bit 0).
        // We combine it with the credit count. Since overlap_mode == 0
        // (synchronised), control_code == initial_credits.
        Message::new(
            MessageType::InitializeResponse,
            initial_credits,
            response_param,
        )
    }

    /// Build an `AsyncInitialize` message (client → server, async channel).
    ///
    /// `message_parameter` carries the session_id obtained from the
    /// `InitializeResponse`.
    #[allow(dead_code)]
    fn async_init_request(session_id: u16) -> Message {
        Message::new(
            MessageType::AsyncInitialize,
            0,
            session_id as u32,
        )
    }

    /// Build an `AsyncInitializeResponse` message (server → client).
    ///
    /// The server echoes back a vendor ID (0 for us).
    #[allow(dead_code)]
    fn async_init_response() -> Message {
        Message::new(MessageType::AsyncInitializeResponse, 0, 0)
    }

    // -------------------------------------------------------------------
    // HislipServer
    // -------------------------------------------------------------------

    /// A HiSLIP server implementing the IVI-6.1 protocol.
    ///
    /// Accepts sync and async channel connections, performs the
    /// Initialize/InitializeResponse handshake (granting initial credits),
    /// then processes SCPI messages received in Data/DataEnd messages.
    ///
    /// # Credit management
    ///
    /// The server grants [`DEFAULT_INITIAL_CREDITS`] credits in the
    /// `InitializeResponse` message.  After processing each Data/DataEnd
    /// message the server replenishes credits by including a credit count
    /// in the `control_code` of response messages.
    pub struct HislipServer {
        listener: TcpListener,
        initial_credits: u8,
    }

    impl HislipServer {
        /// Bind a HiSLIP server to the given address.
        ///
        /// The default HiSLIP port is [`DEFAULT_HISLIP_PORT`] (4880).
        pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
            let listener = TcpListener::bind(addr)?;
            Ok(HislipServer {
                listener,
                initial_credits: DEFAULT_INITIAL_CREDITS,
            })
        }

        /// Set the number of initial credits granted to clients in the
        /// `InitializeResponse`.
        ///
        /// Must be at least 1 so clients are allowed to send data.
        /// Panics if `credits` is 0.
        pub fn set_initial_credits(&mut self, credits: u8) {
            assert!(credits > 0, "initial credits must be at least 1");
            self.initial_credits = credits;
        }

        /// Return the local socket address the server is listening on.
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.listener.local_addr()
        }

        /// Serve connections **sequentially** (one client at a time).
        ///
        /// This call blocks indefinitely.
        pub fn serve(&self, device: &mut Device) -> io::Result<()> {
            let mut next_session_id: u16 = 1;
            for stream in self.listener.incoming() {
                match stream {
                    Ok(s) => {
                        let session_id = next_session_id;
                        next_session_id = next_session_id.wrapping_add(1);
                        if next_session_id == 0 {
                            next_session_id = 1;
                        }
                        let _ = handle_hislip_client(s, device, session_id, self.initial_credits);
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        /// Serve connections **concurrently** (one thread per client).
        pub fn serve_concurrent(self, device: Device) -> io::Result<()> {
            let shared = Arc::new(Mutex::new(device));
            let mut next_session_id: u16 = 1;
            for stream in self.listener.incoming() {
                match stream {
                    Ok(s) => {
                        let dev = Arc::clone(&shared);
                        let session_id = next_session_id;
                        let initial_credits = self.initial_credits;
                        next_session_id = next_session_id.wrapping_add(1);
                        if next_session_id == 0 {
                            next_session_id = 1;
                        }
                        std::thread::spawn(move || {
                            let _ = handle_hislip_client_shared(
                                s,
                                dev,
                                session_id,
                                initial_credits,
                            );
                        });
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    impl std::fmt::Debug for HislipServer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("HislipServer")
                .field("local_addr", &self.listener.local_addr().ok())
                .field("initial_credits", &self.initial_credits)
                .finish()
        }
    }

    /// Handle a single HiSLIP client on the sync channel.
    fn handle_hislip_client(
        mut stream: TcpStream,
        device: &mut Device,
        session_id: u16,
        initial_credits: u8,
    ) -> io::Result<()> {
        // Step 1: Read Initialize from client.
        let init_msg = Message::read_from(&mut stream)?;
        if init_msg.message_type != MessageType::Initialize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected Initialize message",
            ));
        }

        // Step 2: Send InitializeResponse with initial credits.
        let resp = init_response(session_id, initial_credits);
        resp.write_to(&mut stream)?;

        // Step 3: Process Data/DataEnd messages.
        hislip_message_loop(&mut stream, |msg| device.process(msg))
    }

    /// Handle a single HiSLIP client with shared device (concurrent mode).
    fn handle_hislip_client_shared(
        mut stream: TcpStream,
        device: Arc<Mutex<Device>>,
        session_id: u16,
        initial_credits: u8,
    ) -> io::Result<()> {
        let init_msg = Message::read_from(&mut stream)?;
        if init_msg.message_type != MessageType::Initialize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected Initialize message",
            ));
        }

        let resp = init_response(session_id, initial_credits);
        resp.write_to(&mut stream)?;

        hislip_message_loop(&mut stream, |msg| {
            device
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .process(msg)
        })
    }

    /// Core loop: read Data/DataEnd messages, dispatch SCPI, send responses.
    ///
    /// After each response the server grants 1 credit back to the client via
    /// the `control_code` field, allowing the client to send the next message.
    fn hislip_message_loop<F>(stream: &mut TcpStream, mut dispatch: F) -> io::Result<()>
    where
        F: FnMut(&str) -> Vec<Response>,
    {
        let mut pending_data = Vec::new();

        loop {
            let msg = match Message::read_from(stream) {
                Ok(m) => m,
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            };

            match msg.message_type {
                MessageType::Data => {
                    // Accumulate payload; don't process until DataEnd.
                    pending_data.extend_from_slice(&msg.payload);
                }
                MessageType::DataEnd => {
                    pending_data.extend_from_slice(&msg.payload);

                    // Convert to UTF-8 SCPI message.
                    let scpi_msg = String::from_utf8_lossy(&pending_data);
                    let scpi_msg = scpi_msg.trim();

                    if !scpi_msg.is_empty() {
                        let responses = dispatch(scpi_msg);
                        for response in responses {
                            if response != Response::Empty {
                                let payload = format!("{}\n", response).into_bytes();
                                // Grant 1 credit back in the control_code.
                                let reply = Message::with_payload(
                                    MessageType::DataEnd,
                                    1, // credit replenishment
                                    0,
                                    payload,
                                );
                                reply.write_to(stream)?;
                            }
                        }
                    }

                    pending_data.clear();
                }
                MessageType::AsyncMaximumMessageSize => {
                    // Echo back the same max message size.
                    let reply = Message::new(
                        MessageType::AsyncMaximumMessageSizeResponse,
                        0,
                        0,
                    );
                    reply.write_to(stream)?;
                }
                _ => {
                    // Ignore other message types on the sync channel.
                }
            }
        }

        Ok(())
    }

    // -------------------------------------------------------------------
    // HislipClient
    // -------------------------------------------------------------------

    /// A HiSLIP client implementing the IVI-6.1 protocol.
    ///
    /// Connects to a HiSLIP server, performs the Initialize/InitializeResponse
    /// handshake, parses the initial credit grant, and tracks credits to
    /// ensure Data/DataEnd messages are only sent when credits are available.
    ///
    /// # Credit tracking
    ///
    /// The client:
    /// 1. Parses the initial credit count from the `control_code` of the
    ///    `InitializeResponse` message.
    /// 2. Decrements the credit count when sending a Data or DataEnd message.
    /// 3. Replenishes credits when receiving response messages from the server
    ///    (the `control_code` of the response carries the credit grant).
    ///
    /// If the client has no credits remaining, [`HislipClient::send`] and
    /// [`HislipClient::query`] return an error rather than violating the
    /// protocol by sending without permission.
    pub struct HislipClient {
        stream: TcpStream,
        /// Session ID assigned by the server.
        session_id: u16,
        /// Available message credits.
        credits: u32,
    }

    impl HislipClient {
        /// Connect to a HiSLIP server, perform the initialization handshake,
        /// and return a client ready to send SCPI messages.
        ///
        /// The server's `InitializeResponse` grants initial credits; the
        /// client stores them for flow-control enforcement.
        pub fn connect(addr: impl ToSocketAddrs) -> io::Result<Self> {
            let mut stream = TcpStream::connect(addr)?;
            stream.set_read_timeout(Some(DEFAULT_HISLIP_READ_TIMEOUT))?;

            // Send Initialize.
            let init = init_request();
            init.write_to(&mut stream)?;

            // Read InitializeResponse.
            let resp = Message::read_from(&mut stream)?;
            if resp.message_type != MessageType::InitializeResponse {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "expected InitializeResponse, got {:?}",
                        resp.message_type
                    ),
                ));
            }

            // Parse session_id from bits 31‒16 of message_parameter.
            let session_id = (resp.message_parameter >> 16) as u16;

            // Parse initial credits from control_code (IVI-6.1 §3.1).
            let initial_credits = resp.control_code as u32;
            if initial_credits == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "server granted zero initial credits",
                ));
            }

            Ok(HislipClient {
                stream,
                session_id,
                credits: initial_credits,
            })
        }

        /// Return the session ID assigned by the server.
        pub fn session_id(&self) -> u16 {
            self.session_id
        }

        /// Return the number of available credits.
        pub fn credits(&self) -> u32 {
            self.credits
        }

        /// Set the read timeout for responses.
        pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            self.stream.set_read_timeout(timeout)
        }

        /// Send a SCPI command (no response expected).
        ///
        /// Returns an error if no credits are available.
        pub fn send(&mut self, command: &str) -> io::Result<()> {
            self.require_credit()?;
            let msg = Message::with_payload(
                MessageType::DataEnd,
                0,
                0,
                command.as_bytes().to_vec(),
            );
            msg.write_to(&mut self.stream)?;
            self.credits -= 1;
            Ok(())
        }

        /// Send a SCPI query and return the response string.
        ///
        /// Returns an error if no credits are available.
        pub fn query(&mut self, command: &str) -> io::Result<String> {
            self.require_credit()?;
            let msg = Message::with_payload(
                MessageType::DataEnd,
                0,
                0,
                command.as_bytes().to_vec(),
            );
            msg.write_to(&mut self.stream)?;
            self.credits -= 1;

            // Read the response.
            let resp = Message::read_from(&mut self.stream)?;

            // Replenish credits from the response's control_code.
            self.credits += resp.control_code as u32;

            let text = String::from_utf8_lossy(&resp.payload);
            Ok(text.trim().to_string())
        }

        /// Send a SCPI query and parse the response as `f64`.
        pub fn query_f64(&mut self, command: &str) -> io::Result<f64> {
            let raw = self.query(command)?;
            raw.parse::<f64>().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected numeric response, got {:?}: {}", raw, e),
                )
            })
        }

        /// Check that at least one credit is available; return an error if not.
        fn require_credit(&self) -> io::Result<()> {
            if self.credits == 0 {
                return Err(io::Error::other(
                    "no HiSLIP credits available — cannot send data",
                ));
            }
            Ok(())
        }
    }

    impl std::fmt::Debug for HislipClient {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("HislipClient")
                .field("peer_addr", &self.stream.peer_addr().ok())
                .field("session_id", &self.session_id)
                .field("credits", &self.credits)
                .finish()
        }
    }

    // -------------------------------------------------------------------
    // Tests
    // -------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::Identification;
        use std::io::Cursor;

        fn test_device() -> Device {
            let mut dev = Device::new(Identification {
                manufacturer: "TestCo".into(),
                model: "T1".into(),
                serial: "001".into(),
                version: "0.1".into(),
            });
            dev.register(|cmd| {
                if cmd.matches_header("MEASure:VOLTage") && cmd.is_query {
                    Some(Response::Float(3.3))
                } else {
                    None
                }
            });
            dev
        }

        // -- Message serialisation / deserialisation -----------------------

        #[test]
        fn message_round_trip_no_payload() {
            let msg = Message::new(MessageType::Initialize, 42, 0x0001_0200);
            let bytes = msg.to_bytes();
            assert_eq!(bytes.len(), 16);
            assert_eq!(&bytes[0..2], b"HS");

            let mut cursor = Cursor::new(bytes);
            let parsed = Message::read_from(&mut cursor).unwrap();
            assert_eq!(parsed.message_type, MessageType::Initialize);
            assert_eq!(parsed.control_code, 42);
            assert_eq!(parsed.message_parameter, 0x0001_0200);
            assert!(parsed.payload.is_empty());
        }

        #[test]
        fn message_round_trip_with_payload() {
            let payload = b"*IDN?\n".to_vec();
            let msg = Message::with_payload(
                MessageType::DataEnd,
                1,
                0,
                payload.clone(),
            );
            let bytes = msg.to_bytes();
            assert_eq!(bytes.len(), 16 + payload.len());

            let mut cursor = Cursor::new(bytes);
            let parsed = Message::read_from(&mut cursor).unwrap();
            assert_eq!(parsed.message_type, MessageType::DataEnd);
            assert_eq!(parsed.control_code, 1);
            assert_eq!(parsed.payload, payload);
        }

        #[test]
        fn bad_prologue_is_rejected() {
            let mut bad = vec![b'X', b'Y']; // wrong prologue
            bad.extend_from_slice(&[0u8; 14]);
            let mut cursor = Cursor::new(bad);
            let err = Message::read_from(&mut cursor).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }

        // -- init_response carries credits ---------------------------------

        #[test]
        fn init_response_encodes_credits() {
            let msg = init_response(0x1234, 5);
            assert_eq!(msg.message_type, MessageType::InitializeResponse);
            // control_code must carry the initial credit count.
            assert_eq!(msg.control_code, 5);
            // session_id in bits 31-16.
            assert_eq!((msg.message_parameter >> 16) as u16, 0x1234);
            // Protocol version in bits 15-0.
            let major = ((msg.message_parameter >> 8) & 0xFF) as u8;
            let minor = (msg.message_parameter & 0xFF) as u8;
            assert_eq!(major, PROTOCOL_VERSION_MAJOR);
            assert_eq!(minor, PROTOCOL_VERSION_MINOR);
        }

        #[test]
        fn init_response_default_credits_non_zero() {
            let msg = init_response(1, DEFAULT_INITIAL_CREDITS);
            assert!(msg.control_code > 0, "default credits must be > 0");
        }

        // -- Full client-server handshake + query --------------------------

        fn start_hislip_server(device: Device) -> u16 {
            let server = HislipServer::bind("127.0.0.1:0").expect("bind");
            let port = server.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let mut dev = device;
                let _ = server.serve(&mut dev);
            });
            wait_for_port(port);
            port
        }

        fn start_hislip_server_with_credits(device: Device, credits: u8) -> u16 {
            let mut server = HislipServer::bind("127.0.0.1:0").expect("bind");
            server.set_initial_credits(credits);
            let port = server.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let mut dev = device;
                let _ = server.serve(&mut dev);
            });
            wait_for_port(port);
            port
        }

        fn wait_for_port(port: u16) {
            let addr = format!("127.0.0.1:{}", port);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if TcpStream::connect(&addr as &str).is_ok() {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "server did not start in time"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        #[test]
        fn client_connects_and_receives_credits() {
            let port = start_hislip_server(test_device());
            let client = HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            assert!(
                client.credits() > 0,
                "client should have received initial credits"
            );
            assert_ne!(client.session_id(), 0);
        }

        #[test]
        fn client_receives_custom_credits() {
            let port = start_hislip_server_with_credits(test_device(), 5);
            let client = HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            assert_eq!(client.credits(), 5);
        }

        #[test]
        fn client_query_idn() {
            let port = start_hislip_server(test_device());
            let mut client = HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let initial = client.credits();
            let resp = client.query("*IDN?").unwrap();
            assert!(resp.contains("TestCo"), "unexpected: {}", resp);
            // Credits should have been decremented then replenished.
            assert_eq!(client.credits(), initial); // 1 - 1 + 1 = 1
        }

        #[test]
        fn client_query_f64_works() {
            let port = start_hislip_server(test_device());
            let mut client = HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let v = client.query_f64(":MEASure:VOLTage?").unwrap();
            assert!((v - 3.3).abs() < 1e-9, "expected ~3.3, got {}", v);
        }

        #[test]
        fn client_send_consumes_credit() {
            let port = start_hislip_server_with_credits(test_device(), 5);
            let mut client = HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            assert_eq!(client.credits(), 5);
            // send() consumes 1 credit without waiting for replenishment.
            client.send("*RST").unwrap();
            assert_eq!(client.credits(), 4);
        }

        #[test]
        fn client_no_credits_returns_error() {
            // Create a client manually with 0 credits to test enforcement.
            let port = start_hislip_server(test_device());
            let mut client = HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            // Drain credits.
            client.credits = 0;
            let err = client.send("*RST").unwrap_err();
            assert!(
                err.to_string().contains("credits"),
                "expected credit error, got: {}",
                err
            );
        }

        #[test]
        fn server_debug_impl() {
            let server = HislipServer::bind("127.0.0.1:0").unwrap();
            let s = format!("{:?}", server);
            assert!(s.contains("HislipServer"));
            assert!(s.contains("initial_credits"));
        }

        #[test]
        fn client_debug_impl() {
            let port = start_hislip_server(test_device());
            let client = HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let s = format!("{:?}", client);
            assert!(s.contains("HislipClient"));
            assert!(s.contains("credits"));
            assert!(s.contains("session_id"));
        }

        #[test]
        #[should_panic(expected = "initial credits must be at least 1")]
        fn server_rejects_zero_credits() {
            let mut server = HislipServer::bind("127.0.0.1:0").unwrap();
            server.set_initial_credits(0);
        }
    }
}

#[cfg(feature = "hislip")]
pub use hislip::{
    HislipClient, HislipServer, DEFAULT_HISLIP_PORT, DEFAULT_HISLIP_READ_TIMEOUT,
};
