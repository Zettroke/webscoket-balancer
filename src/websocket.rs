extern crate memchr;
use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, Ipv4Addr};
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::sync::atomic::{AtomicU64};
use std::sync::Arc;
use std::collections::HashMap;
use memchr::memchr;
use crypto::sha1::Sha1;
use crypto::digest::Digest;
use thiserror::Error;
// use core::slice::memchr::memchr;

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


#[derive(Default, Debug)]
struct WebsocketData {
    path: String,
    /// Header name(key) is lowercase
    headers: HashMap<String, String>,
    query_params: HashMap<String, String>
}

struct WebsocketServerState {
    _received: AtomicU64
}

pub struct WebsocketServer {
    addr: SocketAddr,
    state: Arc<WebsocketServerState>
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
            state: Arc::new(WebsocketServerState { _received: AtomicU64::new(0)})
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
            tokio::spawn(Self::handler(self.state.clone(), sock));
        }
    }

    async fn handler(_this: Arc<WebsocketServerState>, mut socket: TcpStream) {
        WebsocketServer::handshake(&mut socket).await.unwrap();
    }

    async fn handshake(socket: &mut TcpStream) -> Result<(), HandshakeError> {
        let data = Self::receive_handshake_data(socket).await?;

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

    async fn receive_handshake_data(socket: &mut TcpStream) -> Result<WebsocketData, HandshakeError> {
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