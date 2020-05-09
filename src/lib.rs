use crate::websocket::WebsocketData;
use std::sync::Arc;
use tokio::sync::mpsc;
use futures::Future;
use std::pin::Pin;
use crate::message::RawMessage;
use tokio_tungstenite::WebSocketStream;
use tokio::net::TcpStream;
use tungstenite::{Message, Error as WsError};
use futures::stream::Stream;
use futures::sink::Sink;

#[macro_use] extern crate log;
#[macro_use] extern crate derive_builder;

pub mod websocket;
pub mod proxy;
pub mod websocket_util;
pub mod location_manager;
pub mod message;
pub fn kappa() -> u64 {
    1337*1488
}

pub trait ServerChannel {
    fn websocket_created<S, W>(
        &self,
        data: Arc<WebsocketData>,
        stream: S,
        sink: W) -> Pin<Box<dyn Future<Output=Result<(), WsError>> + Send>>
        where S: Stream<Item=Result<Message, WsError>>, W: Sink<Message>;
    fn websocket_removed(&self, data: Arc<WebsocketData>) -> Pin<Box<dyn Future<Output=()> + Send>>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
