# linebased

[![Build Status](https://travis-ci.org/jwilm/linebased.svg?branch=master)](https://travis-ci.org/jwilm/linebased)
[![Crates.io](https://img.shields.io/crates/v/linebased.svg)](https://crates.io/crates/linebased)

Drop-in TCP command server

## About

Simple line-based TCP server for implementing command interfaces. There's no
authentication or TLS support. Commands are handled synchronously as in Redis.
The server uses an event loop internally to multiplex client connections.

## Usage

This example is kept up-to-date on a best-effort basis. For a guaranteed acurate
example, please see the [docs].

```rust
#[tokio::main]
async fn main() {
    let config = Config::default()
        .host("127.0.0.1")
        .port(5555)
        .max_clients(32)
        .client_buf_size(1024);

    let mut server = Server::new(config, |query| {
        match query {
            "version" => String::from("0.1.0"),
            _ => String::from("unknown command"),
        }
    }).unwrap();

    server.run().await.unwrap();
}
```

This can be accessed over netcat like so:

```
jwilm@jwilm-desk ➜ nc localhost 5555
arst
unknown command
version
0.1.0
```

[docs]: http://blog.jwilm.io/linebased/linebased/index.html
