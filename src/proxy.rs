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
use std::sync::atomic::{AtomicU32, Ordering};
use std::ptr::null;
use std::ops::Deref;
use std::collections::HashMap;

// #[derive(Error)]
// pub enum LocationWebsocketError {
//
// }

#[derive(Debug)]
pub struct WsConnection {
    distribution_id: String,
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
        let mut socket = TcpStream::connect(self.address.clone()).await.ok()?;
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

pub struct PendingMove {
    old_connections: Vec<WsConnection>,
    pending_count: AtomicU32,
    timeout_abort: AbortHandle
}

pub struct ProxyServer {
    pub locations: RwLock<Vec<Arc<ProxyLocation>>>,

    pub distribution: dashmap::DashMap<String, Arc<ProxyLocation>>,
    pub pending_moves: Mutex<HashMap<String, PendingMove>>
    // pub locations: Mutex<Vec<ProxyLocation>>,
    // pub connections: Mutex<Vec<WsConnection>>
}

pub trait MessageListener {
    fn handle(&self, msg: RawMessage) -> Option<RawMessage>;
}

struct ControlMessageListener {
    server: Arc<ProxyServer>,
    data: Arc<WebsocketData>
}

impl MessageListener for ControlMessageListener {
    fn handle(&self, mut msg: RawMessage) -> Option<RawMessage> {
        let v = b"WSPROXY_RECONN";
        if msg.payload.len() == v.len() {
            msg.unmask();
            if msg.payload.as_slice() == v {
                let server = self.server.clone();
                let data = self.data.clone();
                tokio::spawn(async move {
                    let mut done = false;
                    if let Some(pm) = server.pending_moves.lock().await.get(&data.distributed_id) {
                        if pm.pending_count.fetch_sub(1, Ordering::Relaxed) <= 1 {
                            done = true;
                        }
                    }
                    if done {
                        server.pending_done(&data.distributed_id).await;
                    }
                });

                return None;
            }
        }

        Some(msg)
    }
}

impl ProxyServer {
    async fn websocket_created(self: Arc<ProxyServer>, data: Arc<WebsocketData>, mut recv: mpsc::Receiver<RawMessage>, mut send: mpsc::Sender<RawMessage>) {
        let task = async move {
            let arr = self.locations.read().await;
            let loc_ind: usize = rand::thread_rng().gen_range(0, arr.len());
            // println!("Picked location #{}", loc_ind);
            let loc = arr.get(loc_ind).unwrap();
            let mut socket = match loc.get_raw_websocket(data.borrow()).await {
                Some(s) => s,
                None => return
            };
            let (r, w) = socket.split();
            // println!("ProxyServer websocket_created");
            let (rx, tx) = oneshot::channel::<(mpsc::Receiver<RawMessage>, mpsc::Sender<RawMessage>)>();
            let (fut, handle) = abortable(async {
                select! {
                    _ = WsConnection::send_loop(w, &mut recv) => {}
                    _ = WsConnection::receive_loop(r, &mut send) => {}
                };
            });
            loc.connections.lock().await.push(WsConnection {
                data: data.clone(),
                abort_handle: handle,
                channels: tx,
                distribution_id: data.query_params.values().next().unwrap().to_string()
            });
            drop(arr);
            // drop(f1); drop(f2);
            let v = fut.await;
            if let Err(_) = v {
                rx.send((recv, send));
            }
            // drop(fut);
            let mut arr = self.locations.read().await;
            if let Some(loc) = arr.get(loc_ind) {
                let mut arr = loc.connections.lock().await;
                arr.iter().position(|v| v.data.id == data.id)
                    .map(|v| arr.remove(v));
            }
            println!("Proxy closed");
            // println!("Ended");
        };
        // print_size(&task);
        tokio::spawn(task);
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

    /// returns suitable location for distribution
    async fn get_location(&self, distribution_id: String) -> Arc<ProxyLocation> {
        let locations = self.locations.read().await;
        let loc = self.distribution.entry(distribution_id).or_insert_with(|| ProxyServer::pick_new_location(&locations));
        loc.clone()
    }

    fn pick_new_location(locations: &Vec<Arc<ProxyLocation>>) -> Arc<ProxyLocation>{
        let ind = rand::thread_rng().gen_range(0, locations.len());
        return locations.get(ind).unwrap().clone();
    }

    async fn move_distribution(self: Arc<ProxyServer>, distribution_id: String) {
        let new_loc = ProxyServer::pick_new_location(self.locations.read().await.deref());
        self.move_distribution_to(distribution_id, new_loc);
    }
    async fn move_distribution_to(self: Arc<ProxyServer>, distribution_id: String, loc: Arc<ProxyLocation>) {
        /// rewrite distribution map, so all new connections will end up in new location
        let mut pending_map = self.pending_moves.lock().await;
        if pending_map.contains_key(&distribution_id) {
            return;
        }
        let mut old_loc = loc.clone();
        self.distribution.entry(distribution_id.clone()).and_modify(|map_loc| {
            std::mem::swap(map_loc, &mut old_loc);
        });

        let mut moving_connections = vec![];

        let mut conns = old_loc.connections.lock().await;
        for i in 0..conns.len() {
            if conns[i].distribution_id == distribution_id {
                moving_connections.push(conns.remove(i));
            }
        }
        drop(conns);

        let proxy_server = self.clone();
        let d_id = distribution_id.clone();
        let (fut, handle) = abortable(async move {
            delay_for(Duration::from_secs(30)).await;
            proxy_server.pending_done(&d_id).await;
        });
        let pm = PendingMove {
            pending_count: AtomicU32::new(moving_connections.len() as u32),
            old_connections: moving_connections,
            timeout_abort: handle
        };
        pending_map.insert(distribution_id, pm);
        drop(pending_map);

        tokio::spawn(fut);


    }

    async fn pending_done(&self, distribution_id: &str) {
        if let Some(pm) = self.pending_moves.lock().await.remove(distribution_id) {
            pm.pending_count.store(0, Ordering::Relaxed);
            pm.old_connections.iter().for_each(|c|c.abort_handle.abort())
        }
    }

    pub fn get_channel(self: Arc<ProxyServer>) -> Box<ProxyServerChannel> {
        Box::new(ProxyServerChannel {
            server: self.clone()
        })
    }

    pub async fn add_location(&self, addr: String) {
        self.locations.write().await.push(Arc::new(ProxyLocation {
            address: addr,
            connections: Mutex::new(Vec::new())
        }))
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