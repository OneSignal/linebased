//! Drop-in TCP command server
//!
//! Provide a callback that is passed commands from clients and handle them synchronously.
//! `mio` is used internally so multiple clients may be active.
//!
//! # Examples
//!
//! ```no_run
//! use linebased::Server;
//!
//! // Create a server with the default config and a
//! // handler that only knows the "version" command
//! let mut server = Server::new(Default::default(), |query| {
//!     match query {
//!         "version" => String::from("0.1.0"),
//!         _ => String::from("unknown command"),
//!     }
//! }).unwrap();
//!
//! server.run().unwrap();
//! ```
//!
//! Running a server from a separate thread is also possible. Request a handle
//! from the server so that you may shut it down gracefully.
//!
//! ```no_run
//! use linebased::Server;
//! use std::thread;
//!
//! let mut server = Server::new(Default::default(), |query| {
//!     match query {
//!         "version" => String::from("0.1.0"),
//!         _ => String::from("unknown command"),
//!     }
//! }).unwrap();
//!
//! let handle = server.handle();
//! let thread = thread::spawn(move || server.run().unwrap());
//!
//! // Time passes
//!
//! handle.shutdown().unwrap();
//! thread.join().unwrap();
//! ```
#![warn(missing_docs)]
extern crate mio;
#[macro_use]
extern crate log;

use std::str;
use std::io;

use mio::{EventLoop, Token, EventSet, PollOpt};
use mio::tcp::TcpListener;
use mio::tcp::TcpStream;
use mio::util::Slab;
use mio::TryRead;
use mio::TryWrite;

mod error;

pub use error::Result;
pub use error::Error;

const NEWLINE: u8 = 0x0a;
const SERVER_TOKEN: Token = Token(0);

/// Possible states a client is in
enum ClientState {
    /// Waiting for command from client
    Await,

    /// Writing response to client
    Responding(io::Cursor<Vec<u8>>),
}

impl Default for ClientState {
    fn default() -> ClientState {
        ClientState::Await
    }
}

struct Client {
    /// Client's token for event loop
    token: Token,

    /// The client's connection
    socket: TcpStream,

    /// Current client state
    state: ClientState,

    /// Bytes read from the stream are buffered here until a complete message is
    /// received.
    buf: Vec<u8>,

    /// Buffer position
    pos: usize,
}

enum Status {
    Ok,
    Disconnected,
}

impl Client {
    pub fn new(stream: TcpStream, token: Token, config: &Config) -> Client {
        Client {
            token: token,
            socket: stream,
            state: ClientState::Await,
            buf: vec![0u8; config.client_buf_size],
            pos: 0,
        }
    }

    pub fn interest(&self) -> EventSet {
        match self.state {
            ClientState::Await => EventSet::readable(),
            ClientState::Responding(_) => EventSet::writable(),
        }
    }

    #[inline]
    pub fn reregister(&mut self, event_loop: &mut EventLoop<ServerInner>) -> io::Result<()> {
        let opt = PollOpt::edge() | PollOpt::oneshot();
        event_loop.reregister(&self.socket, self.token, self.interest(), opt)
    }

    pub fn write(&mut self, _event_loop: &mut EventLoop<ServerInner>) -> Status {
        let mut done = false;
        match self.state {
            ClientState::Responding(ref mut buf) => {
                match self.socket.try_write_buf(buf) {
                    Ok(_) => {
                        // Done writing if the cursor position is at end of buf
                        if buf.get_ref().len() as u64 == buf.position() {
                            // Transition to base state. We use the done flag since state is
                            // currently borrowed.
                            done = true;
                        }
                    },
                    Err(err) => {
                        debug!("error writing to client; disconnecting. {}", err);
                        return Status::Disconnected
                    }
                }
            },
            _ => ()
        }

        if done {
            // Reset state
            self.state = ClientState::Await;
        }

        Status::Ok
    }

    pub fn consume(&mut self, count: usize) {
        // Optimize for consuming entire contents
        if count == self.pos {
            self.pos = 0;
            return;
        }

        // Move extra bytes to front
        unsafe {
            ::std::ptr::copy(self.buf[count..self.pos].as_ptr(),
                             self.buf.as_mut_ptr(),
                             count);
        }

        self.pos -= count;
    }

