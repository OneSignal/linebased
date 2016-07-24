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
    pub fn new(stream: TcpStream, token: Token) -> Client {
        Client {
            token: token,
            socket: stream,
            state: ClientState::Responding(io::Cursor::new(b"Connected\n> ".to_vec())),
            buf: vec![0u8; 1024],
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
            self.state = ClientState::Await;
        }

        Status::Ok
    }

    pub fn read<F>(&mut self, _event_loop: &mut EventLoop<Server>, func: &F) -> Status
        where F: Fn(&str) -> String
    {
        match self.socket.try_read(&mut self.buf[self.pos..]) {
            Ok(Some(0)) => {
                return Status::Disconnected;
            },
            Ok(Some(bytes_read)) => {
                let new_begin = self.pos;
                self.pos += bytes_read;

                {
                    // Got some bytes. Check if there's a newline in the new
                    // output.  If there is, process it.
                    if let Some(newline_pos) = find_in_slice(&self.buf[new_begin..self.pos],
                                                             NEWLINE)
                    {
                        // The returned position is from a subslice. Resolve the
                        // position relative to the start of the buffer.
                        let newline_pos = new_begin + newline_pos;

                        {
                            // The command is all of the bytes in the buffer
                            // leading up to the newline.
                            let command = &self.buf[..newline_pos];

                            match str::from_utf8(command) {
                                Ok(command) => {
                                    let response = func(command);
                                    let mut response = response.into_bytes();
                                    response.push(NEWLINE);
                                    response.push('>' as u8);
                                    response.push(' ' as u8);
                                    self.state = ClientState::Responding(io::Cursor::new(response));
                                },
                                Err(_) => debug!("command is invalid utf8"),
                            }
                        }

                        // Move leftover bytes to front of buffer and update pos
                        // XXX off-by-1 error if newline is *exactly* at end of
                        // buffer.
                        let new_start = newline_pos + 1;
                        self.buf.drain(..new_start);
                        self.pos = self.pos - new_start;
                    }

                    // TODO handle case where buffer is full. This is similar to
                    // the above thing, but doesn't care about newlines. The
                    // off-by-1 error mentioned above can be handled by dealing
                    // with this first.
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

pub struct Server {
    server: TcpListener,
    clients: Slab<Client>,
    handler: Box<Fn(&str) -> String>,
}

impl Server {
    /// Create a new server
    ///
    /// TODO config, handler Fn, error handling, reduce 'static requirement on F
    pub fn new<F>(func: F) -> Result<Server>
        where F: Fn(&str) -> String + 'static
    {
        let address = try!("0.0.0.0:7343".parse());
        let server = try!(TcpListener::bind(&address));

        // 1024 possible clients; TODO config
        let slab = Slab::new_starting_at(mio::Token(1), 1024);

        Ok(Server {
            server: server,
            clients: slab,
            handler: Box::new(func),
        })
    }

    /// Run the server on current thread
    ///
    /// This call blocks while the server runs
    pub fn run(&mut self) -> io::Result<()> {
        let mut event_loop = try!(EventLoop::new());
        try!(event_loop.register(&self.server,
                                 SERVER_TOKEN,
                                 EventSet::readable(),
                                 PollOpt::level()));

        info!("server running");
        try!(event_loop.run(self));

        Ok(())
    }
}

impl mio::Handler for Server {
    type Timeout = ();
    type Message = (); // Will eventually be for returning responses

    fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
        match token {
            SERVER_TOKEN => {
                // Only receive readable events
                assert!(events.is_readable());

                debug!("accepting connection");
                match self.server.accept() {
                    Ok(Some((socket, _addr))) => {
                        let token = match self.clients
                            .insert_with(|token| Client::new(socket, token))
                        {
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
}

const SERVER_TOKEN: Token = Token(0);

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
    }
}
