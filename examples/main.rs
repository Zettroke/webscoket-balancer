use webscoket_balancer::kappa;
use webscoket_balancer::websocket::WebsocketServer;

#[tokio::main]
async fn main() {
    WebsocketServer::new().address("127.0.0.1:1337").run().await;
    println!("{}", kappa());
}