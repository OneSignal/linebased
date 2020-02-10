#![recursion_limit = "256"]
//! Drop-in TCP command server
//!
//! Provide a callback that is passed commands from clients and handle them synchronously.
//! `tokio` is used internally so multiple clients may be active.
//!
//! # Examples
//!
//! ```no_run
//! use linebased::Server;
//!
//! #[tokio::main]
//! async fn main() {
//!     // Create a server with the default config and a
//!     // handler that only knows the "version" command
//!     let mut server = Server::new(Default::default(), |query| {
//!         match query {
//!             "version" => String::from("0.1.0"),
//!             _ => String::from("unknown command"),
//!         }
//!     }).unwrap();
//!
//!     server.run().await.unwrap();
//! }
//! ```
//!
//! Running a server in the background is also possible, just spawn the future
//! returned by `Server::run`. Request a handle ! from the server so that you
//! may shut it down gracefully.
//!
//! ```no_run
//! use linebased::Server;
//! use std::thread;
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut server = Server::new(Default::default(), |query| {
//!         match query {
//!             "version" => String::from("0.1.0"),
//!             _ => String::from("unknown command"),
//!         }
//!     }).unwrap();
//!
//!     let handle = server.handle();
//!     let fut = tokio::spawn(async move { server.run().await });
//!
//!     // Time passes
//!
//!     handle.shutdown();
//!     fut.await.expect("failed to spawn future").expect("Error from linebased::Server::run");
//! }
//! ```
#![warn(missing_docs)]

use std::io;
use std::net::{Shutdown, SocketAddr};
use std::str;
use std::sync::Arc;

use futures::prelude::*;
use log::{debug, error, info, trace, warn};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{
    broadcast::{self, Receiver, Sender},
    Semaphore,
};

mod error;

pub use error::Error;
pub use error::Result;

type HandleFn = Arc<dyn Fn(&str) -> String + 'static + Send + Sync>;

/// Server configuration
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to listen on
    host: String,

    /// Port to listen on
    port: u16,

    /// Maximum number of client connections
    max_clients: usize,

    /// initial per-client buffer size, will grow beyond this limit if required.
    client_buf_size: usize,

    /// If the handler function blocks, it should be spawned on its own worker
    /// thread so that the tokio threadpool isn't blocked by it.
    handle_fn_blocks: bool,
}

impl Config {
    /// Set host address to listen on
    pub fn host<S>(mut self, val: S) -> Self
    where
        S: Into<String>,
    {
        self.host = val.into();
        self
    }

    /// Set port to listen on
    pub fn port(mut self, val: u16) -> Self {
        self.port = val;
        self
    }

    /// set maximum number of clients
    pub fn max_clients(mut self, val: usize) -> Self {
        self.max_clients = val;
        self
    }

    /// Set the initial per-client buffer size, will grow beyond this limit if required.
    pub fn client_buf_size(mut self, val: usize) -> Self {
        self.client_buf_size = val;
        self
    }

    /// Indicate that the handle fn might block and should be spawned on a tokio
    /// worker thread. This should be used if the fn does heavy computation, or
    /// has any blocking i/o in it.
    pub fn handle_fn_blocks(mut self) -> Self {
        self.handle_fn_blocks = true;
        self
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            host: "127.0.0.1".into(),
            port: 7343,
            max_clients: 32,
            client_buf_size: 1024,
            handle_fn_blocks: false,
        }
    }
}

struct Client {
    buf: String,
    reader: BufReader<ReadHalf<TcpStream>>,
    writer: WriteHalf<TcpStream>,
    handle_fn: HandleFn,
    handle_fn_blocks: bool,
}

impl Client {
    fn new(config: &Config, stream: TcpStream, handle_fn: &HandleFn) -> Client {
        let (reader, writer) = tokio::io::split(stream);

        let buf = String::with_capacity(config.client_buf_size);
        let reader = BufReader::with_capacity(config.client_buf_size, reader);
        let handle_fn = Arc::clone(handle_fn);

        Client {
            buf,
            reader,
            writer,
            handle_fn,
            handle_fn_blocks: config.handle_fn_blocks,
        }
    }

    fn spawn(self, clients: &Arc<Semaphore>, shutdown_send: &Sender<ControlMsg>) {
        let clients = Arc::clone(&clients);
        let shutdown_recv = shutdown_send.subscribe();

        tokio::spawn(self.try_accept(clients, shutdown_recv));
    }

    async fn try_accept(self, clients: Arc<Semaphore>, shutdown_recv: Receiver<ControlMsg>) {
        let permit = match clients.try_acquire() {
            Ok(client) => client,
            Err(_) => {
                warn!("rejecting client; max connections reached");
                return;
            }
        };

        trace!("accept client connection");
        self.accept(shutdown_recv).await;

        drop(permit);
    }

