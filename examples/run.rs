extern crate linebased;

use linebased::{Config, Server};

#[tokio::main]
async fn main() {
    env_logger::init();

    let config = Config::default()
        .host("127.0.0.1")
        .port(5555)
        .max_clients(32)
        .client_buf_size(24);

    let mut server = Server::new(config, |query| match query {
        "version" => String::from("0.1.0"),
        "hash" => String::from("aofijasodifjasklfjlkj"),
        _ => String::from("unknown command"),
    })
    .unwrap();

    server.run().await.unwrap();
}
