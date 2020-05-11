use crate::websocket::WebsocketData;
use std::sync::Arc;
use futures::Future;
use std::pin::Pin;
use tokio_tungstenite::WebSocketStream;
use tokio::net::TcpStream;
use tungstenite::{Message, Error};
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

pub trait WsMessageStream: Stream<Item=Result<Message, Error>> + Send + Unpin {}
impl<T: Stream<Item=Result<Message, Error>> + Send + Unpin> WsMessageStream for T {}

pub trait WsMessageSink: Sink<Message, Error=Error> + Send + Unpin {}
impl<T: Sink<Message, Error=Error> + Send + Unpin> WsMessageSink for T {}

pub trait ServerChannel {
    fn websocket_created(
        &self,
        data: Arc<WebsocketData>,
        ws_stream: WebSocketStream<TcpStream>) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>>;
    fn websocket_removed(&self, data: Arc<WebsocketData>) -> Pin<Box<dyn Future<Output=()> + Send>>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
