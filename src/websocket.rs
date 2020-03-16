extern crate memchr;
use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, Ipv4Addr, Shutdown};
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, AsyncBufReadExt};
use std::sync::Arc;
use std::collections::HashMap;
use memchr::memchr;
use crypto::sha1::Sha1;
use crypto::digest::Digest;
use thiserror::Error;
use tokio::select;
use tokio::sync::{mpsc, Mutex};
use tokio::net::tcp::{ReadHalf, WriteHalf};
use futures::future::{abortable, AbortHandle};
use std::fmt::{Debug, Formatter};
use bytes::{BufMut, Buf};
use crate::ServerChannel;

pub static MAX_MESSAGE_SIZE: u64 = 1024 * 1024; // 1 MB

trait MessageAdapter {
    type Message;
    fn decode(msg: RawMessage) -> Self::Message;

    fn encode(msg: Self::Message) -> RawMessage;
}

#[derive(Error, Debug)]
enum HandshakeError {
    #[error("Bad request")]
    BadRequest,
    #[error("Headers parsing error: {0:?}")]
    HeadersParsing(#[from] httparse::Error),
    #[error("Handshake request was canceled")]
    SocketClosed,
    #[error("Missed header \"{0}\" in handshake request")]
    MissedHeader(String),
    #[error("Header \"{0}\" has bad value \"{1}\"")]
    BadHeaderValue(String, String),
    #[error("IO error happened: {0:?}")]
    IOError(#[from] std::io::Error),
}


#[derive(Debug, Clone)]
pub enum MessageOpCode {
    ContinuationFrame = 0,
    TextFrame = 1,
    BinaryFrame = 2,
    Close = 8,
    Ping = 9,
    Pong = 10,
    Unknown = 15
}

impl From<u8> for MessageOpCode {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::ContinuationFrame,
            1 => Self::TextFrame,
            2 => Self::BinaryFrame,
            8 => Self::Close,
            9 => Self::Ping,
            10 => Self::Pong,
            _ => Self::Unknown
        }
    }
}
#[derive(Debug)]
pub struct RawMessage {
    pub fin: bool,
    pub opcode: MessageOpCode,
    pub mask: bool,
    pub mask_key: [u8; 4],
    pub payload: Vec<u8>
}

#[derive(Default)]
pub struct WebsocketData {
    pub id: u128,
    pub path: String,
    /// Header name(key) is lowercase
    pub headers: HashMap<String, String>,
    pub query_params: HashMap<String, String>
}

impl Debug for WebsocketData {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("")
            .field("id", &format!("{:X}", self.id))
            .field("path", &self.path)
            .field("headers", &self.headers)
            .field("query_params", &self.query_params);

        return Ok(())
    }
}


struct WebsocketConnection {
    data: Arc<WebsocketData>,

    handle: AbortHandle,
}

impl WebsocketConnection {
    async fn receive_loop(_data: Arc<WebsocketData>, read_half: ReadHalf<'_>, mut sink: mpsc::Sender<RawMessage>) {
        let mut r = BufReader::new(read_half);
        loop {
            let msg = match crate::websocket_util::receive_message(&mut r).await {
                Ok(msg) => {
                    println!("msg: {:?}", msg);
                    msg
                },
                Err(_) => {
                    println!("failed");
                    return;
                }
            };
            if let MessageOpCode::Close = msg.opcode {
                return;
            } else {
                sink.send(msg).await.unwrap();
                println!("sended msg");
            }
        }
    }

    async fn send_loop(_data: Arc<WebsocketData>, mut write_half: WriteHalf<'_>, mut source: mpsc::Receiver<RawMessage>) {
        loop {
            let v = source.recv().await;
            if let Some(mut msg) = v {
                crate::websocket_util::send_message(msg, &mut write_half).await;
            } else {
                return;
            }
        }
    }
}

pub struct WebsocketServerInner {
    // _received: AtomicU64,
    addr: SocketAddr,
    connections: Mutex<Vec<WebsocketConnection>>,
    channel: Box<dyn ServerChannel + Send + Sync>
}

