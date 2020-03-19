use std::sync::{Arc};
use crate::ServerChannel;
use futures::Future;
use crate::websocket::{RawMessage, WebsocketData, MessageOpCode};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use futures::future::AbortHandle;
use tokio::time::{delay_for, Duration};
use futures::future::abortable;
use tokio::select;
use std::pin::Pin;
use rand::Rng;
use tokio::net::TcpStream;
use thiserror::Error;
use httparse::{Request, Header, Response};
use std::fmt::{Write, Debug, Formatter, Pointer};
use bytes::Buf;
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use std::borrow::Borrow;
use tokio::net::tcp::{WriteHalf, ReadHalf};
use tokio::sync::RwLock;

// #[derive(Error)]
// pub enum LocationWebsocketError {
//
// }

#[derive(Debug)]
pub struct WsConnection {
    data: Arc<WebsocketData>,
    abort_handle: AbortHandle,
    channels: oneshot::Receiver<(mpsc::Receiver<RawMessage>, mpsc::Sender<RawMessage>)>
}

impl WsConnection {
    async fn send_loop(mut write: WriteHalf<'_>, recv: &mut mpsc::Receiver<RawMessage>) {
        // println!("send_loop");
        loop {
            match recv.recv().await {
                Some(msg) => {
                    // println!("Proxying msg: {:?}", msg);
                    crate::websocket_util::send_message(msg, &mut write).await;
                },
                None => {
                    // println!("Stopped");
                    return;
                }
            }
        }
    }

    async fn receive_loop(mut read: ReadHalf<'_>, send: &mut mpsc::Sender<RawMessage>) {
        loop {
            match crate::websocket_util::receive_message(&mut read).await {
                Ok(msg) => {
                    // if let MessageOpCode::TextFrame = msg.opcode {
                    //     println!("Receive msg from proxy {:?}", msg);
                        send.send(msg).await;
                    // }
                },
                Err(e) => {
                    // println!("Error proxy receive_loop {}", e);
                    return;
                }
            }
        }
    }
}

// #[derive(Debug)]
pub struct ProxyLocation {
    pub address: String,
    pub connections: Mutex<Vec<WsConnection>>
}

