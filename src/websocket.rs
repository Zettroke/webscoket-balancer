use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, Ipv4Addr};
use tokio::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::{RwLock, RwLockReadGuard};
use futures::future::{abortable, AbortHandle};
use std::fmt::{Debug, Formatter};
use crate::ServerChannel;
use http::{Request, Response, HeaderMap, Uri};

pub static MAX_MESSAGE_SIZE: u64 = 1024 * 1024; // 1 MB

#[derive(Default, Clone)]
pub struct WebsocketData {
    pub id: String,
    pub uri: Uri,
    pub distribution_id: String,
    /// Header name(key) is lowercase
    pub headers: HeaderMap,
}

impl Debug for WebsocketData {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("")
            .field("id", &self.id)
            .field("distribution_id", &self.distribution_id)
            .field("path", &self.uri)
            .finish().unwrap();

        return Ok(())
    }
}

pub struct WebsocketConnection {
    pub data: Arc<WebsocketData>,
    pub handle: AbortHandle,
}

pub struct WebsocketServer {
    addr: SocketAddr,
    connections: RwLock<Vec<WebsocketConnection>>,
    channel: Box<dyn ServerChannel + Send + Sync>,
    id_fn: WSfn,
    dist_fn: WSfn
}

impl WebsocketServer {
    async fn handler(self: Arc<WebsocketServer>, socket: TcpStream) {
        let mut data = WebsocketData::default();
        let s = tokio_tungstenite::accept_hdr_async(
            socket,
            |req: &Request<()>, mut resp: Response<()>| {
                let mut query_map = HashMap::new();
                data.uri = req.uri().clone();
                data.headers = req.headers().clone();
                if let Some(q) = req.uri().query() {
                    for param in q.split('&') {
                        if let Some(ind) = param.find('=') {
                            query_map.insert(
                                &param[0..ind],
                                &param[ind+1..param.len()]
                            );
                        } else {
                            query_map.insert(param, "");
                        }
                        param.split('=');
                    }
                }
                data.id = (self.id_fn)(&data.headers, &query_map);
                data.distribution_id = (self.dist_fn)(&data.headers, &query_map);
                if req.headers().contains_key("sec-websocket-protocol") {
                    resp.headers_mut().insert("Sec-WebSocket-Protocol", req.headers().get("sec-websocket-protocol").unwrap().clone());
                }
                Ok(resp)
            }
        ).await.unwrap();
        let data = Arc::new(data);
        let (fut, handle) = abortable(self.channel.websocket_created(data.clone(), s));
        let conn = WebsocketConnection {
            data: data.clone(),
            handle,
        };
        self.connections.write().await.push(conn);

        match fut.await {
            Ok(Err(e)) => {
                error!("{:?}", e);
            },
            _ => {}
        }

        let mut arr = self.connections.write().await;
        arr.iter()
            .position(|v| v.data.id == data.id)
            .map(|v| arr.remove(v));
    }

    pub async fn run(self: Arc<WebsocketServer>) {
        let serv = self.clone();

        let (fut, _handle) = abortable(async move {
            let mut l = TcpListener::bind(serv.addr).await.unwrap();
            loop {
                let (sock, _addr) = l.accept().await.unwrap();
                let task  = WebsocketServer::handler(serv.clone(), sock);
                debug!("server future size: {}", std::mem::size_of_val(&task));
                tokio::spawn(task);
            }
        });
        let _ = fut.await;

    }

    pub async fn get_connections(&self) -> RwLockReadGuard<'_, Vec<WebsocketConnection>> {
        self.connections.read().await
    }
}

type WSfn = fn(&HeaderMap, &HashMap<&str, &str>) -> String;
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

    pub fn build(self) -> Arc<WebsocketServer> {
        Arc::new(WebsocketServer {
            connections: RwLock::new(Vec::new()),
            channel: self.channel.unwrap(),
            addr: self.addr,
            id_fn: self.id_fn,
            dist_fn: self.dist_fn
        })
    }
}