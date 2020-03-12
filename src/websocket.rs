use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, Ipv4Addr, Shutdown};
use tokio::net::{TcpListener, TcpStream};
use std::error::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::collections::HashMap;
use std::io::{BufRead, Cursor};
use memchr::memchr;
use std::convert::From;
use std::any::Any;

#[derive(Default, Debug)]
struct WebsocketData {
    path: String,
    headers: HashMap<String, String>,
    query_params: HashMap<String, String>
}

struct WebsocketServerState {
    received: AtomicU64
}

pub struct WebsocketServer {
    addr: SocketAddr,

    state: Arc<WebsocketServerState>
}


impl WebsocketServer {

    pub fn new() -> Self {
        WebsocketServer {
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080)),
            state: Arc::new(WebsocketServerState {received: AtomicU64::new(0)})
        }
    }

    pub fn address<T: ToSocketAddrs>(&mut self, addr: T) -> &mut Self {
        self.addr = addr.to_socket_addrs().expect("Failed converting socket address!!!").next().unwrap();
        self
    }


    pub async fn run(&mut self) -> Result<(), Box<dyn Error>> {

        let mut l = TcpListener::bind(self.addr).await?;
        loop {
            let (mut sock, addr) = l.accept().await?;
            tokio::spawn(Self::handler(self.state.clone(), sock));
        }
    }


    async fn handler(this: Arc<WebsocketServerState>, mut socket: TcpStream) {
        let data = match Self::receive_handshake_data(&mut socket).await {
            Ok(d) => d,
            Err(_) => {
                socket.shutdown(Shutdown::Both);
                return;
            }
        };

        data.headers.get("Connection");

    }

    async fn receive_handshake_data(socket: &mut TcpStream) -> Result<WebsocketData, ()> {
        let mut handshake: Vec<u8> = Vec::with_capacity(1024);
        let mut buff = [0; 1024];
        let mut prev_packet_ind: usize = 0;

        // handshake
        loop {
            let n = socket.read(&mut buff).await.unwrap();
            handshake.extend_from_slice(&buff[0..n]);
            if handshake[n-2..n] == *"\r\n".as_bytes() {
                break;
            } else if prev_packet_ind != 0 {
                if handshake[prev_packet_ind-1..prev_packet_ind+1] == *"\r\n".as_bytes() {
                    break;
                }
            }
            prev_packet_ind += n;
            if n == 0 {
                return Err(());
            }
        }
        let mut headers = [httparse::EMPTY_HEADER; 30];

        let mut req = httparse::Request::new(&mut headers);
        match req.parse(handshake.as_slice()) {
            Ok(httparse::Status::Complete(len)) => {},
            _ => {
                return Err(());
            }
        }

        let mut res = WebsocketData::default();
        // TODO: URLdecode
        let path = req.path.unwrap();
        let q_index = memchr(b'?', path.as_bytes()).unwrap_or(path.len());
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
            match String::from_utf8(h.value.to_vec()) {
                Ok(v) => res.headers.insert(h.name.to_string(), v),
                Err(_) => continue
            };
        }


        socket.write(format!("HTTP/1.1 200\r\n\r\n<pre>{:#?}</pre>\r\n", res).as_bytes()).await;

        Ok(WebsocketData::default())
    }
}