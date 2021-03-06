use std::sync::Arc;
use crate::ServerChannel;
use futures::Future;
use crate::websocket::WebsocketData;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use futures::future::AbortHandle;
use tokio::time::{delay_for, Duration, timeout_at, Instant};
use futures::future::abortable;
use tokio::select;
use std::pin::Pin;
use std::fmt::{Debug, Formatter};
use std::collections::HashMap;
use crate::location_manager::{LocationManager, LocationManagerMessage, ProxyLocation};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry::Occupied;
use std::ops::Deref;
use rand::Rng;
use crate::websocket_util::{MessageError, HandshakeError};
use crate::message::{RawMessage, MessageOpCode};
use tokio::io::{AsyncWrite, AsyncRead};
use std::sync::atomic::{Ordering, AtomicBool};

pub struct WsConnection {
    pub data: Arc<WebsocketData>,
    abort_handle: AbortHandle,
    // channels: oneshot::Receiver<(mpsc::Receiver<RawMessage>, mpsc::Sender<RawMessage>)>
    client_sender: mpsc::Sender<RawMessage>,
    is_moving: Arc<AtomicBool>
}

impl Debug for WsConnection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConnection")
            .field("data", self.data.deref())
            .finish()
    }
}

impl WsConnection {
    async fn send_loop<W: AsyncWrite + Unpin, T: MessageListener>(mut write: W, recv: &mut mpsc::Receiver<RawMessage>, client_msg_handler: T) -> std::io::Result<()> {
        // println!("send_loop");
        loop {
            match recv.recv().await {
                Some(msg) => {
                    if let Some(msg) = client_msg_handler.handle(msg) {
                        crate::websocket_util::send_message(msg, &mut write).await?;
                    }
                },
                None => {
                    return Ok(());
                }
            }
        }
    }

    async fn receive_loop<R: AsyncRead + Unpin>(mut read: R, send: &mut mpsc::Sender<RawMessage>) -> std::io::Result<()> {
        loop {
            match crate::websocket_util::receive_message(&mut read).await {
                Ok(msg) => {
                    match send.send(msg).await {
                        Err(_) => {
                            return Ok(())
                        },
                        _ => {}
                    }
                },
                Err(MessageError::Socket(e)) => {
                    return Err(e);
                },
                Err(_) => {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct PendingMove {
    old_connections: Vec<WsConnection>,
    pending_count: u32,
    timeout_abort: AbortHandle
}

pub struct ProxyServer {
    pub distribution: dashmap::DashMap<String, Vec<WsConnection>>,
    pub location_manager: Arc<LocationManager>,
    pending_moves: Mutex<HashMap<String, PendingMove>>,

    move_start_message: String,
    move_done_message: String,
    move_end_message: String
}

pub trait MessageListener {
    fn handle(&self, msg: RawMessage) -> Option<RawMessage>;
}

struct ControlMessageListener {
    server: Arc<ProxyServer>,
    data: Arc<WebsocketData>,
}

impl MessageListener for ControlMessageListener {
    fn handle(&self, mut msg: RawMessage) -> Option<RawMessage> {
        //let v = &b"WSPROXY_MOVE_DONE"[..];
        let v = self.server.move_done_message.as_bytes();
        if msg.payload.len() == v.len() {
            msg.unmask();
            if msg.payload.as_slice() == v {
                info!("Websocket {} move done!", self.data.id);
                let server = self.server.clone();
                let data = self.data.clone();
                tokio::spawn(async move {
                    let mut done = false;
                    if let Some(pm) = server.pending_moves.lock().await.get_mut(&data.distribution_id) {
                        pm.pending_count -= 1;
                        if pm.pending_count == 0 {
                            done = true;
                            info!("Distribution {}: all {} connections successfully moved", data.distribution_id, pm.old_connections.len());
                        }
                    } else {

                    }
                    if done {
                        server.pending_done(&data.distribution_id).await;
                    }
                });

                return None;
            }
        }

        Some(msg)
    }
}

impl ProxyServer {
    pub fn run(self: &Arc<ProxyServer>) -> Pin<Box<dyn Future<Output=()> + Send + 'static>>{
        let s = self.clone();
        Box::pin(async move {
            let mut recv = s.location_manager.get_message_recv();
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
                        error!("Location manager channel error: {}", e);
                    }
                }
            }
        })
    }

    async fn try_get_connection<'a, T, Ret>(
        self: &'a ProxyServer,
        f: fn(&'a ProxyLocation, &'a WebsocketData) -> Ret,
        loc: &'a ProxyLocation,
        data: &'a WebsocketData
    ) -> Result<T, String>
        where Ret: Future<Output=Result<T, HandshakeError>>
    {
        let mut retries = 5u32;
        let socket: T = loop {
            let fut = timeout_at(
                Instant::now() + Duration::from_millis(500),
                Box::pin((f)(loc, data))
            );
            let res: Result<T, HandshakeError> = match fut.await {
                Ok(Ok(r)) => Ok(r),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(HandshakeError::Timeout)
            };

            match res {
                Err(e @ HandshakeError::Timeout) | Err(e @ HandshakeError::IOError(_)) => {
                    if retries == 0 {
                        self.location_manager.mark_dead(loc).await;
                        // TODO: keep trying new locations
                        error!("{:?} Couldn't connect to location {:?}", e, loc);
                        return Err(format!("{}", e));
                    } else {
                        debug!("retrying connecting {:?}, retries left: {}", loc, retries);
                        retries -= 1;
                        let t = rand::thread_rng().gen_range(1, 200);
                        delay_for(Duration::from_millis(t)).await;
                    }
                },
                Ok(v) => {
                    break v
                },
                Err(e) => {
                    return Err(format!("{}", e));
                }
            };
        };

        Ok(socket)
    }