// impl Debug for ProxyLocation {
//     fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
//         struct Temp {
//             connections: UnsafeCell<Vec<WsConnection>>
//         }
//         let mut uct = f.debug_struct("ProxyLocation");
//         uct.field("address", &self.address);
//         /// ABSOLUTELY UNSAFE!!!
//         let v = unsafe {
//             let cell = std::mem::transmute::<&Mutex<Vec<WsConnection>>, &Temp>(&self.connections);
//             &*cell.connections.get()
//         };
//         uct.field("connections", v)
//             .finish()
//     }
// }

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
            if k == "host" {
                body.write_str("host: ");
                body.write_str(self.address.as_str());
                body.write_str("\r\n");
            } else if k != "sec-websocket-extensions" {
                body.write_str(k);
                body.write_str(": ");
                body.write_str(v);
                body.write_str("\r\n");
            }
        }
        body.write_str("\r\n");
        // println!("{:#?}", data.headers);
        // println!("body {}", String::from_utf8(body.bytes().to_vec()).unwrap());
        let mut socket = match TcpStream::connect(self.address.clone()).await {
            Ok(s) => s,
            Err(e) => {
                println!("socket connect error: {}", e);
                return None;
            }
        };
        socket.write_all(body.bytes()).await.unwrap();
        // println!("keepalive {:?}", socket.keepalive());

        let mut buff= [0u8; 2048];
        let mut prev_packet_ind = 0;
        let mut handshake = Vec::new();
        loop {
            let n = socket.read(&mut buff).await.ok()?;

            handshake.extend_from_slice(&buff[0..n]);

            let mut ind = if prev_packet_ind == 0 {
                0
            } else {
                if prev_packet_ind > 3 { prev_packet_ind-3 } else { 0 }
            };
            let mut done = false;
            loop {
                match memchr::memchr(b'\r', &handshake[ind..handshake.len()]) {
                    Some(ind_f) => {
                        let v = ind + ind_f;
                        if handshake.len() - v >= 4 {
                            if handshake[v+1..v+4] == [b'\n', b'\r', b'\n'] {
                                done = true;
                                break;
                            }
                        }
                        ind = v+1;
                    },
                    None => break
                }
            }

            if done {
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
    // pub locations: Mutex<Vec<ProxyLocation>>,
    // pub connections: Mutex<Vec<WsConnection>>
}

impl ProxyServer {
    async fn websocket_created(self: Arc<ProxyServer>, data: Arc<WebsocketData>, mut recv: mpsc::Receiver<RawMessage>, mut send: mpsc::Sender<RawMessage>) {
        let task = async move {
            let arr = self.locations.read().await;
            let loc_ind: usize = rand::thread_rng().gen_range(0, arr.len());
            // println!("Picked location #{}", loc_ind);
            let loc = arr.get(loc_ind).unwrap();
            let mut socket = match Box::pin(loc.get_raw_websocket(data.borrow())).await {
                Some(s) => s,
                None => return
            };
            drop(arr);
            tokio::spawn(self.run_proxy_connection(loc_ind, data, socket, recv, send));
        };
        tokio::spawn(task);
    }

    async fn run_proxy_connection(self: Arc<ProxyServer>, loc_id: usize, data: Arc<WebsocketData>, mut socket: TcpStream, mut recv: mpsc::Receiver<RawMessage>, mut send: mpsc::Sender<RawMessage>) {
        let (r, w) = socket.split();
        let (rx, tx) = oneshot::channel::<(mpsc::Receiver<RawMessage>, mpsc::Sender<RawMessage>)>();
        let (fut, handle) = abortable(async {
            select! {
                _ = WsConnection::send_loop(w, &mut recv) => {}
                _ = WsConnection::receive_loop(r, &mut send) => {}
            };
        });

        if let Some(loc) = self.locations.read().await.get(loc_id) {
            loc.connections.lock().await.push(WsConnection {
                data: data.clone(),
                abort_handle: handle,
                channels: tx
            });
        }

        match fut.await {
            Ok(_) => {
                println!("Proxy closed");
            },
            Err(futures::future::Aborted) => {
                println!("Aborted!!!");
                rx.send((recv, send));

            }
        }

        if let Some(loc) = self.locations.read().await.get(loc_id) {
            let mut arr = loc.connections.lock().await;
            arr.iter().position(|v| v.data.id == data.id)
                .map(|v| arr.remove(v));
        }
    }

    async fn websocket_closed(self: Arc<ProxyServer>, data: Arc<WebsocketData>) {
        // let mut arr = self.locations.read().await;
        // if let Some(loc) = arr.get(0) {
        //     let mut arr = loc.connections.lock().await;
        //     let mut ws_conn = arr.iter_mut().find(|v| v.data.id == data.id).unwrap();
        //     let mut v = &mut ws_conn.channels;
        //     let v = v.await;
        // }
        // TODO: implement
        // println!("websocket_closed");
        // unimplemented!();
        // let arr = self.connections.lock().await;
        // arr.iter().find(|v| v.data.id == data.id).map(|v| {
        //     v.abort_handle.abort();
        //     // arr.remove(ind)
        // });
    }

    pub async fn move_connection(self: Arc<ProxyServer>, id: u128, loc_ind: usize) {
        let mut res = None;
        'outer: for v1 in self.locations.read().await.iter() {
            for conn in v1.connections.lock().await.iter_mut() {
                if conn.data.id == id {
                    conn.abort_handle.abort();
                    let data = conn.data.clone();
                    let channels = match (&mut conn.channels).await {
                        Ok(c) => c,
                        Err(e) => {
                            println!("{}", e);
                            return;
                        }
                    };
                    res = Some((data, channels));
                    break 'outer;
                }
            }
        }

        if let Some((data, channels)) = res {
            if let Some(loc) = self.locations.read().await.get(loc_ind) {
                let mut socket = loc.get_raw_websocket(&data).await.unwrap();
                tokio::spawn(self.clone().run_proxy_connection(loc_ind, data, socket, channels.0, channels.1));
            }
        }
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
    fn websocket_created(&self, data: Arc<WebsocketData>, recv: mpsc::Receiver<RawMessage>, send: mpsc::Sender<RawMessage>) -> Pin<Box<dyn Future<Output=()> + Send>> {
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

pub fn print_size<T>(t: &T) {
    println!("{}", std::mem::size_of::<T>());
}