    pub fn try_respond<F>(&mut self, func: &F)
        where F: Fn(&str) -> String
    {
        let mut response_buf = Vec::new();

        loop {
            // Got some bytes. Check if there's a newline in the new
            // output.  If there is, process it.
            if let Some(pos) = find_in_slice(&self.buf[..self.pos], NEWLINE) {
                {
                    // The command is all of the bytes in the buffer
                    // leading up to the newline.
                    let command = &self.buf[..pos];

                    match str::from_utf8(command) {
                        Ok(command) => {
                            let response = func(command);
                            response_buf.extend_from_slice(response.as_bytes());
                            response_buf.push(NEWLINE);
                        },
                        Err(_) => debug!("command is invalid utf8"),
                    }
                }

                // Move leftover bytes to front of buffer and update pos
                // XXX off-by-1 error if newline is *exactly* at end of
                // buffer.
                let count = pos + 1;
                self.consume(count);
            } else {
                break;
            }
        }

        if !response_buf.is_empty() {
            self.state = ClientState::Responding(io::Cursor::new(response_buf));
        }
    }

    pub fn read<F>(&mut self,
                   _event_loop: &mut EventLoop<ServerInner>,
                   func: &F)
                   -> Status
        where F: Fn(&str) -> String
    {
        match self.socket.try_read(&mut self.buf[self.pos..]) {
            Ok(Some(0)) => {
                trace!("read zero bytes; disconnecting client");
                return Status::Disconnected;
            },
            Ok(Some(bytes_read)) => {
                self.pos += bytes_read;

                self.try_respond(func);

                // Handle full buffer; just try and handle the command
                if self.pos == self.buf.len() {
                    match str::from_utf8(&self.buf[..]) {
                        Ok(command) => {
                            let response = func(command);
                            let mut response = response.into_bytes();
                            response.push(NEWLINE);
                            self.state = ClientState::Responding(io::Cursor::new(response));
                        },
                        Err(_) => debug!("command is invalid utf8"),
                    }

                    let count = self.pos;
                    self.consume(count);

                    return Status::Ok;
                }

                Status::Ok
            },
            Ok(None) => {
                Status::Ok
            },
            Err(err) => {
                debug!("error reading from client; disconnecting: {}", err);
                return Status::Disconnected
            }
        }

    }
}

/// Find first occurrence of `target` in `slice`
fn find_in_slice<T: PartialEq>(slice: &[T], target: T) -> Option<usize> {
    for (index, item) in slice.iter().enumerate() {
        if item == &target {
            return Some(index);
        }
    }

    None
}

/// Server configuration
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to listen on
    host: String,

    /// Port to listen on
    port: u16,

    /// Maximum number of client connections
    max_clients: usize,

    /// Per client buffer size.
    ///
    /// This dictates the maximum length of a command.
    client_buf_size: usize,
}

impl Config {
    /// Set host address to listen on
    pub fn host<S>(mut self, val: S) -> Self
        where S: Into<String>
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

    /// Set the per-client buffer size
    pub fn client_buf_size(mut self, val: usize) -> Self {
        self.client_buf_size = val;
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
        }
    }
}

/// Handle for the server
pub struct Handle {
    sender: mio::Sender<ControlMsg>,
}

impl Handle {
    /// Request the server to shutdown gracefully
    pub fn shutdown(&self) -> io::Result<()> {
        while let Err(err) = self.sender.send(ControlMsg::Shutdown) {
            match err {
                mio::NotifyError::Full(_) => continue,
                mio::NotifyError::Closed(_) => break,
                mio::NotifyError::Io(err) => return Err(err),
            }
        }

        Ok(())
    }
}

/// The linebased TCP server
pub struct Server {
    inner: ServerInner,
    event_loop: EventLoop<ServerInner>,
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
        where F: Fn(&str) -> String + 'static + Send
    {
        let inner = try!(ServerInner::new(config, func));
        let event_loop = try!(EventLoop::new());

        Ok(Server {
            inner: inner,
            event_loop: event_loop
        })
    }

    /// Get a handle for the server so graceful shutdown can be requested
    pub fn handle(&self) -> Handle {
        Handle {
            sender: self.event_loop.channel()
        }
    }

