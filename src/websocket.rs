use std::net::{SocketAddr, ToSocketAddrs};
use tokio::net::{TcpListener, TcpStream};
use std::error::Error;
use tokio::io::AsyncReadExt;

pub struct WebsocketServer {
    addr: SocketAddr
}


impl WebsocketServer {
    pub fn address<T: ToSocketAddrs>(&mut self, addr: T) -> &mut Self {
        self.addr = addr.to_socket_addrs().expect("Failed converting socket address!!!").next().unwrap();
        self
    }


    pub async fn run(&mut self) -> Result<(), Box<dyn Error>> {
        let mut l = TcpListener::bind(self.addr).await?;
        loop {
            let (mut sock, addr) = l.accept().await?;

            tokio::spawn(self.handler(sock));
        }
    }

    async fn handler(&mut self, mut socket: TcpStream) {
        let mut buff = [0; 32];
        loop {
            let n = socket.read(&mut buff).await.unwrap();
            println!("Read {} bytes: {:?}", n, buff);
            if n == 0 {
                return;
            }
        }
    }
}