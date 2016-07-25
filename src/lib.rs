//! Add a TCP query server to your program with ease
//!
//! I've found myself wanting a drop-in TCP server for some programs recently.
//! This crate exposes such a server which uses `mio` to multiplex clients and
//! runs it in a dedicated thread. Communication with the rest of the program
//! needs to happen through thread-safe APIs. Come to think of it, forcing the
//! thread on users is kind of cumbersome. TODO remove thread.
#![warn(missing_docs)]
extern crate mio;
#[macro_use]
extern crate log;

use std::str;
use std::io;
use std::thread;

use mio::{EventLoop, Token, EventSet, PollOpt};
use mio::tcp::TcpListener;
use mio::tcp::TcpStream;
use mio::util::Slab;
use mio::TryRead;
use mio::TryWrite;

const NEWLINE: u8 = 0x0a;

mod error;

pub use error::Result;
pub use error::Error;

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
        // Build initial connect response. It's the welcome message, a newline, and the prompt.
        let mut message = Vec::new();
        message.extend_from_slice(config.welcome_message.as_bytes());
        message.push(NEWLINE);
        message.extend_from_slice(config.prompt.as_bytes());

        Client {
            token: token,
            socket: stream,
            state: ClientState::Responding(io::Cursor::new(message)),
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
    pub fn reregister(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
        let opt = PollOpt::edge() | PollOpt::oneshot();
        event_loop.reregister(&self.socket, self.token, self.interest(), opt)
    }

    pub fn write(&mut self, _event_loop: &mut EventLoop<Server>) -> Status {
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

    pub fn try_respond<F>(&mut self, func: &F, config: &Config)
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
            response_buf.extend_from_slice(config.prompt.as_bytes());
            self.state = ClientState::Responding(io::Cursor::new(response_buf));
        }
    }

    pub fn read<F>(&mut self,
                   _event_loop: &mut EventLoop<Server>,
                   func: &F,
                   config: &Config)
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

                self.try_respond(func, config);

                // Handle full buffer; just try and handle the command
                if self.pos == self.buf.len() {
                    match str::from_utf8(&self.buf[..]) {
                        Ok(command) => {
                            let response = func(command);
                            let mut response = response.into_bytes();
                            response.push(NEWLINE);
                            response.extend_from_slice(config.prompt.as_bytes());
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
    /// Client prompt
    prompt: String,

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

    /// Welcome message
    ///
    /// This message is displayed by clients when they connect.
    welcome_message: String,
}

impl Config {
    /// prompt is displayed to client whenever they may submit a command
    pub fn prompt<S>(mut self, val: S) -> Self
        where S: Into<String>
    {
        self.prompt = val.into();
        self
    }

    /// Set host address to listen on
    pub fn host<S>(mut self, val: S) -> Self
        where S: Into<String>
    {
        self.host = val.into();
        self
    }

    /// Set welcome message which client receives upon connection
    pub fn welcome_message<S>(mut self, val: S) -> Self
        where S: Into<String>
    {
        self.welcome_message = val.into();
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
            prompt: "> ".into(),
            host: "127.0.0.1".into(),
            port: 7343,
            max_clients: 32,
            client_buf_size: 1024,
            welcome_message: "Connected".into(),
        }
    }
}

/// The linebased TCP server
pub struct Server {
    server: TcpListener,
    clients: Slab<Client>,
    handler: Box<Fn(&str) -> String + Send>,
    config: Config,
}

impl Server {
    /// Create a new server
    ///
    /// # Examples
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
    pub fn new<F>(config: Config, func: F) -> Result<Handle>
        where F: Fn(&str) -> String + 'static + Send
    {
        let address = try!(format!("{host}:{port}", host=config.host, port=config.port).parse());
        let server = try!(TcpListener::bind(&address));

        let slab = Slab::new_starting_at(mio::Token(1), config.max_clients);

        let server = Server {
            server: server,
            clients: slab,
            handler: Box::new(func),
            config: config,
        };

        let handle = try!(server.run());
        Ok(handle)
    }

    /// Run the server on current thread
    ///
    /// This call blocks while the server runs
    fn run(self) -> io::Result<Handle> {
        let mut event_loop = try!(EventLoop::new());

        let sender = event_loop.channel();
        let mut server = self;

        let thread = ::std::thread::spawn(move || {
            event_loop.register(&server.server,
                                SERVER_TOKEN,
                                EventSet::readable(),
                                PollOpt::level()).expect("server accepting");
            info!("server running");
            event_loop.run(&mut server).expect("event loop ok");
        });

        Ok(Handle {
            thread: Some(thread),
            sender: sender,
        })
    }
}

/// Handle for server returned from `Server::new()`
pub struct Handle {
    thread: Option<thread::JoinHandle<()>>,
    sender: ::mio::Sender<ControlMsg>,
}

impl Handle {
    /// Shut down the server and join its thread
    pub fn join(&mut self) -> Option<thread::Result<()>> {
        match self.thread.take() {
            Some(handle) => {
                while let Err(err) = self.sender.send(ControlMsg::Shutdown) {
                    match err {
                        mio::NotifyError::Full(_) => continue,
                        mio::NotifyError::Closed(_) => break,
                        mio::NotifyError::Io(err) => {
                            // Not sure what to do here. Join will probably block indefinitely.
                            warn!("cannot send shutdown message; {}", err);
                            break;
                        },
                    }
                }
                Some(handle.join())
            },
            None => None,
        }
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

impl mio::Handler for Server {
    type Timeout = ();
    type Message = ControlMsg;

    fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
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
                        client.read(event_loop, &&*self.handler, &self.config)
                    } else if events.is_writable() {
                        client.write(event_loop)
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

const SERVER_TOKEN: Token = Token(0);

#[cfg(test)]
mod tests {
    use super::{Config, Server, find_in_slice, NEWLINE, Handle};
    use std::net::TcpStream;

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

    fn run_server(config: &Config) -> Handle {
        let config = config.to_owned();

        Server::new(config, |query| {
            match query {
                "version" => {
                    String::from("0.1.0")
                },
                _ => {
                    String::from("unknown command")
                }
            }
        }).expect("create server ok")
    }


    #[test]
    fn it_works() {
        let config = Config::default();
        let mut server = run_server(&config);

        {
            let mut client = Client::new(&config);
            client.expect("Connected");
            client.send("version");
            client.expect("> 0.1.0");
            client.send("nope");
            client.expect("> unknown command");
        }

        server.join().unwrap().unwrap();
    }

    #[test]
    fn client_message_larger_than_read_buf() {
        let config = Config::default().client_buf_size(8).port(5500);
        let mut server = run_server(&config);

        {
            let mut client = Client::new(&config);
            client.expect("Connected");
            client.send("123456789"); // send more than buf size of 8
            client.expect("> unknown command"); // first 8 bytes trigger this response
            client.expect("> unknown command"); // "9\n" triggers this response
            // commands should continue to work
            client.send("version");
            client.expect("> 0.1.0");
        }

        server.join().unwrap().unwrap();
    }

    #[test]
    fn send_empty_line() {
        let config = Config::default().port(5501);
        let mut server = run_server(&config);

        {
            let mut client = Client::new(&config);
            client.expect("Connected");
            client.send("");
            client.expect("> unknown command");
            // commands should continue to work
            client.send("version");
            client.expect("> 0.1.0");
        }

        server.join().unwrap().unwrap();
    }

    #[test]
    fn multiple_commands_received_at_once() {
        let config = Config::default().port(5502);
        let mut server = run_server(&config);

        {
            let mut client = Client::new(&config);
            client.expect("Connected");
            client.send("version\nversion");
            client.expect("> 0.1.0");

            // This is a bug. Second response may or may not have a prompt.
            let got = client.recv().unwrap();
            assert!(got.contains("0.1.0"));
        }

        server.join().unwrap().unwrap();
    }

    #[test]
    fn exceed_max_clients() {
        let config = Config::default().max_clients(1).port(5503);
        let mut server = run_server(&config);

        {
            let mut client = Client::new(&config);
            {
                // should get disconnected immediately
                let _client = Client::new(&config);
            }
            client.expect("Connected");
            client.send("version");
            client.expect("> 0.1.0");
        }

        server.join().unwrap().unwrap();
    }
}
