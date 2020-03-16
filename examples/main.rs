use webscoket_balancer::kappa;
use webscoket_balancer::websocket::WebsocketServerBuilder;
use webscoket_balancer::proxy::ProxyServer;
use tokio::sync::Mutex;
use std::sync::Arc;

#[tokio::main]
async fn main() {

    let ps = Arc::new(ProxyServer {
        connections: Mutex::new(Vec::new())
    });



    let s = WebsocketServerBuilder::new().address("127.0.0.1:1337").channel(ps.get_channel()).build();
    s.run().await;
    println!("{}", kappa());
}