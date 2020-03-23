use tokio::sync::RwLock;
use std::sync::Arc;
use crate::websocket::WebsocketData;
use tokio::net::TcpStream;
use bytes::{BufMut, Buf};
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::broadcast;

pub struct ProxyLocation {
    pub address: String,
    pub connection_count: AtomicU32
}

pub struct SocketWrapper {
    loc: Arc<ProxyLocation>,
    inner: TcpStream
}

impl Deref for SocketWrapper {
    type Target = TcpStream;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for SocketWrapper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
impl Drop for SocketWrapper {
    fn drop(&mut self) {
        self.loc.connection_count.fetch_sub(1, Ordering::Relaxed);
    }
}


impl ProxyLocation {
    pub async fn get_connection(self: Arc<ProxyLocation>, data: &WebsocketData) -> Option<SocketWrapper> {
        let mut body = bytes::BytesMut::new();
        body.put_slice(b"GET ");

        body.put_slice(data.path.as_bytes());
        if data.query_params.len() > 0 {
            body.put_u8(b'?');
            for (ind, (k, v)) in data.query_params.iter().enumerate() {
                body.put_slice(k.as_bytes());
                body.put_u8(b'=');
                body.put_slice(v.as_bytes());
                if ind != data.query_params.len() - 1 {
                    body.put_u8(b'&');
                }
            }
        }
        body.put_slice(b" HTTP/1.1\r\n");

        for (k, v) in data.headers.iter() {
            if k == "host" {
                body.put_slice(b"host: ");
                body.put_slice(self.address.as_bytes());
                body.put_slice(b"\r\n");
            } else if k != "sec-websocket-extensions" {
                body.put_slice(k.as_bytes());
                body.put_slice(b": ");
                body.put_slice(v.as_bytes());
                body.put_slice(b"\r\n");
            }
        }
        body.put_slice(b"\r\n");
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

        self.connection_count.fetch_add(1, Ordering::Relaxed);
        Some(SocketWrapper{
            inner: socket,
            loc: self.clone()
        })
    }
}

#[derive(Clone)]
pub enum LocationManagerMessage {
    MoveDistribution { distribution_id: String, new_loc: Arc<ProxyLocation> }
}

pub struct LocationManager {
    channel: broadcast::Sender<LocationManagerMessage>,
    pub locations: RwLock<Vec<Arc<ProxyLocation>>>,
    distributions: DashMap<String, Arc<ProxyLocation>>
}

impl LocationManager {
    pub fn new() -> Self {
        let (send, _) = broadcast::channel(8);
        Self {
            channel: send,
            distributions: DashMap::new(),
            locations: RwLock::new(Vec::new())
        }
    }

    pub async fn move_distribution(&self, d_id: String) {
        let loc = self.find_best_location().await;
        self.distributions.entry(d_id.clone()).and_modify(|l| {
            std::mem::replace(l, loc.clone());
        });

        self.channel.send(
            LocationManagerMessage::MoveDistribution {
                distribution_id: d_id,
                new_loc: loc
            }
        ).unwrap_or(0);
    }

    pub async fn get_location(&self, distribution_id: String) -> Arc<ProxyLocation> {

        let entry = self.distributions.entry(distribution_id);
        let loc = match entry {
            Entry::Occupied(e) => {
                e.get().clone()
            },
            Entry::Vacant(e) => {
                let loc = self.find_best_location().await;
                e.insert(loc.clone());
                loc
            }
        };

        loc
    }

    pub async fn add_location(&self, addr: String) {
        self.locations.write().await.push(Arc::new(ProxyLocation {
            address: addr,
            connection_count: AtomicU32::new(0)
        }))
    }

    pub fn get_message_recv(&self) -> broadcast::Receiver<LocationManagerMessage> {
        self.channel.subscribe()
    }

    async fn find_best_location(&self) -> Arc<ProxyLocation> {
        self.locations.read().await.iter()
            .min_by_key(|v| v.connection_count.load(Ordering::Relaxed))
            .unwrap().clone()
    }
}