impl WebsocketServerInner {
    async fn handler(self: Arc<WebsocketServerInner>, mut socket: TcpStream) {
        let mut d = self.handshake(&mut socket).await.unwrap();
        d.id = rand::random();
        let data = Arc::new(d);

        // receive pair
        let (tx, rx) = mpsc::channel::<RawMessage>(5);
        // send pair
        let (tx2, rx2) = mpsc::channel::<RawMessage>(5);

        self.channel.websocket_created(data.clone(), rx, tx2).await;
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
            self.connections.lock().await.push(conn);
            // join!(recv_fut, send_fut);
            fut.await;
            println!("Kappa!");
            let mut arr = self.connections.lock().await;
            arr.iter().position(|v| v.data.id == data.id).map(|v| arr.remove(v));
        }
        self.channel.websocket_removed(data.clone()).await;
        println!("closed");
        socket.shutdown(Shutdown::Both).unwrap();
    }

    async fn handshake(&self, socket: &mut TcpStream) -> Result<WebsocketData, HandshakeError> {
        let data = self.receive_handshake_data(socket).await?;

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

        socket.write(
            format!("\
            HTTP/1.1 101 Switching Protocols\r\n\
            Upgrade: websocket\r\n\
            Connection: Upgrade\r\n\
            Sec-WebSocket-Accept: {}\r\n\
            \r\n", resp_key).as_bytes()
        ).await?;

        Ok(data)
    }

    async fn receive_handshake_data(&self, socket: &mut TcpStream) -> Result<WebsocketData, HandshakeError> {
        let mut handshake: Vec<u8> = Vec::with_capacity(2048);
        let mut buff = [0; 2048];
        let mut prev_packet_ind: usize = 0;

        // handshake
        loop {
            let n = socket.read(&mut buff).await?;
            handshake.extend_from_slice(&buff[0..n]);
            if handshake[n-2..n] == *b"\r\n" ||
                prev_packet_ind != 0 && handshake[prev_packet_ind-1..prev_packet_ind+1] == *b"\r\n" {
                break;
            }
            prev_packet_ind += n;
            if n == 0 {
                return Err(HandshakeError::SocketClosed);
            }
        }
        let mut headers = [httparse::EMPTY_HEADER; 30];

        let mut req = httparse::Request::new(&mut headers);
        req.parse(handshake.as_slice())?;

        let mut res = WebsocketData::default();
        // TODO: URLdecode
        let path = req.path.ok_or(HandshakeError::BadRequest)?;
        let q_index = memchr(b'?', path.as_bytes()).unwrap_or_else(|| path.len());
        res.path = path[0..q_index].to_string();

        // query string processing
        if q_index < path.len() {
            for pair in path[q_index + 1..path.len()].split('&') {
                match memchr(b'=', pair.as_bytes()) {
                    Some(ind) => {
                        res.query_params.insert(
                            pair[0..ind].to_string(),
                            pair[ind + 1..pair.len()].to_string()
                        )
                    },
                    None => {
                        res.query_params.insert(
                            pair.to_string(),
                            "".to_string()
                        )
                    }
                };
            }
        }

        // header processing
        for h in headers.iter() {
            if *h == httparse::EMPTY_HEADER {
                break;
            }

            let v = String::from_utf8(h.value.to_vec()).map_err(|_| HandshakeError::BadRequest)?;
            res.headers.insert(h.name.to_lowercase(), v);
        }

        Ok(res)
    }

    pub async fn run(self: Arc<WebsocketServerInner>) {
        let serv = self.clone();
        tokio::spawn(async move {
            loop {
                let mut v = String::new();
                let mut reader = BufReader::new(tokio::io::stdin());
                reader.read_line(&mut v).await.unwrap();
                v.remove(v.len()-1);
                let res: Vec<&str> = v.split(' ').collect();
                // println!("Red \"{}\" line!", v);
                match res[0] {
                    "p" => {
                        let arr = self.connections.lock().await;
                        if arr.len() == 0 {
                            println!("There are no connections!");
                        } else {
                            println!("Connections:")
                        }
                        for conn in arr.iter().take(10) {
                            println!("  {:?}", conn.data);
                        }
                        if arr.len() > 10 {
                            println!("And {} more...", arr.len() - 10);
                        }
                    },
                    "close" => {
                        if res.len() == 2 {
                            let arr = self.connections.lock().await;
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
        let mut l = TcpListener::bind(serv.addr).await.unwrap();
        loop {
            let (sock, _addr) = l.accept().await.unwrap();
            // serv.channel.websocket_created()
            tokio::spawn(WebsocketServerInner::handler(serv.clone(), sock));
        }
    }
}

pub struct WebsocketServerBuilder {
    addr: SocketAddr,
    channel: Option<Box<dyn ServerChannel + Send + Sync>>
    // state: Arc<WebsocketServerInner>
}

impl Default for WebsocketServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl WebsocketServerBuilder {

    pub fn new() -> Self {
        WebsocketServerBuilder {
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080)),
            channel: None
            // state: Arc::new(WebsocketServerInner {
            //     // _received: AtomicU64::new(0),
            //     connections: Mutex::new(Vec::new())
            // })
        }
    }

    pub fn address<T: ToSocketAddrs>(mut self, addr: T) -> Self {
        self.addr = addr.to_socket_addrs().expect("Failed converting socket address!!!").next().unwrap();
        self
    }

    pub fn channel(mut self, c: Box<dyn ServerChannel + Send + Sync>) -> Self {
        self.channel = Some(c);
        self
    }

    pub fn build(self) -> Arc<WebsocketServerInner> {
        Arc::new(WebsocketServerInner {
            connections: Mutex::new(Vec::new()),
            channel: self.channel.unwrap(),
            addr: self.addr
        })
    }


}