    async fn accept(mut self, mut shutdown_recv: Receiver<ControlMsg>) {
        loop {
            futures::select! {
                result = self.handle_line().fuse() => {
                    if let Err(e) = result {
                        debug!("Error handling value - shutting down connection: {}", e);
                        break;
                    }

                    self.buf.clear();
                }
                control_msg = shutdown_recv.recv().fuse() => {
                    match control_msg {
                        Ok(ControlMsg::Shutdown) => {
                            info!("Shutting down server");
                            break;
                        }
                        Err(e) => {
                            error!("Error receiving control message {:?}", e);
                            break;
                        }
                    }
                }
            }
        }

        self.shutdown();
    }

    fn shutdown(self) {
        if let Err(e) = self
            .reader
            .into_inner()
            .unsplit(self.writer)
            .shutdown(Shutdown::Both)
        {
            debug!("Error closing socket connection {:?}", e);
        }
    }

    async fn handle_line(&mut self) -> Result<()> {
        let bytes_read = self.reader.read_line(&mut self.buf).await?;
        if bytes_read == 0 {
            return Err(Error::NoBytesRead);
        }

        let slice = if self.buf.is_empty() {
            &self.buf[..]
        } else {
            // Remove the newline at the end of the string
            &self.buf[0..self.buf.len() - 1]
        };

        trace!("Read line: \"{}\"", slice);

        let handle_fn = &self.handle_fn;
        let mut response = if self.handle_fn_blocks {
            let handle_fn = Arc::clone(&self.handle_fn);
            let string = slice.to_owned();

            tokio::task::spawn_blocking(move || handle_fn(&string)).await?
        } else {
            handle_fn(&slice)
        };

        response.push('\n');

        self.writer.write_all(response.as_bytes()).await?;
        trace!("Wrote response: \"{}\"", response.trim());

        Ok(())
    }
}

/// Handle for the server
pub struct Handle {
    sender: Sender<ControlMsg>,
}

impl Handle {
    /// Request the server to shutdown gracefully
    pub fn shutdown(self) {
        // send only returns an error if there are no receivers active, meaning
        // the server was already shut down, so it is safe to ignore this
        // result.
        let _ = self.sender.send(ControlMsg::Shutdown);
    }
}

/// The linebased TCP server
pub struct Server {
    handler: HandleFn,
    config: Config,
    address: SocketAddr,
    shutdown_recv: Receiver<ControlMsg>,
    shutdown_send: Sender<ControlMsg>,
    clients: Arc<Semaphore>,
}

impl Server {
    /// Create a new server
    // # Examples
    ///
    /// ```no_run
    /// use linebased::Server;
    ///
    /// // Create a server with the default config and a
    /// // handler that only knows the "version" command
    /// let mut server = Server::new(Default::default(), |query| {
    ///     match query {
    ///         "version" => {
    ///             String::from("0.1.0")
    ///         },
    ///         _ => {
    ///             String::from("unknown command")
    ///         }
    ///     }
    /// }).unwrap();
    /// ```
    pub fn new<F>(config: Config, func: F) -> Result<Server>
    where
        F: Fn(&str) -> String + 'static + Send + Sync,
    {
        let address = format!("{host}:{port}", host = config.host, port = config.port).parse()?;
        let (shutdown_send, shutdown_recv) = broadcast::channel(1);
        let clients = Arc::new(Semaphore::new(config.max_clients));

        Ok(Server {
            handler: Arc::new(func),
            config,
            address,
            shutdown_send,
            shutdown_recv,
            clients,
        })
    }

    /// Get a handle for the server so graceful shutdown can be requested
    pub fn handle(&self) -> Handle {
        Handle {
            sender: self.shutdown_send.clone(),
        }
    }

