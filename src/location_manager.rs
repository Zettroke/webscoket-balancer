use tokio::sync::RwLock;
use std::sync::Arc;
use crate::websocket::WebsocketData;
use tokio::net::TcpStream;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use std::sync::atomic::{AtomicU32, Ordering, AtomicBool};
use tokio::sync::broadcast;
use crate::websocket_util::HandshakeError;
use tokio_tls::TlsStream;
use dashmap::mapref::one::Ref;
use http::Uri;
use std::str::FromStr;
use http::uri::Authority;

#[derive(Debug, Builder)]
pub struct ProxyLocation {
    #[builder(setter(skip))]
    #[builder(default)]
    pub id: u64,
    pub uri: Uri,
    #[builder(setter(skip))]
    #[builder(default)]
    pub connection_count: AtomicU32,
    #[builder(setter(skip))]
    #[builder(default)]
    pub dead: AtomicBool,
    #[builder(default)]
    pub secure: bool
}

impl Default for ProxyLocation {
    fn default() -> Self {
        ProxyLocation {
            id: rand::random(),
            uri: Uri::from_str("ws://127.0.0.1:80").unwrap(),
            connection_count: AtomicU32::new(0),
            dead: AtomicBool::new(false),
            secure: false
        }
    }
}

impl ProxyLocation {
    pub fn new() -> ProxyLocationBuilder {
        ProxyLocationBuilder::default()
    }

    pub async fn get_plain_connection(self: &ProxyLocation, _data: &WebsocketData) -> Result<TcpStream, HandshakeError> {
        let socket = TcpStream::connect(self.uri.authority().map(|a| a.as_str()).unwrap_or("")).await?;

        self.connection_count.fetch_add(1, Ordering::Release);

        Ok(socket)
    }

    pub async fn get_secure_connection(self: &ProxyLocation, _data: &WebsocketData) -> Result<TlsStream<TcpStream>, HandshakeError> {
        let socket = TcpStream::connect(self.uri.authority().unwrap().as_str()).await?;
        let cx = native_tls::TlsConnector::builder().build()?;
        let cx = tokio_tls::TlsConnector::from(cx);

        let tls_socket = cx.connect(self.uri.host().unwrap_or(""), socket).await?;
        self.connection_count.fetch_add(1, Ordering::Release);
        Ok(tls_socket)
    }
}

#[derive(Clone)]
pub enum LocationManagerMessage {
    MoveDistribution { distribution_id: String, new_loc: Arc<ProxyLocation> }
}

pub struct LocationManager {
    channel: broadcast::Sender<LocationManagerMessage>,
    pub locations: RwLock<Vec<Arc<ProxyLocation>>>,
    pub distributions: DashMap<String, Arc<ProxyLocation>>
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
        loop {
            loc.id = rand::random();
            if !loc_list.iter().any(|v| v.id == loc.id) { break; }
        }
        if loc.uri.port().is_none() && loc.uri.scheme_str() == Some("wss") {
            let mut parts = loc.uri.clone().into_parts();
            let auth = parts.authority.as_ref().unwrap().as_str().to_string() + ":443";
            parts.authority.replace(Authority::from_maybe_shared(auth).unwrap());
            loc.uri = Uri::from_parts(parts).unwrap();
        }
        println!("{}", loc.uri.to_string());
        loc_list.push(Arc::new(loc));
    }

    pub fn get_message_recv(&self) -> broadcast::Receiver<LocationManagerMessage> {
        self.channel.subscribe()
    }

    pub async fn mark_dead(&self, loc: &ProxyLocation) {
        loc.dead.store(true, Ordering::Release);
        error!("Location {} is dead!", loc.uri);

        // remove all distributions linked to the dead location
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