    async fn websocket_created(self: Arc<ProxyServer>, data: Arc<WebsocketData>, recv: mpsc::Receiver<RawMessage>, send: mpsc::Sender<RawMessage>) -> Result<(), String> {
        let loc = match self.location_manager.find_location(data.distribution_id.clone()).await {
            Some(loc) => loc,
            None => {
                error!("Couldn't find proxy location");
                return Err("Couldn't find proxy location".to_string())
            }
        };

        if loc.secure {
            let socket = self.try_get_connection(ProxyLocation::get_secure_connection, &loc, data.deref()).await?;
            info!("Proxying websocket {} to location {}", data.id, loc.address);
            tokio::spawn(self.handler(socket, data, loc, recv, send));
        } else {
            let socket= self.try_get_connection(ProxyLocation::get_plain_connection, &loc, data.deref()).await?;
            info!("Proxying websocket {} to location {}", data.id, loc.address);
            tokio::spawn(self.handler(socket, data, loc, recv, send));
        }


        Ok(())
    }

    async fn handler<S>(
        self: Arc<ProxyServer>,
        socket: S,
        data: Arc<WebsocketData>,
        loc: Arc<ProxyLocation>,
        mut recv: mpsc::Receiver<RawMessage>,
        mut send: mpsc::Sender<RawMessage>
    ) where S: AsyncRead + AsyncWrite + Unpin {

        let (r, w) = tokio::io::split(socket);

        let cml = ControlMessageListener {
            data: data.clone(),
            server: self.clone()
        };
        let send_clone = send.clone();
        let (fut, handle) = abortable(async {
            select! {
                    d = WsConnection::send_loop(w, &mut recv, cml) => {d}
                    d = WsConnection::receive_loop(r, &mut send) => {d}
                }
        });

        let is_moving = Arc::new(AtomicBool::new(false));
        let ws_conn = WsConnection {
            data: data.clone(),
            abort_handle: handle,
            client_sender: send_clone,
            is_moving: is_moving.clone()
        };

        self.distribution
            .entry(data.distribution_id.clone())
            .or_insert_with(|| vec![])
            .push(ws_conn);
        let _ = fut.await;

        info!("Websocket {} disconnected from location {}", data.id, loc.address);
        loc.connection_count.fetch_sub(1, Ordering::Relaxed);
        if !is_moving.load(Ordering::Acquire) {
            if let Occupied(mut entry) = self.distribution.entry(data.distribution_id.clone()) {
                let conns = entry.get_mut();
                conns.iter().position(|v| v.data.id == data.id).map(|ind| conns.remove(ind));
                if conns.len() == 0 {
                    entry.remove();
                }
            }
        }
    }

    async fn websocket_closed(self: Arc<ProxyServer>, _data: Arc<WebsocketData>) {}

