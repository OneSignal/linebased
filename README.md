linebased
=========

Line based TCP server

## About

Simple line-based TCP server for implementing command interfaces. There's no
authentication or TLS support. Commands are handled synchronously as in Redis.
The server uses an event loop internally to multiplex client connections.

## Usage

```rust
let config = Config::default()
        .host("127.0.0.1")
        .port(5555)
        .max_clients(32)
        .client_buf_size(1024)
        .welcome_message("Welcome to the jungle")
        .prompt(">>> ");

let mut server = Server::new(config, |query| {
    match query {
        "version" => String::from("0.1.0"),
        _ => String::from("unknown command"),
    }
}).unwrap();

// server is a `Handle`. Call `join` when you want it to stop. It's running on a
// thread.
server.join().unwrap().unwrap();
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