    /// Run the event loop
    ///
    /// Blocks until a graceful shutdown is initiated or an unrecoverable error
    /// occurs.
    pub fn run(&mut self) -> io::Result<()> {
        let event_loop = &mut self.event_loop;
        let server = &mut self.inner;

        try!(event_loop.register(&server.server,
                                 SERVER_TOKEN,
                                 EventSet::readable() | EventSet::hup(),
                                 PollOpt::level()));
        try!(event_loop.run(server));

        Ok(())
    }
}

/// mio::Handler implementation
///
/// Can't implement mio::Handler for `Server` itself since that holds the
/// `EventLoop`.
struct ServerInner {
    server: TcpListener,
    clients: Slab<Client>,
    handler: Box<Fn(&str) -> String + Send>,
    config: Config,
}

impl ServerInner {
    /// Create new ServerInner
    pub fn new<F>(config: Config, func: F) -> Result<ServerInner>
        where F: Fn(&str) -> String + 'static + Send
    {
        let address = try!(format!("{host}:{port}", host=config.host, port=config.port).parse());
        let server = try!(TcpListener::bind(&address));

        let slab = Slab::new_starting_at(mio::Token(1), config.max_clients);

        Ok(ServerInner {
            server: server,
            clients: slab,
            handler: Box::new(func),
            config: config,
        })
    }
}

#[doc(hidden)]
pub enum ControlMsg {
    /// Stop the event loop
    Shutdown,
}

/// Accepts a client connection
///
/// This exists to work around borrowck at the call site.
#[inline]
fn accept(clients: &mut Slab<Client>, config: &Config, socket: TcpStream) -> Option<Token> {
    clients.insert_with(|token| Client::new(socket, token, config))
}

impl mio::Handler for ServerInner {
    type Timeout = ();
    type Message = ControlMsg;

