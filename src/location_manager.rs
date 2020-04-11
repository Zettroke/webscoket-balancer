use tokio::sync::RwLock;
use std::sync::Arc;
use crate::websocket::WebsocketData;
use tokio::net::TcpStream;
use bytes::{BufMut, Buf};
use tokio::io::{AsyncWriteExt, AsyncRead, AsyncWrite};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU32, Ordering, AtomicBool};
use tokio::sync::broadcast;
use futures::task::{Context, Poll};
use tokio::macros::support::Pin;
use std::mem::MaybeUninit;
use crate::websocket_util::HandshakeError;
use tokio_tls::TlsStream;
use dashmap::mapref::one::Ref;

#[derive(Debug, Builder)]
pub struct ProxyLocation {
    #[builder(setter(skip))]
    #[builder(default)]
    pub id: u64,
    pub address: String,
    #[builder(setter(skip))]
    #[builder(default)]
    pub connection_count: AtomicU32,
    #[builder(setter(skip))]
    #[builder(default)]
    pub dead: AtomicBool,
    #[builder(setter(strip_option))]
    #[builder(default)]
    pub domain: Option<String>,
    #[builder(default)]
    pub secure: bool
}

impl Default for ProxyLocation {
    fn default() -> Self {
        ProxyLocation {
            id: rand::random(),
            address: "127.0.0.1:80".to_string(),
            connection_count: AtomicU32::new(0),
            dead: AtomicBool::new(false),
            domain: None,
            secure: false
        }
    }
}


pub struct SocketWrapper<T: AsyncRead + AsyncWrite + Unpin> {
    loc: Arc<ProxyLocation>,
    inner: T
}
impl<T: AsyncRead + AsyncWrite + Unpin> Deref for SocketWrapper<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<T: AsyncRead + AsyncWrite + Unpin> DerefMut for SocketWrapper<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
impl<T: AsyncRead + AsyncWrite + Unpin> Drop for SocketWrapper<T> {
    fn drop(&mut self) {
        self.loc.connection_count.fetch_sub(1, Ordering::Relaxed);
    }
}

impl<T: tokio::io::AsyncRead + AsyncWrite + Unpin> AsyncRead for SocketWrapper<T> {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [MaybeUninit<u8>]) -> bool {
        self.inner.prepare_uninitialized_buffer(buf)
    }

    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }

    fn poll_read_buf<B: BufMut>(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut B) -> Poll<std::io::Result<usize>> where
        Self: Sized, {
        Pin::new(&mut self.inner).poll_read_buf(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for SocketWrapper<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_buf<B: Buf>(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut B) -> Poll<std::io::Result<usize>> where
        Self: Sized, {
        Pin::new(&mut self.inner).poll_write_buf(cx, buf)
    }
}


impl ProxyLocation {
    pub fn new() -> ProxyLocationBuilder {
        ProxyLocationBuilder::default()
    }

    async fn send_handshake<T: AsyncWrite + Unpin>(&self, socket: &mut T, data: &WebsocketData) -> Result<(), HandshakeError> {
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
                body.put_slice(self.domain.as_ref().unwrap_or(&self.address).as_bytes());
                body.put_slice(b"\r\n");
            } else if k != "sec-websocket-extensions" {
                body.put_slice(k.as_bytes());
                body.put_slice(b": ");
                body.put_slice(v.as_bytes());
                body.put_slice(b"\r\n");
            }
        }
        body.put_slice(b"\r\n");
        socket.write_all(body.as_ref()).await?;
        Ok(())
    }

    pub async fn get_plain_connection(self: &ProxyLocation, data: &WebsocketData) -> Result<TcpStream, HandshakeError> {
        let socket = TcpStream::connect(self.address.clone()).await?;

        self.connect(socket, data).await
    }

    pub async fn get_secure_connection(self: &ProxyLocation, data: &WebsocketData) -> Result<TlsStream<TcpStream>, HandshakeError> {
        let socket = TcpStream::connect(self.address.as_str()).await?;
        let cx = native_tls::TlsConnector::builder().build()?;
        let cx = tokio_tls::TlsConnector::from(cx);

        let tls_socket = cx.connect(self.domain.as_ref().map(|s| s.as_str()).unwrap_or(""), socket).await?;

        self.connect(tls_socket, data).await
    }

    async fn connect<T: AsyncRead + AsyncWrite + Unpin>(self:&ProxyLocation, mut socket: T, data: &WebsocketData) -> Result<T, HandshakeError> {
        self.send_handshake(&mut socket, data).await?;

        let _v = crate::websocket_util::read_headers(&mut socket).await?;

        self.connection_count.fetch_add(1, Ordering::Release);
        Ok(socket)
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

    pub async fn move_distribution(&self, d_id: String) -> Option<()> {
        let loc = self.find_best_location().await?;
        self.distributions.entry(d_id.clone()).and_modify(|l| {
            std::mem::replace(l, loc.clone());
        });

        self.channel.send(
            LocationManagerMessage::MoveDistribution {
                distribution_id: d_id,
                new_loc: loc
            }
        ).unwrap_or(0);
        Some(())
    }

    pub(crate) async fn find_location(&self, distribution_id: String) -> Option<Arc<ProxyLocation>> {
        let entry = self.distributions.entry(distribution_id);
        let loc = match entry {
            Entry::Occupied(e) => {
                e.get().clone()
            },
            Entry::Vacant(e) => {
                let loc = self.find_best_location().await?;
                e.insert(loc.clone());
                loc
            }
        };

        Some(loc)
    }

    pub fn get_location_ref(&self, distribution_id: &str) -> Option<Ref<String, Arc<ProxyLocation>>> {
        self.distributions.get(distribution_id)
    }

    pub async fn add_location(&self, mut loc: ProxyLocation) {
        let mut loc_list = self.locations.write().await;
        while loc_list.iter().any(|v| v.id == loc.id) {
            loc.id = rand::random();
        }
        loc_list.push(Arc::new(loc));
    }

    pub fn get_message_recv(&self) -> broadcast::Receiver<LocationManagerMessage> {
        self.channel.subscribe()
    }

    pub async fn mark_dead(&self, loc: &ProxyLocation) {
        loc.dead.store(true, Ordering::Release);

        // remove all distributions linked to dead location
        self.distributions.retain(|_k, v| v.id != loc.id);
    }

    async fn find_best_location(&self) -> Option<Arc<ProxyLocation>> {
        Some(
            self.locations.read().await.iter()
                .filter(|v| !v.dead.load(Ordering::Acquire))
                .min_by_key(|v| v.connection_count.load(Ordering::Acquire))?
                .clone()
        )
    }
}