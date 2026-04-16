//! Transport adapters for SCPI over network links.
//!
//! Enable the **`tcp`** Cargo feature to compile [`TcpClient`] and [`TcpServer`].
//! Enable the **`hislip`** Cargo feature to compile [`HislipClient`] and
//! [`HislipServer`].
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
//! # #[cfg(feature = "tcp")]
//! # fn main() {
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
//! # }
//! # #[cfg(not(feature = "tcp"))]
//! # fn main() {}
//! ```
//!
//! ## Hosting a SCPI server (sequential)
//!
//! Use [`TcpServer`] when *this* application implements the instrument and
//! handles one client at a time:
//!
//! ```no_run
//! # #[cfg(feature = "tcp")]
//! # fn main() {
//! use scpify::{Device, Identification};
//! use scpify::transport::TcpServer;
//!
//! let mut device = Device::new(Identification::default());
//! let server = TcpServer::bind("127.0.0.1:5025").expect("bind failed");
//! server.serve(&mut device).expect("server error");
//! # }
//! # #[cfg(not(feature = "tcp"))]
//! # fn main() {}
//! ```
//!
//! ## Hosting a SCPI server (concurrent)
//!
//! Spawns a thread per connection, sharing the `Device` behind an
//! `Arc<Mutex<_>>`:
//!
//! ```no_run
//! # #[cfg(feature = "tcp")]
//! # fn main() {
//! use scpify::{Device, Identification};
//! use scpify::transport::TcpServer;
//!
//! let device = Device::new(Identification::default());
//! let server = TcpServer::bind("0.0.0.0:5025").expect("bind failed");
//! server.serve_concurrent(device).expect("server error");
//! # }
//! # #[cfg(not(feature = "tcp"))]
//! # fn main() {}
//! ```
//!
//! # HiSLIP (IVI-6.1)
//!
//! [`HislipClient`] and [`HislipServer`] implement the *High-Speed LAN
//! Instrument Protocol* (HiSLIP) defined by IVI-6.1.  HiSLIP uses a binary
//! framing protocol over TCP (IANA port **4880**).  Each session consists of
//! two TCP connections — a *synchronous* channel for request-response message
//! exchange and an *asynchronous* channel for out-of-band control.
//!
//! ## Connecting via HiSLIP
//!
//! ```no_run
//! # #[cfg(feature = "hislip")]
//! # fn main() {
//! use scpify::transport::HislipClient;
//!
//! let mut inst = HislipClient::connect("192.168.1.100:4880")
//!     .expect("connection failed");
//! println!("{}", inst.query("*IDN?").unwrap());
//! # }
//! # #[cfg(not(feature = "hislip"))]
//! # fn main() {}
//! ```
//!
//! ## Hosting a HiSLIP server
//!
//! ```no_run
//! # #[cfg(feature = "hislip")]
//! # fn main() {
//! use scpify::{Device, Identification};
//! use scpify::transport::HislipServer;
//!
//! let mut device = Device::new(Identification::default());
//! let server = HislipServer::bind("0.0.0.0:4880").expect("bind failed");
//! server.serve(&mut device).expect("server error");
//! # }
//! # #[cfg(not(feature = "hislip"))]
//! # fn main() {}
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
            // Send the command
            self.send(command)?;

            // Read first byte
            let mut start_char = [0u8; 1];
            self.reader.read_exact(&mut start_char)?;
            let start_char = start_char[0];

            if start_char == b'#' {
                // Read digit count
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

                // Consume trailing newline (handle \n or \r\n)
                let mut trailing = [0u8; 1];
                if self.reader.read(&mut trailing).is_ok() && trailing[0] == b'\r' {
                    let _ = self.reader.read(&mut trailing); // consume '\n'
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
    }
}

#[cfg(feature = "tcp")]
pub use tcp::{TcpClient, TcpServer, DEFAULT_READ_TIMEOUT};

// ===================================================================
// HiSLIP transport (IVI-6.1)
// ===================================================================

