extern crate linebased;

use linebased::Server;

fn main() {
    let mut server = Server::new(|query| {
        match &*query {
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
