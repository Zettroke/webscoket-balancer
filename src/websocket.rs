extern crate memchr;
use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, Ipv4Addr, Shutdown};
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::sync::atomic::{AtomicU64};
use std::sync::Arc;
use std::collections::HashMap;
use memchr::memchr;
use crypto::sha1::Sha1;
use crypto::digest::Digest;
use thiserror::Error;
use std::ops::BitXorAssign;
use std::u64::MAX;
use crate::websocket::MessageError::MessageTooBig;

static MAX_MESSAGE_SIZE: u64 = 1024 * 1024; // 1 MB

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

#[derive(Error, Debug)]
enum MessageError{
    #[error("Socket error while reading message: {0:?}")]
    Socket(#[from] std::io::Error),
    #[error("Bad message format")]
    Format,
    #[error("Message size: {size} bytes too big. Max size is {max_size}")]
    MessageTooBig { size: u64, max_size: u64 }
}

#[derive(Debug, Clone)]
enum MessageOpCode {
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

struct RawMessage {
    fin: bool,
    opcode: MessageOpCode,
    mask: bool,
    mask_key: [u8; 4],
    payload: Vec<u8>
}

#[derive(Default, Debug)]
struct WebsocketData {
    path: String,
    /// Header name(key) is lowercase
    headers: HashMap<String, String>,
    query_params: HashMap<String, String>
}

struct WebsocketServerInner {
    _received: AtomicU64

}

impl WebsocketServerInner {
    async fn handler(self: Arc<WebsocketServerInner>, mut socket: TcpStream) {
        self.handshake(&mut socket).await.unwrap();
        loop {
            let msg = match self.next_message(&mut socket).await {
                Ok(msg) => msg,
                Err(e) => {
                    eprintln!("Message error: {}\nClosing connection!", e);
                    socket.shutdown(Shutdown::Both).await; // TODO: Correct closing with message sent
                    return;
                },
            };
        }
    }

    async fn next_message(&self, socket: &mut TcpStream) -> Result<RawMessage, MessageError> {
        // read from socket until n bytes will be in buff


        let mut cur: usize = 0;
        let mut fin = false;
        let mut opcode: u8 = 0;
        let mut mask = false;
        let mut mask_key = [0u8; 4];
        let mut buff = [0u8; 16];
        macro_rules! read_until {
            ($n:expr) => {
                while cur < $n {
                    let n = socket.read(&mut buff[cur..16]).await?;
                    if n == 0 { return Err(MessageError::Socket(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "socket was closed!"))) }
                    cur += n;
                }
            };
        }

        read_until!(2);
        fin = buff[0] & 0b1000_0000 != 0;
        let opcode = MessageOpCode::from(buff[0] & 0b0000_1111);
        mask = buff[1] & 0b1000_0000 != 0;
        let short_payload_len = (buff[1] & 0b0111_1111) as u16;
        println!("fin: {}, opcode: {:?} - {}, short_payload_len: {}", fin, opcode, opcode.clone() as u8, short_payload_len);

        let (size_end, payload_len) = match short_payload_len {
            126 => {
                read_until!(4);
                let mut tmp = [0u8; 2];
                tmp.copy_from_slice(&buff[2..4]);

                (4, u16::from_be_bytes(tmp) as u64)
            },
            127 => {
                read_until!(10);
                let mut tmp = [0u8; 8];
                tmp.copy_from_slice(&buff[2..10]);

                (10, u64::from_be_bytes(tmp))
            },
            v => (2, v as u64)
        };

        if payload_len > MAX_MESSAGE_SIZE {
            return Err(MessageTooBig { size: payload_len, max_size: MAX_MESSAGE_SIZE });
        }

        if mask {
            read_until!(size_end + 4);

            mask_key.copy_from_slice(&buff[size_end..size_end+4]);
        }
        let header_end = size_end + 4;

        let mut payload_buff = vec![0u8; payload_len as usize];

        if header_end < cur {
            payload_buff[0..cur-header_end].copy_from_slice(&mut buff[header_end..cur]);
        }

        if cur - header_end < payload_buff.len() {
            socket.read_exact(&mut payload_buff[cur-header_end..payload_len as usize]).await?;
        }

        for b in payload_buff.iter_mut().enumerate() {
            *b.1 ^= mask_key[b.0 % 4];
        }

        println!("payload: {}", String::from_utf8(payload_buff.clone()).unwrap_or(String::from("")));
        return Ok(RawMessage {
            fin,
            mask,
            mask_key,
            opcode,
            payload: payload_buff
        });
    }

    async fn handshake(&self, socket: &mut TcpStream) -> Result<(), HandshakeError> {
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

        println!("Connected!!!!!1");
        Ok(())


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
}

pub struct WebsocketServer {
    addr: SocketAddr,
    state: Arc<WebsocketServerInner>
}

impl Default for WebsocketServer {
    fn default() -> Self {
        Self::new()
    }
}

impl WebsocketServer {

    pub fn new() -> Self {
        WebsocketServer {
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080)),
            state: Arc::new(WebsocketServerInner { _received: AtomicU64::new(0)})
        }
    }

    pub fn address<T: ToSocketAddrs>(&mut self, addr: T) -> &mut Self {
        self.addr = addr.to_socket_addrs().expect("Failed converting socket address!!!").next().unwrap();
        self
    }

    pub async fn run(&mut self) {
        let mut l = TcpListener::bind(self.addr).await.unwrap();
        loop {
            let (sock, _addr) = l.accept().await.unwrap();
            tokio::spawn(WebsocketServerInner::handler(self.state.clone(), sock));
        }
    }
}