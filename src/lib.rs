use crate::websocket::{WebsocketData, RawMessage};
use std::sync::Arc;
use tokio::sync::mpsc;
use futures::Future;
use std::pin::Pin;

pub mod websocket;
pub mod proxy;
pub mod websocket_util;
pub mod location_manager;
pub fn kappa() -> u64 {
    1337*1488
}

pub trait ServerChannel {
    fn websocket_created(&self, data: Arc<WebsocketData>, recv: mpsc::Receiver<RawMessage>, send: mpsc::Sender<RawMessage>) -> Pin<Box<dyn Future<Output=()> + Send>>;
    fn websocket_removed(&self, data: Arc<WebsocketData>) -> Pin<Box<dyn Future<Output=()> + Send>>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
