use websocket_balancer::kappa;
use websocket_balancer::websocket::{WebsocketServerBuilder, WebsocketConnection};
use websocket_balancer::proxy::ProxyServerBuilder;
use std::sync::Arc;
use tokio::io::{BufReader, AsyncBufReadExt};
use websocket_balancer::location_manager::{LocationManager, ProxyLocation};
use std::sync::atomic::Ordering;
use http::uri::Uri;
use std::convert::TryInto;

// #[macro_use] extern crate log;

#[tokio::main]
async fn main() {
    env_logger::init();
    let lm = Arc::new(LocationManager::new());
    // let uri: Uri = "wss://127.0.0.1:13338".try_into().unwrap();
    lm.add_location(ProxyLocation::new().uri("wss://127.0.0.1:13338".try_into().unwrap()).secure(false).build().unwrap()).await;
    lm.add_location(ProxyLocation::new().uri("wss://127.0.0.1:13339".try_into().unwrap()).secure(false).build().unwrap()).await;
    // lm.add_location().await;
    // lm.add_location("127.0.0.1:13339".to_string()).await;
    let lmm = lm.clone();

    let ps = ProxyServerBuilder::new()
        .location_manager(lm.clone())
        .move_start_message("{\"notification\":true,\"method\":\"WSPROXY_MOVE_START\",\"data\":{}}".to_string())
        .move_done_message("WSPROXY_MOVE_DONE".to_string())
        .move_end_message("{\"notification\":true,\"method\":\"WSPROXY_MOVE_END\",\"data\":{}}".to_string())
        .build();

    let _pss = ps.clone();
    tokio::spawn(ps.run());

    let s = WebsocketServerBuilder::new()
        .address("127.0.0.1:1337")
        .channel(ps.get_channel())
        .build();
    let ss = s.clone();
    tokio::spawn(async move {
        loop {
            let mut v = String::new();
            let mut reader = BufReader::new(tokio::io::stdin());
            reader.read_line(&mut v).await.unwrap();
            v.remove(v.len()-1);
            let res: Vec<&str> = v.split(' ').collect();

            match res[0] {
                "p" => {
                    let arr = s.get_connections().await;
                    if arr.len() == 0 {
                        println!("There are no connections!");
                    } else {
                        println!("Connections:")
                    }
                    let cnt = res.get(1).map(|v| v.parse::<usize>().unwrap_or(10)).unwrap_or(10);
                    for conn in arr.iter().take(cnt) {
                        println!("  {:?}", conn.data);
                    }
                    if arr.len() > cnt {
                        println!("And {} more...", arr.len() - cnt);
                    }
                },
                "proxies" => {
                    println!("proxies:");
                    let v = lmm.locations.read().await;
                    println!("got 1 lock");
                    for pl in v.iter() {
                        println!("{:?} got {} connections. dead - {}", pl.uri, pl.connection_count.load(Ordering::Relaxed), pl.dead.load(Ordering::Relaxed));
                    }
                },
                "proxy" => {
                    if res.get(1) == Some(&"move") {
                        let d_id = res[2].to_string();
                        lmm.move_distribution(d_id).await;
                    }
                },
                "close" => {
                    if res.len() == 2 {
                        let arr = s.get_connections().await;
                        let found: Vec<&WebsocketConnection> = arr.iter().filter(|v| {
                            v.data.id.starts_with(res[1])
                        }).collect();
                        if found.len() > 1 {
                            println!("Found multiple connection. Enter more accurate id.");
                            for v in found.iter() {
                                println!("{}", v.data.id);
                            }
                        } else {
                            found[0].handle.abort();
                        }
                    }
                    println!("close");
                },
                "help" => {
                    println!("p [n] - prints n connections");
                    println!("proxies - show ");
                    println!("proxy move <distribution_id> - move distribution to another location");
                }
                _ => {
                    println!("Unknown command!");
                }
            }
        }
    });
    ss.run().await;
    println!("{}", kappa());


}