extern crate memchr;
use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, Ipv4Addr, Shutdown};
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncWriteExt, BufReader};
use std::sync::Arc;
use std::collections::HashMap;
use crypto::sha1::Sha1;
use crypto::digest::Digest;
use tokio::select;
use tokio::sync::{mpsc, RwLock, RwLockReadGuard};
use tokio::net::tcp::{ReadHalf, WriteHalf};
use futures::future::{abortable, AbortHandle};
use std::fmt::{Debug, Formatter};
use crate::ServerChannel;
use crate::message::{RawMessage, MessageOpCode};
use crate::websocket_util::{MessageError, HandshakeError};

pub static MAX_MESSAGE_SIZE: u64 = 1024 * 1024; // 1 MB

#[derive(Default)]
pub struct WebsocketData {
    pub id: String,
    pub path: String,
    pub distribution_id: String,
    /// Header name(key) is lowercase
    pub headers: HashMap<String, String>,
    pub query_params: HashMap<String, String>
}

impl Debug for WebsocketData {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("")
            .field("id", &self.id)
            .field("distribution_id", &self.distribution_id)
            .field("path", &self.path)
            .finish().unwrap();

        return Ok(())
    }
}

pub struct WebsocketConnection {
    pub data: Arc<WebsocketData>,
    pub handle: AbortHandle,
}

impl WebsocketConnection {
    async fn receive_loop(_data: Arc<WebsocketData>, read_half: ReadHalf<'_>, mut sink: mpsc::Sender<RawMessage>) -> Result<(), MessageError> {
        let mut r = BufReader::new(read_half);
        loop {
            let msg = crate::websocket_util::receive_message(&mut r).await?;
            if let MessageOpCode::Close = msg.opcode {
                return Ok(());
            } else {
                if let Err(_) = sink.send(msg).await {
                    return Ok(())
                }
            }
        }
    }

    async fn send_loop(_data: Arc<WebsocketData>, mut write_half: WriteHalf<'_>, mut source: mpsc::Receiver<RawMessage>) -> Result<(), std::io::Error> {
        loop {
            let v = source.recv().await;
            if let Some(msg) = v {
                crate::websocket_util::send_message(msg, &mut write_half).await?;
            } else {
                return Ok(());
            }
        }
    }
}

pub struct WebsocketServerInner {
    addr: SocketAddr,
    connections: RwLock<Vec<WebsocketConnection>>,
    channel: Box<dyn ServerChannel + Send + Sync>,
    id_fn: WSfn,
    dist_fn: WSfn
}

impl WebsocketServerInner {
    async fn handler(self: Arc<WebsocketServerInner>, mut socket: TcpStream) {

        //TODO: Разделить handshake, и если websocket_created вернет Err, то возвращать http ответ с ошибкой.
        let data: Arc<WebsocketData> = Arc::new(Box::pin(self.handshake(&mut socket)).await.unwrap());

        // receive pair
        let (tx, rx) = mpsc::channel::<RawMessage>(5);
        // send pair
        let (tx2, rx2) = mpsc::channel::<RawMessage>(5);

        match self.channel.websocket_created(data.clone(), rx, tx2).await {
            Ok(_) => {},
            Err(e) => {
                error!("{:?}", e);
            }
        }
        // if not err { handshake_complete(socket) }
        let (r, w) = socket.split();
        {
            let d = data.clone();
            let (fut, handle) = abortable(async move {
                select! {
                    _ = WebsocketConnection::receive_loop(d.clone(), r, tx) => {}
                    _ = WebsocketConnection::send_loop(d.clone(), w, rx2) => {}
                }
            });
            let conn = WebsocketConnection {
                data: data.clone(),
                handle,
            };
            self.connections.write().await.push(conn);

            match fut.await {
                Err(_) => {
                    match crate::websocket_util::send_message(RawMessage::close_message(), &mut socket).await {
                        Err(e) => {
                            warn!("Can't send close message to client {} err: {:?}", data.id, e);
                        }
                        _ => {}
                    };
                },
                Ok(_) => {}
            }
            // println!("Kappa!");
            let mut arr = self.connections.write().await;
            arr.iter().position(|v| v.data.id == data.id).map(|v| arr.remove(v));
            // info!("Client connection closed");
        }
        socket.shutdown(Shutdown::Both).unwrap();
    }

