linebased
=========

Line based TCP server

## About

Simple line-based TCP server for implementing command interfaces. There's no
authentication or TLS support. Commands are handled synchronously as in Redis.
This library doesn't spawn any threads; that is left to the user. The server
uses an event loop internally to multiplex client connections.

Error handling is coming soon! Be warned, this initial version is very
success-path.

## Usage

Nothing is configurable right now, but that will be changing shortly. A closure
is provided to the server at startup. Match on queries and return a response.

```rust
let mut server = CommandServer::new(|query| {
    match query {
        "version" => String::from("0.1.0"),
        _ => String::from("unknown command"),
    }
});

server.run();
```

This can be accessed over netcat like so:

```
jwilm@jwilm-desk ➜ nc localhost 7343
Connected
> arst
unknown command
> version
0.1.0
```
