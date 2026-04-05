//! Transport adapters for serving SCPI over network links.
//!
//! Enable the **`tcp`** Cargo feature to compile [`TcpServer`].
//!
//! # TCP (SCPI-RAW)
//!
//! [`TcpServer`] implements the *SCPI-RAW* protocol: plain TCP with one SCPI
//! message per line (terminated by `\n` or `\r\n`).  Port **5025** is the
//! IANA-registered port for this protocol.
//!
//! ## Sequential server
//!
//! Handles one client at a time — simpler, no synchronisation overhead:
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
//! ## Concurrent server
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
    use std::io::{self, BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
    use std::sync::{Arc, Mutex};

    use crate::{Device, Response};

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
    // Client handlers
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
    fn serve_client_shared(
        stream: TcpStream,
        device: Arc<Mutex<Device>>,
    ) -> io::Result<()> {
        let mut writer = stream.try_clone()?;
        let reader = BufReader::new(stream);
        process_lines(reader, &mut writer, |msg| {
            device.lock().unwrap_or_else(|e| e.into_inner()).process(msg)
        })
    }

    /// Core line-processing loop shared by both client handlers.
    fn process_lines<R, W, F>(reader: BufReader<R>, writer: &mut W, mut dispatch: F) -> io::Result<()>
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
                assert!(std::time::Instant::now() < deadline, "server did not start in time");
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
            let value: f64 = resp.parse().expect("expected a numeric response, got: {resp}");
            assert!((value - 3.3).abs() < 1e-9, "expected ~3.3, got {}", value);
        }

        #[test]
        fn sequential_non_query_produces_no_output() {
            let port = start_sequential(test_device());
            let stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_millis(150))).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            writeln!(writer, "*RST").unwrap();
            let mut buf = String::new();
            // The server sends nothing for non-query commands — read_line should time out.
            let result = reader.read_line(&mut buf);
            assert!(result.is_err() || buf.is_empty(), "unexpected output: {:?}", buf);
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
    }
}

#[cfg(feature = "tcp")]
pub use tcp::TcpServer;
