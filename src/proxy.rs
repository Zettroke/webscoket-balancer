use std::sync::Arc;
use crate::ServerChannel;
use futures::Future;
use crate::websocket::{RawMessage, WebsocketData, MessageOpCode};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use futures::future::AbortHandle;
use tokio::time::{delay_for, Duration};
use futures::future::abortable;
use tokio::select;
use std::pin::Pin;
use std::fmt::Debug;
use std::borrow::Borrow;
use tokio::net::tcp::{WriteHalf, ReadHalf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::collections::HashMap;
use crate::location_manager::{LocationManager, SocketWrapper, LocationManagerMessage};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry::Occupied;

// #[derive(Error)]
// pub enum LocationWebsocketError {
//
// }

#[derive(Debug)]
pub struct WsConnection {
    distribution_id: String,
    data: Arc<WebsocketData>,
    abort_handle: AbortHandle,
    // channels: oneshot::Receiver<(mpsc::Receiver<RawMessage>, mpsc::Sender<RawMessage>)>
    client_sender: mpsc::Sender<RawMessage>
}

impl WsConnection {
    async fn send_loop<T: MessageListener>(mut write: WriteHalf<'_>, recv: &mut mpsc::Receiver<RawMessage>, client_msg_handler: T) {
        // println!("send_loop");
        loop {
            match recv.recv().await {
                Some(msg) => {
                    //println!("Proxiyng msg {:?}", msg);
                    if let Some(msg) = client_msg_handler.handle(msg) {
                        crate::websocket_util::send_message(msg, &mut write).await;
                    }
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
                    match send.send(msg).await {
                        Err(_) => return,
                        _ => {}
                    }
                    // }
                },
                Err(_e) => {
                    // println!("Error proxy receive_loop {}", e);
                    return;
                }
            }
        }
    }
}

pub struct PendingMove {
    old_connections: Vec<WsConnection>,
    pending_count: AtomicU32,
    timeout_abort: AbortHandle
}

pub struct ProxyServer {
    pub distribution: dashmap::DashMap<String, Vec<WsConnection>>,
    pub pending_moves: Mutex<HashMap<String, PendingMove>>,
    pub location_manager: Arc<LocationManager>
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
        let v = b"WSPROXY_RECONNECTED";
        if msg.payload.len() == v.len() {
            msg.unmask();
            if msg.payload.as_slice() == v {
                println!("receive control message WSPROXY_RECONNECTED");
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
    pub fn new(lm: Arc<LocationManager>) -> Arc<Self> {

        let res = Arc::new(Self {
            location_manager: lm.clone(),
            distribution: DashMap::new(),
            pending_moves: Mutex::new(HashMap::new())
        });
        let s = res.clone();
        tokio::spawn(async move {
            let mut recv = lm.get_message_recv();
            loop {
                match recv.recv().await {
                    Ok(v) => {
                        match v {
                            LocationManagerMessage::MoveDistribution {distribution_id, new_loc: _} => {
                                s.clone().move_distribution(distribution_id).await;
                            }
                        }
                    },
                    Err(e) => {
                        println!("location manager channel error: {}", e);
                    }
                }
            }
        });

        res
    }

    async fn websocket_created(self: Arc<ProxyServer>, data: Arc<WebsocketData>, mut recv: mpsc::Receiver<RawMessage>, mut send: mpsc::Sender<RawMessage>) {
        let task = async move {

            let loc = self.location_manager.get_location(data.distributed_id.clone()).await;

            let mut socket: SocketWrapper = match Box::pin(loc.get_connection(data.borrow())).await {
                Some(s) => s,
                None => return
            };
            let (r, w) = socket.split();
            // println!("ProxyServer websocket_created");
            let cml = ControlMessageListener {
                data: data.clone(),
                server: self.clone()
            };
            let send_clone = send.clone();
            let (fut, handle) = abortable(async {
                select! {
                    _ = WsConnection::send_loop(w, &mut recv, cml) => {}
                    _ = WsConnection::receive_loop(r, &mut send) => {}
                };
            });
            let ws_conn = WsConnection {
                data: data.clone(),
                abort_handle: handle,
                distribution_id: data.query_params.values().next().unwrap().to_string(),
                client_sender: send_clone
            };
            self.distribution
                .entry(data.distributed_id.clone())
                .or_insert_with(|| vec![])
                .push(ws_conn);
            let _ = fut.await;

            if let Occupied(mut entry) = self.distribution.entry(data.distributed_id.clone()) {
                let conns = entry.get_mut();
                conns.iter().position(|v| v.data.id == data.id).map(|ind| conns.remove(ind));
                if conns.len() == 0 {
                    entry.remove();
                }
            }
            println!("Proxy closed");
            // println!("Ended");
        };
        // print_size(&task);
        tokio::spawn(task);
    }

    async fn websocket_closed(self: Arc<ProxyServer>, _data: Arc<WebsocketData>) {}

    pub async fn move_distribution(self: Arc<ProxyServer>, distribution_id: String) {

        println!("PS move_distribution");
        // rewrite distribution map, so all new connections will end up in new location
        let mut pending_map = self.pending_moves.lock().await;
        if pending_map.contains_key(&distribution_id) || !self.distribution.contains_key(&distribution_id) {
            return;
        }

        let mut moving_connections = vec![];

        let mut conns = self.distribution.get_mut(&distribution_id).unwrap();
        let mut i = 0;
        while i < conns.len() {
            if conns[i].distribution_id == distribution_id {
                moving_connections.push(conns.remove(i));
            } else {
                i += 1
            }
        }
        drop(conns);

        let proxy_server = self.clone();
        let d_id = distribution_id.clone();
        let (fut, handle) = abortable(async move {
            delay_for(Duration::from_secs(30)).await;
            println!("timeout for distribution id {}", d_id);
            proxy_server.pending_done(&d_id).await;
        });
        for conn in moving_connections.iter_mut() {
            conn.client_sender.send(RawMessage {
                fin: true,
                opcode: MessageOpCode::TextFrame,
                mask: false,
                mask_key: [0u8; 4],
                payload: b"WSPROXY_DISCONNECTED".to_vec()
            }).await.unwrap();
        }

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
            pm.old_connections.iter().for_each(|c|c.abort_handle.abort());
            pm.timeout_abort.abort();
        }
    }

    pub fn get_channel(self: Arc<ProxyServer>) -> Box<ProxyServerChannel> {
        Box::new(ProxyServerChannel {
            server: self.clone()
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

pub fn print_size<T>(_: &T) {
    println!("{}", std::mem::size_of::<T>());
}