    pub async fn move_distribution(self: Arc<ProxyServer>, distribution_id: String) {
        let mut pending_map = self.pending_moves.lock().await;
        if pending_map.contains_key(&distribution_id) || !self.distribution.contains_key(&distribution_id) {
            if pending_map.contains_key(&distribution_id) {
                warn!("Distribution {}: trying to move already pending distribution", distribution_id);
            } else if !self.distribution.contains_key(&distribution_id) {
                warn!("Distribution {}: trying to move non-exist(all connections was closed) distribution", distribution_id);
            }
            return;
        }

        let mut moving_connections = vec![];

        let mut conns = self.distribution.get_mut(&distribution_id).unwrap();
        let mut i = 0;
        while i < conns.len() {
            if conns[i].data.distribution_id == distribution_id {
                let c = conns.remove(i);
                c.is_moving.store(true, Ordering::Release);
                moving_connections.push(c);
            } else {
                i += 1
            }
        }
        drop(conns);

        info!("Distribution {}: moving {} connections", distribution_id, moving_connections.len());

        let proxy_server = self.clone();
        let d_id = distribution_id.clone();
        let (fut, handle) = abortable(async move {
            delay_for(Duration::from_secs(20)).await;

            warn!("Distribution {}: move timeout! {} connections left",
                  d_id,
                  proxy_server.pending_moves.lock().await.get(&d_id)
                      .map(|v| v.pending_count).unwrap_or(0)
            );
            proxy_server.pending_done(&d_id).await;
        });
        for conn in moving_connections.iter_mut() {
            conn.client_sender.send(RawMessage {
                fin: true,
                opcode: MessageOpCode::TextFrame,
                mask: false,
                mask_key: [0u8; 4],
                payload: self.move_start_message.clone().into_bytes()
            }).await.unwrap();
        }

        let pm = PendingMove {
            pending_count: moving_connections.len() as u32,
            old_connections: moving_connections,
            timeout_abort: handle
        };
        pending_map.insert(distribution_id, pm);
        drop(pending_map);

        tokio::spawn(fut);
    }

    async fn pending_done(&self, distribution_id: &str) {
        if let Some(d) = self.distribution.get(distribution_id) {
            for conn in d.value() {
                conn.client_sender.clone()
                    .send(RawMessage::text_message(self.move_end_message.clone().into_bytes()))
                    .await.err().map(|e| error!("Couldn't send 'WSPROXY_MOVE_END' message because of: {:?}", e));
            }
        }

        // If we drop old connection too fast after
        // WSPROXY_MOVE_END message they couldn't not have time to move.
        delay_for(Duration::from_secs(1)).await;

        if let Some(pm) = self.pending_moves.lock().await.remove(distribution_id) {
            pm.old_connections.iter().for_each(|c| c.abort_handle.abort());
            pm.timeout_abort.abort();
        }
    }

    pub fn get_channel(self: Arc<ProxyServer>) -> Box<ProxyServerChannel> {
        Box::new(ProxyServerChannel {
            server: self.clone()
        })
    }

}

pub struct ProxyServerBuilder {
    lm: Option<Arc<LocationManager>>,

    move_start_message: String,
    move_done_message: String,
    move_end_message: String
}

impl ProxyServerBuilder {
    pub fn new() -> Self {
        Self {
            lm: None,
            move_start_message: "".to_string(),
            move_done_message: "".to_string(),
            move_end_message: "".to_string()
        }
    }
    
    pub fn location_manager(mut self, lm: Arc<LocationManager>) -> Self {
        self.lm = Some(lm);
        self
    }
    
    pub fn move_start_message(mut self, s: String) -> Self {
        self.move_start_message = s;
        self
    }
    pub fn move_done_message(mut self, s: String) -> Self {
        self.move_done_message = s;
        self
    }
    pub fn move_end_message(mut self, s: String) -> Self {
        self.move_end_message = s;
        self
    }
    
    pub fn build(self) -> Arc<ProxyServer> {
        Arc::new(ProxyServer {
            location_manager: self.lm.expect("You should set location_manager"),
            move_start_message: self.move_start_message,
            move_done_message: self.move_done_message,
            move_end_message: self.move_end_message,
            distribution: DashMap::new(),
            pending_moves: Mutex::new(HashMap::new())
        })
    }
}

pub struct ProxyServerChannel {
    server: Arc<ProxyServer>
}

impl ServerChannel for ProxyServerChannel {
    fn websocket_created(&self, data: Arc<WebsocketData>, recv: mpsc::Receiver<RawMessage>, send: mpsc::Sender<RawMessage>) -> Pin<Box<dyn Future<Output=Result<(), String>> + Send>> {
        let s = self.server.clone();
        Box::pin(async move {
            s.websocket_created(data, recv, send).await
        })
    }

    fn websocket_removed(&self, data: Arc<WebsocketData>) -> Pin<Box<dyn Future<Output=()> + Send>> {
        let s = self.server.clone();
        Box::pin(async move {
            s.websocket_closed(data).await;
        })
    }
}

pub fn get_size<T>(_: &T) -> usize {
    return std::mem::size_of::<T>();
}