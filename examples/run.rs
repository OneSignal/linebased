extern crate linebased;

use linebased::{Config, Server};

fn main() {
    let config = Config::default()
        .host("127.0.0.1")
        .port(5555)
        .max_clients(32)
        .client_buf_size(24)
        .welcome_message("Welcome to the jungle")
        .prompt(">>> ");

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


    server.run().unwrap();
}