    fn ready(&mut self, event_loop: &mut EventLoop<ServerInner>, token: Token, events: EventSet) {
        match token {
            SERVER_TOKEN => {
                debug!("accepting connection");
                match self.server.accept() {
                    Ok(Some((socket, _addr))) => {
                        let token = match accept(&mut self.clients, &self.config, socket) {
                            Some(token) => token,
                            None => {
                                info!("rejecting client; max connections reached");
                                return;
                            }
                        };

                        let opt = PollOpt::edge() | PollOpt::oneshot();
                        if let Err(err) = event_loop.register(&self.clients[token].socket,
                                                              token,
                                                              self.clients[token].interest(),
                                                              opt)
                        {
                            warn!("Couldn't register new client; {}", err);
                            self.clients.remove(token);
                        }
                    },
                    Ok(None) => {
                        // pass
                    },
                    Err(err) => {
                        error!("listener.accept() error; {}", err);
                        event_loop.shutdown();
                    },
                }
            }

            // Client token
            _ => {
                let status = {
                    let client = &mut self.clients[token];
                    if events.is_readable() {
                        client.read(event_loop, &&*self.handler)
                    } else if events.is_writable() {
                        client.write(event_loop)
                    } else if events.is_hup() {
                        Status::Disconnected
                    } else {
                        Status::Ok
                    }
                };

                match status {
                    Status::Disconnected => {
                        trace!("closing client connection");
                        self.clients.remove(token);
                    },
                    Status::Ok => {
                        if let Err(err) = self.clients[token].reregister(event_loop) {
                            warn!("Failed to reregister client interest; {}", err);
                            self.clients.remove(token);
                        }
                    },
                }
            },
        }
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Self>, msg: ControlMsg) {
        match msg {
            ControlMsg::Shutdown => event_loop.shutdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpStream;
    use std::thread;

    use super::{Config, Server, find_in_slice, NEWLINE, Handle};

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
        stream: TcpStream,
        buf: Vec<u8>,
        pos: usize,
    }

    impl Client {
        /// Create a client
        ///
        /// Any errors will panic since this is for testing only. The server is
        /// assumed to be on the default port. Performance is not a
        /// consideration here; only ergonomics, correctness, and failing early.
        pub fn new(config: &Config) -> Client {
            let stream = Client::connect(config);

            Client {
                stream: stream,
                buf: vec![0u8; 2048],
                pos: 0,
            }
        }

        /// Connect to the server
        ///
        /// Retries as long as error is connection refused. I guess this can
        /// mean tests hang if something is wrong. Oh well.
        fn connect(config: &Config) -> TcpStream {
            loop {
                match TcpStream::connect(format!("{}:{}", config.host, config.port).as_str()) {
                    Ok(stream) => return stream,
                    Err(err) => {
                        match err.kind() {
                            ::std::io::ErrorKind::ConnectionRefused => continue,
                            _ => panic!("failed to connect; {}", err),
                        }
                    }
                }
            }
        }

        /// Sends all bytes to the remote
        pub fn send<B>(&mut self, bytes: B)
            where B: AsByteSlice
        {
            use ::std::io::Write;
            self.stream.write_all(bytes.as_byte_slice()).expect("successfully send bytes");
            self.stream.write_all(b"\n").expect("successfully send bytes");
        }

        /// Receive the next line.
        ///
        /// Extra data is buffered internally.
        pub fn recv(&mut self) -> Option<String> {
            use ::std::io::Read;

            // Return next line if it's already buffered
            if let Some(s) = self.string_from_buf() {
                return Some(s);
            }

            let got = self.stream.read(&mut self.buf[self.pos..]).expect("read gets bytes");
            assert!(got != 0);

            self.pos += got;

            self.string_from_buf()
        }

        pub fn string_from_buf(&mut self) -> Option<String> {
            if let Some(pos) = find_in_slice(&self.buf[..self.pos], NEWLINE) {
                let s = ::std::str::from_utf8(&self.buf[..pos]).expect("valid utf8").to_owned();

                // Consume bytes
                let consume = pos + 1;
                self.buf.drain(..consume);
                self.pos -= consume;

                Some(s)
            } else {
                None
            }
        }

        pub fn expect(&mut self, s: &str) {
            let got = self.recv().unwrap();
            assert_eq!(got.as_str(), s);
        }
    }

    fn run_server(config: &Config) -> TestHandle {
        let config = config.to_owned();

        let mut server = Server::new(config, |query| {
            match query {
                "version" => {
                    String::from("0.1.0")
                },
                _ => {
                    String::from("unknown command")
                }
            }
        }).unwrap();

        let command_handle = server.handle();

        let thread_handle = ::std::thread::spawn(move || {
            server.run().unwrap();
        });

        TestHandle {
            thread: Some(thread_handle),
            handle: command_handle,
        }
    }

    /// Handle wrapping test server
    ///
    /// Requests graceful shutdown and joins with thread on drop
    pub struct TestHandle {
        thread: Option<thread::JoinHandle<()>>,
        handle: Handle,
    }

    impl Drop for TestHandle {
        fn drop(&mut self) {
            let _ = self.handle.shutdown();
            if let Some(handle) = self.thread.take() {
                handle.join().unwrap();
            }
        }
    }


    #[test]
    fn it_works() {
        let config = Config::default();
        let _server = run_server(&config);

        {
            let mut client = Client::new(&config);
            client.send("version");
            client.expect("0.1.0");
            client.send("nope");
            client.expect("unknown command");
        }
    }

    #[test]
    fn client_message_larger_than_read_buf() {
        let config = Config::default().client_buf_size(8).port(5500);
        let _server = run_server(&config);

        {
            let mut client = Client::new(&config);
            client.send("123456789"); // send more than buf size of 8
            client.expect("unknown command"); // first 8 bytes trigger this response
            client.expect("unknown command"); // "9\n" triggers this response
            // commands should continue to work
            client.send("version");
            client.expect("0.1.0");
        }
    }

    #[test]
    fn send_empty_line() {
        let config = Config::default().port(5501);
        let _server = run_server(&config);

        {
            let mut client = Client::new(&config);
            client.send("");
            client.expect("unknown command");
            // commands should continue to work
            client.send("version");
            client.expect("0.1.0");
        }
    }

    #[test]
    fn multiple_commands_received_at_once() {
        let config = Config::default().port(5502);
        let _server = run_server(&config);

        {
            let mut client = Client::new(&config);
            client.send("version\nversion");

            // This is a bug. Second response may or may not have a prompt.
            let got = client.recv().unwrap();
            assert!(got.contains("0.1.0"));
        }
    }

    #[test]
    fn exceed_max_clients() {
        let config = Config::default().max_clients(1).port(5503);
        let _server = run_server(&config);

        {
            let mut client = Client::new(&config);
            {
                // should get disconnected immediately
                let _client = Client::new(&config);
            }
            client.send("version");
            client.expect("0.1.0");
        }
    }
}