    async fn handshake(&self, socket: &mut TcpStream) -> Result<WebsocketData, HandshakeError> {
        let mut data = crate::websocket_util::receive_handshake_data(socket).await?;

        // Check connection header
        let conn = data.headers.get("connection").ok_or_else(|| HandshakeError::MissedHeader("connection".to_owned()))?;
        if conn.to_lowercase() != "upgrade" {
            return Err(HandshakeError::BadHeaderValue("connection".to_owned(), conn.to_owned()))
        }
        // Check upgrade header
        let upg = data.headers.get("upgrade").ok_or_else(|| HandshakeError::MissedHeader("upgrade".to_owned()))?;
        if upg.to_lowercase() != "websocket" {
            return Err(HandshakeError::BadHeaderValue("upgrade".to_owned(), upg.to_owned()))
        }

        // Calculating Sec-WebSocket-Accept key
        let key = data.headers.get("sec-websocket-key")
            .ok_or_else(|| HandshakeError::MissedHeader("sec-websocket-key".to_owned()))?;

        let mut hasher = Sha1::new();
        hasher.input_str(key);
        hasher.input_str("258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let mut output = [0u8; 20];
        hasher.result(&mut output);
        let resp_key = base64::encode(output);

        socket.write(b"\
            HTTP/1.1 101 Switching Protocols\r\n\
            Upgrade: websocket\r\n\
            Connection: Upgrade\r\n\
        ").await?;

        socket.write(
            format!("Sec-WebSocket-Accept: {}\r\n", resp_key).as_bytes()
        ).await?;
        if let Some(protocol) = data.headers.get("sec-websocket-protocol") {
            socket.write(
                format!("Sec-WebSocket-Protocol: {}\r\n", protocol).as_bytes()
            ).await?;
        }

        data.id = (self.id_fn)(&data.headers, &data.query_params);
        data.distribution_id = (self.dist_fn)(&data.headers, &data.query_params);

        socket.write(b"\r\n").await?;

        Ok(data)
    }

    pub async fn run(self: Arc<WebsocketServerInner>) {
        let serv = self.clone();

        let (fut, _handle) = abortable(async move {
            let mut l = TcpListener::bind(serv.addr).await.unwrap();
            loop {
                let (sock, _addr) = l.accept().await.unwrap();
                let task  = WebsocketServerInner::handler(serv.clone(), sock);
                // debug!("server future size: {}", get_size(&task));
                tokio::spawn(task);
            }
        });
        let _ = fut.await;

    }

    pub async fn get_connections(&self) -> RwLockReadGuard<'_, Vec<WebsocketConnection>> {
        self.connections.read().await
    }
}

type WSfn = fn(&HashMap<String, String>, &HashMap<String, String>) -> String;
pub struct WebsocketServerBuilder {
    addr: SocketAddr,
    channel: Option<Box<dyn ServerChannel + Send + Sync>>,
    id_fn: WSfn,
    dist_fn: WSfn
}

impl Default for WebsocketServerBuilder {
    fn default() -> Self {
        WebsocketServerBuilder {
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080)),
            channel: None,
            id_fn: |_, _| format!("{:032X}", rand::random::<u128>()),
            dist_fn: |_, _| format!("{:032X}", rand::random::<u128>())
        }
    }
}

impl WebsocketServerBuilder {

    pub fn new() -> Self {
        Self::default()
    }

    pub fn address<T: ToSocketAddrs>(mut self, addr: T) -> Self {
        self.addr = addr.to_socket_addrs().expect("Failed converting socket address!!!").next().unwrap();
        self
    }

    pub fn channel(mut self, c: Box<dyn ServerChannel + Send + Sync>) -> Self {
        self.channel = Some(c);
        self
    }

    pub fn id_fn(mut self, f: WSfn) -> Self {
        self.id_fn = f;
        self
    }

    pub fn dist_fn(mut self, f: WSfn) -> Self {
        self.dist_fn = f;
        self
    }

    pub fn build(self) -> Arc<WebsocketServerInner> {
        Arc::new(WebsocketServerInner {
            connections: RwLock::new(Vec::new()),
            channel: self.channel.unwrap(),
            addr: self.addr,
            id_fn: self.id_fn,
            dist_fn: self.dist_fn
        })
    }
}