#[cfg(feature = "hislip")]
mod hislip {
    use std::collections::HashMap;
    use std::io::{self, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::{Device, Response};

    // -------------------------------------------------------------------
    // Constants
    // -------------------------------------------------------------------

    /// "HS" prologue that begins every HiSLIP message header.
    const PROLOGUE: [u8; 2] = [b'H', b'S'];

    /// Size of a HiSLIP message header in bytes.
    const HEADER_SIZE: usize = 16;

    /// IANA-registered port for HiSLIP (IVI-6.1 §2).
    pub const DEFAULT_HISLIP_PORT: u16 = 4880;

    /// Default read timeout applied to every new [`HislipClient`] connection.
    pub const DEFAULT_HISLIP_READ_TIMEOUT: Duration = Duration::from_secs(10);

    /// HiSLIP protocol version negotiated by this implementation.
    const PROTOCOL_VERSION_MAJOR: u8 = 1;
    const PROTOCOL_VERSION_MINOR: u16 = 0;

    /// Safety limit: reject payloads larger than this to prevent
    /// out-of-memory on malformed frames.
    const MAX_PAYLOAD_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB

    /// Number of initial message credits the server grants to each client
    /// in the `InitializeResponse` `control_code` field (IVI-6.1 §3.1).
    const INITIAL_CREDITS: u8 = 1;

    /// Fallback credit count used when a server's `InitializeResponse`
    /// sets `control_code` to 0 (backward-compatible synchronized mode).
    const DEFAULT_FALLBACK_CREDITS: u32 = 1;

    /// Timeout applied when waiting for an optional
    /// `AsyncMaximumMessageSize` message from the client after the async
    /// channel has been initialized.
    const ASYNC_MAX_MSG_SIZE_TIMEOUT: Duration = Duration::from_millis(200);

    /// Initial MessageID value used by the client (IVI-6.1 §6.3).
    const INITIAL_MESSAGE_ID: u32 = 0xFFFF_FF00;

    // -------------------------------------------------------------------
    // Message types (IVI-6.1 §2, Table 1)
    // -------------------------------------------------------------------

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    enum MessageType {
        Initialize = 0,
        InitializeResponse = 1,
        FatalError = 2,
        Error = 3,
        AsyncLock = 4,
        AsyncLockResponse = 5,
        Data = 6,
        DataEnd = 7,
        DeviceClearComplete = 8,
        DeviceClearAcknowledge = 9,
        AsyncRemoteLocalControl = 10,
        AsyncRemoteLocalResponse = 11,
        Trigger = 12,
        Interrupted = 13,
        AsyncInterrupted = 14,
        AsyncMaximumMessageSize = 15,
        AsyncMaximumMessageSizeResponse = 16,
        AsyncInitialize = 17,
        AsyncInitializeResponse = 18,
        AsyncDeviceClear = 19,
        AsyncServiceRequest = 20,
        AsyncStatusQuery = 21,
        AsyncStatusResponse = 22,
        AsyncDeviceClearAcknowledge = 23,
        AsyncLockInfo = 24,
        AsyncLockInfoResponse = 25,
    }

    impl MessageType {
        fn from_u8(val: u8) -> io::Result<Self> {
            match val {
                0 => Ok(Self::Initialize),
                1 => Ok(Self::InitializeResponse),
                2 => Ok(Self::FatalError),
                3 => Ok(Self::Error),
                4 => Ok(Self::AsyncLock),
                5 => Ok(Self::AsyncLockResponse),
                6 => Ok(Self::Data),
                7 => Ok(Self::DataEnd),
                8 => Ok(Self::DeviceClearComplete),
                9 => Ok(Self::DeviceClearAcknowledge),
                10 => Ok(Self::AsyncRemoteLocalControl),
                11 => Ok(Self::AsyncRemoteLocalResponse),
                12 => Ok(Self::Trigger),
                13 => Ok(Self::Interrupted),
                14 => Ok(Self::AsyncInterrupted),
                15 => Ok(Self::AsyncMaximumMessageSize),
                16 => Ok(Self::AsyncMaximumMessageSizeResponse),
                17 => Ok(Self::AsyncInitialize),
                18 => Ok(Self::AsyncInitializeResponse),
                19 => Ok(Self::AsyncDeviceClear),
                20 => Ok(Self::AsyncServiceRequest),
                21 => Ok(Self::AsyncStatusQuery),
                22 => Ok(Self::AsyncStatusResponse),
                23 => Ok(Self::AsyncDeviceClearAcknowledge),
                24 => Ok(Self::AsyncLockInfo),
                25 => Ok(Self::AsyncLockInfoResponse),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown HiSLIP message type: {}", val),
                )),
            }
        }
    }

    // -------------------------------------------------------------------
    // HiSLIP message framing
    // -------------------------------------------------------------------

    /// A single HiSLIP message (header + payload).
    #[derive(Debug, Clone)]
    struct Message {
        msg_type: MessageType,
        control_code: u8,
        message_parameter: u32,
        payload: Vec<u8>,
    }

    impl Message {
        fn new(
            msg_type: MessageType,
            control_code: u8,
            message_parameter: u32,
            payload: Vec<u8>,
        ) -> Self {
            Message {
                msg_type,
                control_code,
                message_parameter,
                payload,
            }
        }

        /// Serialize the message into a byte buffer ready for the wire.
        fn encode(&self) -> Vec<u8> {
            let payload_len = self.payload.len() as u64;
            let mut buf = Vec::with_capacity(HEADER_SIZE + self.payload.len());
            buf.extend_from_slice(&PROLOGUE);
            buf.push(self.msg_type as u8);
            buf.push(self.control_code);
            buf.extend_from_slice(&self.message_parameter.to_be_bytes());
            buf.extend_from_slice(&payload_len.to_be_bytes());
            buf.extend_from_slice(&self.payload);
            buf
        }

        /// Read exactly one HiSLIP message from `reader`.
        fn decode<R: Read>(reader: &mut R) -> io::Result<Self> {
            let mut header = [0u8; HEADER_SIZE];
            reader.read_exact(&mut header)?;

            if header[0] != PROLOGUE[0] || header[1] != PROLOGUE[1] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid HiSLIP prologue: expected 'HS', got [{:#04x}, {:#04x}]",
                        header[0], header[1]
                    ),
                ));
            }

            let msg_type = MessageType::from_u8(header[2])?;
            let control_code = header[3];
            let message_parameter =
                u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
            let payload_len = u64::from_be_bytes([
                header[8], header[9], header[10], header[11], header[12], header[13], header[14],
                header[15],
            ]);

            if payload_len > MAX_PAYLOAD_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("HiSLIP payload too large: {} bytes", payload_len),
                ));
            }

            let mut payload = vec![0u8; payload_len as usize];
            if payload_len > 0 {
                reader.read_exact(&mut payload)?;
            }

            Ok(Message {
                msg_type,
                control_code,
                message_parameter,
                payload,
            })
        }
    }

    // -------------------------------------------------------------------
    // HislipServer
    // -------------------------------------------------------------------

    /// A HiSLIP (IVI-6.1) server for SCPI instruments.
    ///
    /// Listens for incoming HiSLIP connections and processes SCPI messages
    /// using the binary HiSLIP framing protocol.  Each session consists of
    /// two TCP connections (synchronous + asynchronous channels) established
    /// during the HiSLIP initialisation handshake.
    ///
    /// Construct with [`HislipServer::bind`], then call
    /// [`HislipServer::serve`] (sequential) or
    /// [`HislipServer::serve_concurrent`] (one thread per session).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use scpify::{Device, Identification};
    /// use scpify::transport::HislipServer;
    ///
    /// let mut device = Device::new(Identification::default());
    /// let server = HislipServer::bind("127.0.0.1:4880").expect("bind failed");
    /// server.serve(&mut device).expect("server error");
    /// ```
    pub struct HislipServer {
        listener: TcpListener,
    }

    impl HislipServer {
        /// Bind to the given address.
        ///
        /// Pass any value accepted by [`std::net::TcpListener::bind`], e.g.
        /// `"0.0.0.0:4880"` or `"127.0.0.1:0"` (OS-assigned port, useful for
        /// tests).
        pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
            let listener = TcpListener::bind(addr)?;
            Ok(HislipServer { listener })
        }

        /// Return the local socket address the server is listening on.
        ///
        /// Useful when the server was bound to port `0` (OS-chosen port).
        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.listener.local_addr()
        }

        /// Serve sessions **sequentially**: one client session is handled to
        /// completion before the next is accepted.
        ///
        /// Each session requires two TCP connections (sync + async channels).
        /// This call blocks indefinitely (until an I/O error on the listener
        /// itself).
        pub fn serve(&self, device: &mut Device) -> io::Result<()> {
            let mut next_session_id: u16 = 0;
            let mut pending: HashMap<u16, TcpStream> = HashMap::new();

            for stream in self.listener.incoming() {
                match stream {
                    Ok(mut s) => {
                        let msg = match Message::decode(&mut s) {
                            Ok(m) => m,
                            Err(_) => continue, // malformed / closed connection
                        };

                        match msg.msg_type {
                            MessageType::Initialize => {
                                let session_id = next_session_id;
                                next_session_id = next_session_id.wrapping_add(1);

                                let response = init_response(session_id);
                                if s.write_all(&response.encode()).is_err() {
                                    continue;
                                }

                                pending.insert(session_id, s);
                            }
                            MessageType::AsyncInitialize => {
                                let session_id = (msg.message_parameter >> 16) as u16;

                                let response = async_init_response();
                                let _ = s.write_all(&response.encode());

                                handle_async_max_msg_size(&mut s);

                                if let Some(sync_stream) = pending.remove(&session_id) {
                                    // Ignore per-client errors so the server
                                    // keeps accepting new sessions.
                                    let _ = serve_hislip_client(sync_stream, device);
                                }
                            }
                            _ => {
                                let _ = send_fatal_error(&mut s, "unexpected message during init");
                            }
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        /// Serve sessions **concurrently** by spawning a thread per session.
        ///
        /// The [`Device`] is shared across threads behind an
        /// `Arc<Mutex<Device>>`.  This call blocks indefinitely.
        pub fn serve_concurrent(self, device: Device) -> io::Result<()> {
            let shared = Arc::new(Mutex::new(device));
            let mut next_session_id: u16 = 0;
            let pending: Arc<Mutex<HashMap<u16, TcpStream>>> =
                Arc::new(Mutex::new(HashMap::new()));

            for stream in self.listener.incoming() {
                match stream {
                    Ok(mut s) => {
                        let msg = match Message::decode(&mut s) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };

                        match msg.msg_type {
                            MessageType::Initialize => {
                                let session_id = next_session_id;
                                next_session_id = next_session_id.wrapping_add(1);

                                let response = init_response(session_id);
                                if s.write_all(&response.encode()).is_err() {
                                    continue;
                                }

                                pending.lock().unwrap().insert(session_id, s);
                            }
                            MessageType::AsyncInitialize => {
                                let session_id = (msg.message_parameter >> 16) as u16;

                                let response = async_init_response();
                                let _ = s.write_all(&response.encode());

                                handle_async_max_msg_size(&mut s);

                                if let Some(sync_stream) =
                                    pending.lock().unwrap().remove(&session_id)
                                {
                                    let dev = Arc::clone(&shared);
                                    std::thread::spawn(move || {
                                        let _ = serve_hislip_client_shared(sync_stream, dev);
                                    });
                                }
                            }
                            _ => {
                                let _ =
                                    send_fatal_error(&mut s, "unexpected message during init");
                            }
                        }
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
                .finish()
        }
    }

    // -------------------------------------------------------------------
    // HislipClient
    // -------------------------------------------------------------------

    /// A HiSLIP (IVI-6.1) client.
    ///
    /// Connect to an instrument that exposes a HiSLIP server and
    /// send/receive SCPI messages using the binary HiSLIP framing protocol.
    ///
    /// Construct with [`HislipClient::connect`], then use
    /// [`HislipClient::send`] for non-query commands and
    /// [`HislipClient::query`] (or [`HislipClient::query_f64`]) for queries.
    ///
    /// A read timeout of [`DEFAULT_HISLIP_READ_TIMEOUT`] (10 s) is set
    /// automatically on every new connection.  Call
    /// [`HislipClient::set_read_timeout`] to adjust or disable it.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use scpify::transport::HislipClient;
    ///
    /// let mut inst = HislipClient::connect("192.168.1.100:4880").unwrap();
    /// println!("{}", inst.query("*IDN?").unwrap());
    /// let v = inst.query_f64(":MEASure:VOLTage:DC?").unwrap();
    /// println!("DC voltage: {} V", v);
    /// ```
    pub struct HislipClient {
        sync_stream: TcpStream,
        /// The async channel is kept alive for the lifetime of the session.
        _async_stream: TcpStream,
        /// Next MessageID to use (incremented by 2 per request).
        message_id: u32,
        /// Available message-send credits (IVI-6.1 credit-based flow
        /// control).  Decremented when a `Data`/`DataEnd` is sent;
        /// replenished when a `DataEnd` response is received.
        credits: u32,
    }

    impl HislipClient {
        /// Connect to a HiSLIP instrument at `addr` using the default
        /// sub-address `"hislip0"`.
        ///
        /// The connection performs the full HiSLIP initialisation handshake
        /// (synchronous channel `Initialize` + asynchronous channel
        /// `AsyncInitialize`).
        pub fn connect(addr: impl ToSocketAddrs + Clone) -> io::Result<Self> {
            Self::connect_with_sub_address(addr, "hislip0")
        }

        /// Connect using a specific HiSLIP sub-address (e.g. `"hislip1"`).
        pub fn connect_with_sub_address(
            addr: impl ToSocketAddrs + Clone,
            sub_address: &str,
        ) -> io::Result<Self> {
            // -- 1. Open synchronous channel and perform Initialize handshake --
            let mut sync_stream = TcpStream::connect(addr.clone())?;
            sync_stream.set_read_timeout(Some(DEFAULT_HISLIP_READ_TIMEOUT))?;

            let version_param = (PROTOCOL_VERSION_MINOR as u32) << 16; // vendor ID = 0
            let init_msg = Message::new(
                MessageType::Initialize,
                PROTOCOL_VERSION_MAJOR,
                version_param,
                sub_address.as_bytes().to_vec(),
            );
            sync_stream.write_all(&init_msg.encode())?;

            let init_resp = Message::decode(&mut sync_stream)?;
            if init_resp.msg_type != MessageType::InitializeResponse {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "expected InitializeResponse, got {:?}",
                        init_resp.msg_type
                    ),
                ));
            }

            let session_id = (init_resp.message_parameter >> 16) as u16;

            // Parse initial credits from the control_code field.
            // In synchronized mode, if the server sets control_code to 0 we
            // fall back to DEFAULT_FALLBACK_CREDITS (request-response).
            let credits = if init_resp.control_code > 0 {
                init_resp.control_code as u32
            } else {
                DEFAULT_FALLBACK_CREDITS
            };

            // -- 2. Open asynchronous channel and perform AsyncInitialize --
            let mut async_stream = TcpStream::connect(addr)?;
            async_stream.set_read_timeout(Some(DEFAULT_HISLIP_READ_TIMEOUT))?;

            let async_init = Message::new(
                MessageType::AsyncInitialize,
                0,
                (session_id as u32) << 16,
                Vec::new(),
            );
            async_stream.write_all(&async_init.encode())?;

            let async_resp = Message::decode(&mut async_stream)?;
            if async_resp.msg_type != MessageType::AsyncInitializeResponse {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "expected AsyncInitializeResponse, got {:?}",
                        async_resp.msg_type
                    ),
                ));
            }

            // -- 3. Negotiate maximum message size (AsyncMaximumMessageSize) --
            let max_msg = Message::new(
                MessageType::AsyncMaximumMessageSize,
                0,
                0,
                MAX_PAYLOAD_SIZE.to_be_bytes().to_vec(),
            );
            async_stream.write_all(&max_msg.encode())?;

            let max_resp = Message::decode(&mut async_stream)?;
            if max_resp.msg_type != MessageType::AsyncMaximumMessageSizeResponse {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "expected AsyncMaximumMessageSizeResponse, got {:?}",
                        max_resp.msg_type
                    ),
                ));
            }

            Ok(HislipClient {
                sync_stream,
                _async_stream: async_stream,
                message_id: INITIAL_MESSAGE_ID,
                credits,
            })
        }

        /// Set the read timeout for query responses.
        ///
        /// Pass `None` to disable the timeout entirely.
        pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            self.sync_stream.set_read_timeout(timeout)
        }

        /// Return the next MessageID and advance the counter.
        fn next_message_id(&mut self) -> u32 {
            let id = self.message_id;
            self.message_id = self.message_id.wrapping_add(2);
            id
        }

        /// Return the number of message-send credits currently available.
        pub fn credits(&self) -> u32 {
            self.credits
        }

        /// Consume one credit and send a `DataEnd` message on the sync
        /// channel.  Returns an error if no credits are available.
        fn send_data_end(&mut self, payload: Vec<u8>) -> io::Result<()> {
            if self.credits == 0 {
                return Err(io::Error::other(
                    "no HiSLIP message credits available; \
                     cannot send until a credit is granted by the server",
                ));
            }
            self.credits -= 1;

            let msg_id = self.next_message_id();
            let msg = Message::new(MessageType::DataEnd, 0, msg_id, payload);
            self.sync_stream.write_all(&msg.encode())
        }

        /// Send a SCPI command that produces **no response** (e.g. `*RST`).
        ///
        /// The command is wrapped in a HiSLIP `DataEnd` message.  The server
        /// responds with a `DataEnd` acknowledgement which is read and
        /// discarded automatically.
        pub fn send(&mut self, command: &str) -> io::Result<()> {
            self.send_data_end(command.as_bytes().to_vec())?;

            // HiSLIP is request-response: consume the server's reply so
            // it does not interfere with the next query().
            let _resp = self.read_response()?;
            Ok(())
        }

        /// Send a query command and return the instrument's response as a
        /// trimmed `String`.
        ///
        /// The command should end with `?` (e.g. `"*IDN?"`).
        pub fn query(&mut self, command: &str) -> io::Result<String> {
            self.send_data_end(command.as_bytes().to_vec())?;

            let data = self.read_response()?;
            let text = String::from_utf8(data).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("HiSLIP response is not valid UTF-8: {}", e),
                )
            })?;

            Ok(text.trim().to_string())
        }

        /// Send a query command and parse the response as an `f64`.
        pub fn query_f64(&mut self, command: &str) -> io::Result<f64> {
            let raw = self.query(command)?;
            raw.parse::<f64>().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected a numeric response, got {:?}: {}", raw, e),
                )
            })
        }

        /// Send a query and return the raw response bytes (useful for
        /// binary / block data).
        pub fn query_raw(&mut self, command: &str) -> io::Result<Vec<u8>> {
            self.send_data_end(command.as_bytes().to_vec())?;
            self.read_response()
        }

        /// Read one complete response (possibly spanning multiple `Data` /
        /// `DataEnd` messages) from the synchronous channel.
        fn read_response(&mut self) -> io::Result<Vec<u8>> {
            let mut buf = Vec::new();
            loop {
                let msg = Message::decode(&mut self.sync_stream)?;
                match msg.msg_type {
                    MessageType::Data => {
                        buf.extend_from_slice(&msg.payload);
                    }
                    MessageType::DataEnd => {
                        buf.extend_from_slice(&msg.payload);
                        // In synchronized mode each response restores one
                        // send credit, allowing the next request.
                        self.credits += 1;
                        return Ok(buf);
                    }
                    MessageType::FatalError | MessageType::Error => {
                        let text = String::from_utf8_lossy(&msg.payload);
                        return Err(io::Error::other(
                            format!("HiSLIP error ({:?}): {}", msg.msg_type, text),
                        ));
                    }
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("unexpected HiSLIP message type: {:?}", other),
                        ));
                    }
                }
            }
        }
    }

    impl std::fmt::Debug for HislipClient {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("HislipClient")
                .field("peer_addr", &self.sync_stream.peer_addr().ok())
                .field("message_id", &self.message_id)
                .field("credits", &self.credits)
                .finish()
        }
    }

    // -------------------------------------------------------------------
    // Server-side helpers
    // -------------------------------------------------------------------

    /// Build an `InitializeResponse` message for the given session ID.
    ///
    /// The `control_code` carries the initial message-credit grant so that
    /// clients know they are allowed to send `Data`/`DataEnd` messages
    /// (IVI-6.1 §3.1).
    fn init_response(session_id: u16) -> Message {
        let response_param = ((session_id as u32) << 16)
            | ((PROTOCOL_VERSION_MAJOR as u32) << 8)
            | (PROTOCOL_VERSION_MINOR as u32);
        Message::new(
            MessageType::InitializeResponse,
            INITIAL_CREDITS,
            response_param,
            Vec::new(),
        )
    }

    /// Build an `AsyncInitializeResponse` message.
    fn async_init_response() -> Message {
        Message::new(MessageType::AsyncInitializeResponse, 0, 0, Vec::new())
    }

    /// Handle an optional `AsyncMaximumMessageSize` exchange on the async
    /// channel.  Many real HiSLIP clients send this message immediately
    /// after `AsyncInitialize`; if it arrives, the server responds with
    /// `AsyncMaximumMessageSizeResponse`.  A short read-timeout is used so
    /// clients that omit this step are not penalised.
    fn handle_async_max_msg_size(stream: &mut TcpStream) {
        let original_timeout = stream.read_timeout().ok().flatten();
        let _ = stream.set_read_timeout(Some(ASYNC_MAX_MSG_SIZE_TIMEOUT));

        if let Ok(msg) = Message::decode(stream) {
            if msg.msg_type == MessageType::AsyncMaximumMessageSize {
                let resp = Message::new(
                    MessageType::AsyncMaximumMessageSizeResponse,
                    0,
                    0,
                    MAX_PAYLOAD_SIZE.to_be_bytes().to_vec(),
                );
                let _ = stream.write_all(&resp.encode());
            }
        }

        let _ = stream.set_read_timeout(original_timeout);
    }

    /// Send a `FatalError` message on a stream (best-effort).
    fn send_fatal_error(stream: &mut TcpStream, text: &str) -> io::Result<()> {
        let msg = Message::new(
            MessageType::FatalError,
            3, // InvalidInitSequence
            0,
            text.as_bytes().to_vec(),
        );
        stream.write_all(&msg.encode())
    }

    /// Handle a single HiSLIP client session on `sync_stream` (sequential).
    fn serve_hislip_client(mut sync_stream: TcpStream, device: &mut Device) -> io::Result<()> {
        process_hislip_messages(&mut sync_stream, |msg| device.process(msg))
    }

    /// Handle a single HiSLIP client session on `sync_stream` (concurrent).
    fn serve_hislip_client_shared(
        mut sync_stream: TcpStream,
        device: Arc<Mutex<Device>>,
    ) -> io::Result<()> {
        process_hislip_messages(&mut sync_stream, |msg| {
            device
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .process(msg)
        })
    }

    /// Core message loop shared by both client handlers.
    ///
    /// Reads `Data`/`DataEnd` messages from the synchronous channel,
    /// accumulates multi-chunk payloads, dispatches the complete SCPI
    /// message, and writes back a `DataEnd` response.
    fn process_hislip_messages<F>(stream: &mut TcpStream, mut dispatch: F) -> io::Result<()>
    where
        F: FnMut(&str) -> Vec<Response>,
    {
        let mut read_stream = stream.try_clone()?;
        let mut accumulated = Vec::new();

        loop {
            let msg = match Message::decode(&mut read_stream) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            };

            match msg.msg_type {
                MessageType::Data => {
                    accumulated.extend_from_slice(&msg.payload);
                }
                MessageType::DataEnd => {
                    accumulated.extend_from_slice(&msg.payload);

                    let command = String::from_utf8_lossy(&accumulated);
                    let command = command.trim();

                    let response_text = if command.is_empty() {
                        String::new()
                    } else {
                        let responses = dispatch(command);
                        responses
                            .iter()
                            .filter(|r| **r != Response::Empty)
                            .map(|r| r.to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    };

                    // Server response uses (client_msg_id | 1).
                    let resp_msg = Message::new(
                        MessageType::DataEnd,
                        0,
                        msg.message_parameter | 1,
                        response_text.into_bytes(),
                    );
                    stream.write_all(&resp_msg.encode())?;

                    accumulated.clear();
                }
                _ => {
                    // Ignore unhandled message types (locks, triggers, …).
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // Tests
    // -------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::{Identification, Response};

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

        // -- helpers ----------------------------------------------------

        /// Start a sequential HiSLIP server; return the bound port.
        fn start_sequential(device: Device) -> u16 {
            let server = HislipServer::bind("127.0.0.1:0").expect("bind");
            let port = server.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let mut dev = device;
                let _ = server.serve(&mut dev);
            });
            wait_for_port(port);
            port
        }

        /// Start a concurrent HiSLIP server; return the bound port.
        fn start_concurrent(device: Device) -> u16 {
            let server = HislipServer::bind("127.0.0.1:0").expect("bind");
            let port = server.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let _ = server.serve_concurrent(device);
            });
            wait_for_port(port);
            port
        }

        /// Spin until a TCP connection succeeds.
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

        // -- Message encode / decode ------------------------------------

        #[test]
        fn message_round_trip() {
            let original = Message::new(
                MessageType::DataEnd,
                0,
                42,
                b"*IDN?".to_vec(),
            );
            let encoded = original.encode();
            let decoded = Message::decode(&mut &encoded[..]).unwrap();

            assert_eq!(decoded.msg_type, MessageType::DataEnd);
            assert_eq!(decoded.control_code, 0);
            assert_eq!(decoded.message_parameter, 42);
            assert_eq!(decoded.payload, b"*IDN?");
        }

        #[test]
        fn message_decode_bad_prologue() {
            let bad = b"XX\x06\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
            let result = Message::decode(&mut &bad[..]);
            assert!(result.is_err());
        }

        #[test]
        fn message_encode_empty_payload() {
            let msg = Message::new(MessageType::Initialize, 1, 0, Vec::new());
            let encoded = msg.encode();
            assert_eq!(encoded.len(), HEADER_SIZE);
            // Payload length field should be 0.
            assert_eq!(&encoded[8..16], &[0u8; 8]);
        }

        // -- Client-server integration ----------------------------------

        #[test]
        fn sequential_idn_query() {
            let port = start_sequential(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let resp = client.query("*IDN?").unwrap();
            assert_eq!(resp, "\"TestCo,T1,001,0.1\"");
        }

        #[test]
        fn sequential_custom_query() {
            let port = start_sequential(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let resp = client.query(":MEASure:VOLTage?").unwrap();
            let value: f64 = resp
                .parse()
                .unwrap_or_else(|_| panic!("expected numeric, got: {}", resp));
            assert!(
                (value - 3.3).abs() < 1e-9,
                "expected ~3.3, got {}",
                value
            );
        }

        #[test]
        fn sequential_send_then_query() {
            let port = start_sequential(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            // send() should not hang — the server sends back an empty DataEnd.
            client.send("*RST").unwrap();
            let resp = client.query("*IDN?").unwrap();
            assert!(resp.contains("TestCo"), "{}", resp);
        }

        #[test]
        fn sequential_compound_message() {
            let port = start_sequential(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            // Compound: RST (empty) then IDN? → one line in the response.
            let resp = client.query("*RST;*IDN?").unwrap();
            assert!(resp.contains("TestCo"), "{}", resp);
        }

        #[test]
        fn client_query_f64() {
            let port = start_sequential(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let v = client.query_f64(":MEASure:VOLTage?").unwrap();
            assert!((v - 3.3).abs() < 1e-9, "expected ~3.3, got {}", v);
        }

        #[test]
        fn client_query_raw_returns_bytes() {
            let port = start_sequential(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let data = client.query_raw("*IDN?").unwrap();
            let text = String::from_utf8(data).unwrap();
            assert!(text.contains("TestCo"), "{}", text);
        }

        #[test]
        fn concurrent_idn_query() {
            let port = start_concurrent(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let resp = client.query("*IDN?").unwrap();
            assert_eq!(resp, "\"TestCo,T1,001,0.1\"");
        }

        #[test]
        fn concurrent_multiple_clients() {
            let port = start_concurrent(test_device());

            let handles: Vec<_> = (0..4)
                .map(|_| {
                    std::thread::spawn(move || {
                        let mut client =
                            HislipClient::connect(format!("127.0.0.1:{}", port))
                                .unwrap();
                        client.query("*IDN?").unwrap()
                    })
                })
                .collect();

            for h in handles {
                let resp = h.join().expect("thread panicked");
                assert!(resp.contains("TestCo"), "{}", resp);
            }
        }

        #[test]
        fn server_debug_impl() {
            let server = HislipServer::bind("127.0.0.1:0").unwrap();
            let s = format!("{:?}", server);
            assert!(s.contains("HislipServer"));
        }

        #[test]
        fn client_debug_impl() {
            let port = start_sequential(test_device());
            let client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let s = format!("{:?}", client);
            assert!(s.contains("HislipClient"));
        }

        #[test]
        fn client_set_read_timeout() {
            let port = start_sequential(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let resp = client.query("*IDN?").unwrap();
            assert!(resp.contains("TestCo"), "{}", resp);
        }

        #[test]
        fn client_query_times_out_when_no_response() {
            // A "silent" TCP server: accept connections but never
            // complete the HiSLIP handshake → connect() should time out.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                // Accept connections but never write anything back.
                for stream in listener.incoming() {
                    let _conn = stream.unwrap();
                    std::thread::sleep(Duration::from_secs(5));
                }
            });

            // Use a very short timeout.
            let mut sync_stream =
                TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
            sync_stream
                .set_read_timeout(Some(Duration::from_millis(150)))
                .unwrap();

            // Send an Initialize message.
            let init_msg = Message::new(
                MessageType::Initialize,
                1,
                0,
                b"hislip0".to_vec(),
            );
            sync_stream.write_all(&init_msg.encode()).unwrap();

            // Reading the response should time out.
            let err = Message::decode(&mut sync_stream).unwrap_err();
            assert!(
                err.kind() == io::ErrorKind::TimedOut
                    || err.kind() == io::ErrorKind::WouldBlock,
                "expected a timeout error, got: {:?}",
                err
            );
        }

        #[test]
        fn connect_with_sub_address() {
            let port = start_sequential(test_device());
            let mut client = HislipClient::connect_with_sub_address(
                format!("127.0.0.1:{}", port),
                "hislip1",
            )
            .unwrap();
            let resp = client.query("*IDN?").unwrap();
            assert!(resp.contains("TestCo"), "{}", resp);
        }

        // -- Credit tracking -------------------------------------------

        #[test]
        fn init_response_grants_credits() {
            let msg = init_response(0);
            assert_eq!(
                msg.control_code, INITIAL_CREDITS,
                "InitializeResponse must carry initial credits in control_code"
            );
        }

        #[test]
        fn client_credits_after_connect() {
            let port = start_sequential(test_device());
            let client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            assert!(
                client.credits() >= 1,
                "client should have at least 1 credit after connecting"
            );
        }

        #[test]
        fn credits_restored_after_query() {
            let port = start_sequential(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let before = client.credits();
            client.query("*IDN?").unwrap();
            assert_eq!(
                client.credits(),
                before,
                "credits should be restored after a completed query"
            );
        }

        #[test]
        fn credits_restored_after_send() {
            let port = start_sequential(test_device());
            let mut client =
                HislipClient::connect(format!("127.0.0.1:{}", port)).unwrap();
            let before = client.credits();
            client.send("*RST").unwrap();
            assert_eq!(
                client.credits(),
                before,
                "credits should be restored after a completed send"
            );
        }
    }
}

#[cfg(feature = "hislip")]
pub use hislip::{
    HislipClient, HislipServer, DEFAULT_HISLIP_PORT, DEFAULT_HISLIP_READ_TIMEOUT,
};
