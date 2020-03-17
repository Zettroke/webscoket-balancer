use std::sync::{Arc};
use crate::ServerChannel;
use tokio::sync::mpsc::{Sender, Receiver};
use futures::Future;
use crate::websocket::{RawMessage, WebsocketData, MessageOpCode};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use futures::future::AbortHandle;
use tokio::time::{delay_for, Duration};
use futures::future::abortable;
use tokio::select;
use std::pin::Pin;
use rand::Rng;
use tokio::net::TcpStream;
use thiserror::Error;
use httparse::{Request, Header, Response};
use std::fmt::Write;
use bytes::Buf;
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use std::borrow::Borrow;
use tokio::net::tcp::{WriteHalf, ReadHalf};
use tokio::sync::RwLock;

// #[derive(Error)]
// pub enum LocationWebsocketError {
//
// }

pub struct WsConnection {
    data: Arc<WebsocketData>,
    abort_handle: AbortHandle
}

impl WsConnection {
    async fn send_loop(mut write: WriteHalf<'_>, mut recv: mpsc::Receiver<RawMessage>) {
        println!("send_loop");
        loop {
            match recv.recv().await {
                Some(msg) => {
                    println!("Proxying msg: {:?}", msg);
                    crate::websocket_util::send_message(msg, &mut write).await;
                },
                None => {
                    println!("Stopped");
                    return;
                }
            }
        }
    }

    async fn receive_loop(mut read: ReadHalf<'_>, mut send: mpsc::Sender<RawMessage>) {
        loop {
            match crate::websocket_util::receive_message(&mut read).await {
                Ok(msg) => {
                    // if let MessageOpCode::TextFrame = msg.opcode {
                        println!("Receive msg from proxy {:?}", msg);
                        send.send(msg).await;
                    // }
                },
                Err(e) => {
                    println!("Error proxy receive_loop {}", e);
                    return;
                }
            }
        }
    }
}

pub struct ProxyLocation {
    address: String,
    connections: Mutex<Vec<WsConnection>>
}

impl ProxyLocation {
    /// Returns TcpStream with completed handshake.
    async fn get_raw_websocket(&self, data: &WebsocketData) -> Option<TcpStream> {

        let mut query_params: String = data.query_params.iter().map(|(k, v)| {
            k.clone() + "=" + v
        }).collect::<Vec<String>>().join("&");
        let path = data.path.to_owned() + "?" + query_params.as_str();

        let mut body = bytes::BytesMut::new();
        body.write_str("GET ");

        body.write_str(data.path.as_str());
        if data.query_params.len() > 0 {
            body.write_char('?');
            for (ind, (k, v)) in data.query_params.iter().enumerate() {
                body.write_str(k);
                body.write_char('=');
                body.write_str(v);
                if ind != data.query_params.len() - 1 {
                    body.write_char('?');
                }
            }
        }
        body.write_str(" HTTP/1.1\r\n");

        for (k, v) in data.headers.iter() {
            if k != "sec-websocket-extensions" {
                body.write_str(k);
                body.write_str(": ");
                body.write_str(v);
                body.write_str("\r\n");
            }
        }
        body.write_str("\r\n");
        println!("{:#?}", data.headers);
        println!("{}", String::from_utf8(body.bytes().to_vec()).unwrap());
        let mut socket = TcpStream::connect(self.address.clone()).await.ok()?;
        socket.write_all(body.bytes()).await.unwrap();
        socket.set_keepalive(Some(Duration::from_secs(20)));
        // println!("keepalive {:?}", socket.keepalive());

        let mut buff= [0u8; 2048];
        let mut prev_packet_ind = 0;
        let mut handshake = Vec::new();
        loop {
            let n = socket.read(&mut buff).await.ok()?;
            println!("read {} bytes", n);
            handshake.extend_from_slice(&buff[0..n]);
            if handshake[n-2..n] == *b"\r\n" ||
                prev_packet_ind != 0 && handshake[prev_packet_ind-1..prev_packet_ind+1] == *b"\r\n" {
                break;
            }
            prev_packet_ind += n;
            if n == 0 {
                return None;
            }
        }
        // TODO: Handshake check!
        Some(socket)
    }
}

pub struct ProxyServer {
    pub locations: RwLock<Vec<ProxyLocation>>,
    // pub connections: Mutex<Vec<WsConnection>>
}

impl ProxyServer {
    async fn websocket_created(self: Arc<ProxyServer>, data: Arc<WebsocketData>, recv: Receiver<RawMessage>, send: Sender<RawMessage>) {
        tokio::spawn(async move {
            let arr = self.locations.read().await;
            let loc_ind: usize = rand::thread_rng().gen_range(0, arr.len());
            println!("Picked location #{}", loc_ind);
            let loc = arr.get(loc_ind).unwrap();
            let mut socket = loc.get_raw_websocket(data.borrow()).await.unwrap();
            let (r, w) = socket.split();
            println!("ProxyServer websocket_created");
            let fut = abortable(async move {
                select! {
                    _ = WsConnection::send_loop(w, recv) => {}
                    _ = WsConnection::receive_loop(r, send) => {}
                };
            });
            loc.connections.lock().await.push(WsConnection {
                data: data.clone(),
                abort_handle: fut.1
            });
            drop(arr);
            fut.0.await;
            println!("Proxy closed");
            let mut arr = self.locations.read().await;
            if let Some(loc) = arr.get(loc_ind) {
                let mut arr = loc.connections.lock().await;
                arr.iter().position(|v| v.data.id == data.id)
                    .map(|v| arr.remove(v));
            }
            println!("Ended");
        });
    }

    async fn websocket_closed(self: Arc<ProxyServer>, data: Arc<WebsocketData>) {
        // TODO: implement
        println!("websocket_closed");
        // unimplemented!();
        // let arr = self.connections.lock().await;
        // arr.iter().find(|v| v.data.id == data.id).map(|v| {
        //     v.abort_handle.abort();
        //     // arr.remove(ind)
        // });
    }

    pub fn get_channel(self: Arc<ProxyServer>) -> Box<ProxyServerChannel> {
        Box::new(ProxyServerChannel {
            server: self.clone()
        })
    }

    pub async fn add_location(&self, addr: String) {
        self.locations.write().await.push(ProxyLocation {
            address: addr,
            connections: Mutex::new(Vec::new())
        })
    }

}


pub struct ProxyServerChannel {
    server: Arc<ProxyServer>
}

impl ServerChannel for ProxyServerChannel {
    fn websocket_created(&self, data: Arc<WebsocketData>, recv: Receiver<RawMessage>, send: Sender<RawMessage>) -> Pin<Box<dyn Future<Output=()> + Send>> {
        let s = self.server.clone();
        Box::pin(async move {
            s.websocket_created(data, recv, send).await;
        })
    }

    fn websocket_removed(&self, data: Arc<WebsocketData>) -> Pin<Box<dyn Future<Output=()> + Send>> {
        let s = self.server.clone();
        Box::pin(async move {
            s.websocket_closed(data).await;
        })
    }
}