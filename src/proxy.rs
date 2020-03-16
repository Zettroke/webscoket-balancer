use std::sync::Arc;
use crate::ServerChannel;
use tokio::sync::mpsc::{Sender, Receiver};
use futures::Future;
use crate::websocket::{RawMessage, WebsocketData, MessageOpCode};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use futures::future::AbortHandle;
use tokio::time::{delay_for, Duration};
use futures::future::abortable;
use tokio::select;
use std::pin::Pin;

pub struct WsConnection {
    data: Arc<WebsocketData>,
    abort_handle: AbortHandle
}

impl WsConnection {
    async fn send_loop(mut recv: mpsc::Receiver<RawMessage>) {
        println!("send_loop");
        loop {
            match recv.recv().await {
                Some(msg) => {
                    println!("Proxying msg: {:?}", msg);
                },
                None => {
                    println!("Stopped");
                    return;
                }
            }
        }
    }

    async fn receive_loop(mut send: mpsc::Sender<RawMessage>) {
        loop {
            delay_for(Duration::from_secs(5)).await;
            send.send(RawMessage {
                fin: true,
                mask: false,
                mask_key: [0u8; 4],
                opcode: MessageOpCode::TextFrame,
                payload: vec![b'H', b'I', b' ', b'T', b'H', b'E', b'R', b'E']
            }).await;
        }
    }
}

pub struct ProxyServerConfig {
    address: String
}

pub struct ProxyServer {
    pub configs: Mutex<Vec<ProxyServerConfig>>,
    pub connections: Mutex<Vec<WsConnection>>
}

impl ProxyServer {
    async fn websocket_created(self: Arc<ProxyServer>, data: Arc<WebsocketData>, recv: Receiver<RawMessage>, send: Sender<RawMessage>) {
        tokio::spawn(async move {
            println!("ProxyServer websocket_created");
            let fut = abortable(async move {
                select! {
                    _ = WsConnection::send_loop(recv) => {}
                    _ = WsConnection::receive_loop(send) => {}
                };
            });
            self.connections.lock().await.push(WsConnection {
                data: data.clone(),
                abort_handle: fut.1
            });
            fut.0.await;
            println!("Proxy closed");
            let mut arr = self.connections.lock().await;
            arr.iter().position(|v| v.data.id == data.id).map(|v| arr.remove(v));
            println!("Ended");
        });
    }

    async fn websocket_closed(self: Arc<ProxyServer>, data: Arc<WebsocketData>) {
        println!("websocket_closed");
        let arr = self.connections.lock().await;
        arr.iter().find(|v| v.data.id == data.id).map(|v| {
            v.abort_handle.abort();
            // arr.remove(ind)
        });
    }

    pub fn get_channel(self: Arc<ProxyServer>) -> Box<ProxyServerChannel> {
        Box::new(ProxyServerChannel {
            server: self.clone()
        })
    }

}


pub struct ProxyServerChannel {
    server: Arc<ProxyServer>
}

impl ServerChannel for ProxyServerChannel {
    fn websocket_created(&self, data: Arc<WebsocketData>, recv: Receiver<RawMessage>, send: Sender<RawMessage>) -> Pin<Box<dyn Future<Output=()> + Send>> {
        let s = self.server.clone();
        Box::pin(async move {
            s.websocket_created(data, recv, send).await;
        })
    }

    fn websocket_removed(&self, data: Arc<WebsocketData>) -> Pin<Box<dyn Future<Output=()> + Send>> {
        let s = self.server.clone();
        Box::pin(async move {
            s.websocket_closed(data).await;
        })
    }
}