    /// Run the event loop
    pub async fn run(&mut self) -> io::Result<()> {
        info!("Listening at {}", self.address);
        let mut listener = TcpListener::bind(self.address).await?;

        loop {
            futures::select! {
                accept = listener.accept().fuse() => {
                    let (socket, _) = match accept {
                        Ok(socket) => socket,
                        Err(e) => {
                            error!("Error accepting connection: {}", e);
                            continue;
                        }
                    };

                    self.accept(socket);
                }
                control_msg = self.shutdown_recv.recv().fuse() => {
                    match control_msg {
                        Ok(ControlMsg::Shutdown) => {
                            info!("Shutting down server");
                            break;
                        }
                        Err(e) => {
                            error!("Error receiving control message {:?}", e);
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn accept(&self, socket: TcpStream) {
        let client = Client::new(&self.config, socket, &self.handler);

        client.spawn(&self.clients, &self.shutdown_send);
    }
}

#[doc(hidden)]
#[derive(Debug, Clone)]
pub enum ControlMsg {
    /// Stop the server and end all connections immediately
    Shutdown,
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
    use tokio::net::TcpStream;

    use super::{Config, Handle, Server};

    trait AsByteSlice {
        fn as_byte_slice(&self) -> &[u8];
    }

    impl AsByteSlice for String {
        fn as_byte_slice(&self) -> &[u8] {
            self.as_bytes()
        }
    }

    impl<'a> AsByteSlice for &'a str {
        fn as_byte_slice(&self) -> &[u8] {
            self.as_bytes()
        }
    }

    /// Client for testing
    struct Client {
        stream_read: BufReader<ReadHalf<TcpStream>>,
        stream_write: WriteHalf<TcpStream>,
    }

    impl Client {
        /// Create a client
        ///
        /// Any errors will panic since this is for testing only. The server is
        /// assumed to be on the default port. Performance is not a
        /// consideration here; only ergonomics, correctness, and failing early.
        pub async fn new(config: &Config) -> Self {
            let stream = Client::connect(config).await;

            let (stream_read, stream_write) = io::split(stream);
            let stream_read = BufReader::new(stream_read);

            Self {
                stream_read,
                stream_write,
            }
        }

        /// Connect to the server
        ///
        /// Retries as long as error is connection refused. I guess this can
        /// mean tests hang if something is wrong. Oh well.
        async fn connect(config: &Config) -> TcpStream {
            loop {
                match TcpStream::connect(
                    format!("{}:{}", config.host, config.port)
                        .parse::<SocketAddr>()
                        .unwrap(),
                )
                .await
                {
                    Ok(stream) => return stream,
                    Err(err) => match err.kind() {
                        ::std::io::ErrorKind::ConnectionRefused => continue,
                        _ => panic!("failed to connect; {}", err),
                    },
                }
            }
        }

        /// Sends all bytes to the remote
        pub async fn send<B>(&mut self, bytes: B)
        where
            B: AsByteSlice,
        {
            self.stream_write
                .write_all(bytes.as_byte_slice())
                .await
                .expect("successfully send bytes");
            self.stream_write
                .write_all(b"\n")
                .await
                .expect("successfully send bytes");
        }

        /// Receive the next line.
        ///
        /// Extra data is buffered internally.
        pub async fn recv(&mut self) -> String {
            let mut buf = String::new();
            self.stream_read
                .read_line(&mut buf)
                .await
                .expect("read_line");

            buf.trim_end().into()
        }

        pub async fn expect(&mut self, s: &str) {
            let got = self.recv().await;
            assert_eq!(got.as_str(), s);
        }
    }

    fn run_server(config: &Config) -> TestHandle {
        let _ = env_logger::try_init();
        let config = config.to_owned();

        let mut server = Server::new(config, |query| match query {
            "version" => String::from("0.1.0"),
            _ => String::from("unknown command"),
        })
        .unwrap();

        let handle = server.handle();

        tokio::spawn(async move {
            server.run().await.unwrap();
        });

        TestHandle {
            handle: Some(handle),
        }
    }

    /// Handle wrapping test server
    ///
    /// Requests graceful shutdown and joins with thread on drop
    pub struct TestHandle {
        handle: Option<Handle>,
    }

    impl Drop for TestHandle {
        fn drop(&mut self) {
            let _ = self.handle.take().unwrap().shutdown();
        }
    }

    #[tokio::test]
    async fn it_works() {
        let config = Config::default();
        let _server = run_server(&config);

        {
            let mut client = Client::new(&config).await;
            client.send("version").await;
            client.expect("0.1.0").await;
            client.send("nope").await;
            client.expect("unknown command").await;
        }
    }

    #[tokio::test]
    async fn send_empty_line() {
        let config = Config::default().port(5501);
        let _server = run_server(&config);

        {
            let mut client = Client::new(&config).await;
            client.send("").await;
            client.expect("unknown command").await;
            // commands should continue to work
            client.send("version").await;
            client.expect("0.1.0").await;
        }
    }

    #[tokio::test]
    async fn multiple_commands_received_at_once() {
        let config = Config::default().port(5502);
        let _server = run_server(&config);

        {
            let mut client = Client::new(&config).await;
            client.send("version\nversion").await;

            // This is a bug. Second response may or may not have a prompt.
            let got = client.recv().await;
            assert!(got.contains("0.1.0"));
        }
    }

    #[tokio::test]
    async fn exceed_max_clients() {
        let config = Config::default().max_clients(1).port(5503);
        let _server = run_server(&config);

        {
            let mut client = Client::new(&config).await;
            {
                // should get disconnected immediately
                let _client = Client::new(&config).await;
            }
            client.send("version").await;
            client.expect("0.1.0").await;
        }
    }
}
