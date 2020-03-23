use webscoket_balancer::kappa;
use webscoket_balancer::websocket::{WebsocketServerBuilder, WebsocketConnection};
use webscoket_balancer::proxy::ProxyServer;
use std::sync::Arc;
use tokio::io::{BufReader, AsyncBufReadExt};
use webscoket_balancer::location_manager::LocationManager;
use std::sync::atomic::Ordering;

// extern crate cpuprofiler;
// use cpuprofiler::PROFILER;
#[tokio::main]
async fn main() {
    let lm = Arc::new(LocationManager::new());
    lm.add_location("127.0.0.1:1338".to_string()).await;
    lm.add_location("127.0.0.1:1339".to_string()).await;
    let lmm = lm.clone();

    let ps = ProxyServer::new(lm.clone());
    let _pss = ps.clone();

    // ps.add_location("127.0.0.1:1338".to_string()).await;
    // ps.add_location("127.0.0.1:1339".to_string()).await;
// Code you want to sample goes here!

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
                        println!("{:?} got {} connections", pl.address, pl.connection_count.load(Ordering::Relaxed));
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
                            format!("{:X}", v.data.id).starts_with(res[1])
                        }).collect();
                        if found.len() > 1 {
                            println!("Found multiple connection. Enter more accurate id.");
                            for v in found.iter() {
                                println!("{:X}", v.data.id);
                            }
                        } else {
                            found[0].handle.abort();
                        }
                    }
                    println!("close");
                },
                "help" => {
                    println!("p - prints current connections")
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