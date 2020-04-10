use crate::websocket::{MAX_MESSAGE_SIZE, WebsocketData};
use tokio::io::{AsyncWriteExt, AsyncReadExt, AsyncWrite, AsyncRead};
use bytes::{BufMut, Buf};
use thiserror::Error;
use crate::message::{RawMessage, MessageOpCode};
use futures::io::ErrorKind;
extern crate memchr;
use memchr::memchr;

#[derive(Error, Debug)]
pub enum MessageError{
    #[error("Socket error while reading message: {0:?}")]
    Socket(#[from] std::io::Error),
    #[error("Bad message format")]
    Format,
    #[error("Message size: {size} bytes too big. Max size is {max_size}")]
    MessageTooBig { size: u64, max_size: u64 },
}

#[derive(Error, Debug)]
pub enum HandshakeError {
    #[error("Bad request")]
    BadRequest,
    #[error("Headers parsing error: {0:?}")]
    HeadersParsing(#[from] httparse::Error),
    #[error("Handshake request was canceled")]
    SocketClosed,
    #[error("Missed header \"{0}\" in handshake request")]
    MissedHeader(String),
    #[error("Header \"{0}\" has bad value \"{1}\"")]
    BadHeaderValue(String, String),
    #[error("IO error happened: {0:?}")]
    IOError(#[from] std::io::Error),
    #[error("TLS error: {0:?}")]
    TlsError(#[from] native_tls::Error)
}

pub async fn receive_message<T: AsyncReadExt + Unpin>(reader: &mut T) -> Result<RawMessage, MessageError> {
    let mut mask_key = [0u8; 4];
    let v = reader.read_u16().await?;

    let fin = v & (1 << 15) != 0;

    let opcode_v = ((v & (0b0000_1111 << 8)) >> 8) as u8;
    let opcode = MessageOpCode::from(opcode_v);

    let mask = v & 0b1000_0000 != 0;

    let short_payload_len = (v & 0b0111_1111) as u8;

    let payload_len = match short_payload_len {
        126 => {
            reader.read_u16().await? as u64
        },
        127 => {
            reader.read_u64().await?
        },
        v => v as u64
    };
    if payload_len > MAX_MESSAGE_SIZE {
        return Err(MessageError::MessageTooBig { size: payload_len, max_size: MAX_MESSAGE_SIZE });
    }

    if mask {
        reader.read(&mut mask_key).await?;
    }

    let mut payload_buff = vec![0u8; payload_len as usize];
    if payload_len != 0 {
        reader.read(payload_buff.as_mut_slice()).await?;
    }

    return Ok(RawMessage {
        fin,
        mask,
        mask_key,
        opcode,
        payload: payload_buff
    });
}

pub async fn send_message<T: AsyncWrite + Unpin>(msg: RawMessage, writer: &mut T) -> Result<(), std::io::Error> {
    let mut out = bytes::BytesMut::new();
    out.put_u8(((msg.fin as u8) << 7) | (msg.opcode as u8));

    let mask_v = (msg.mask as u8) << 7;
    if msg.payload.len() <= 125 {
        out.put_u8(mask_v | msg.payload.len() as u8);
    } else if msg.payload.len() < u16::max_value() as usize {
        out.put_u8(mask_v | 126);
        out.put_u16(msg.payload.len() as u16);
    } else {
        out.put_u8(mask_v | 127);
        out.put_u64(msg.payload.len() as u64);
    }
    if msg.mask {
        out.put_slice(&msg.mask_key);
    }
    out.put_slice(msg.payload.as_slice());
    writer.write(out.bytes()).await?;
    Ok(())
}

pub async fn receive_handshake_data<T: AsyncRead + Unpin>(socket: &mut T) -> Result<WebsocketData, HandshakeError> {
    let mut handshake: Vec<u8> = read_headers(socket).await?;
    let mut headers = [httparse::EMPTY_HEADER; 30];

    let mut req = httparse::Request::new(&mut headers);
    req.parse(handshake.as_slice())?;

    let mut res = WebsocketData::default();
    // TODO: URLdecode
    let path = req.path.ok_or(HandshakeError::BadRequest)?;
    let q_index = memchr(b'?', path.as_bytes()).unwrap_or_else(|| path.len());
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
        if *h == httparse::EMPTY_HEADER {
            break;
        }

        let v = String::from_utf8(h.value.to_vec()).map_err(|_| HandshakeError::BadRequest)?;
        res.headers.insert(h.name.to_lowercase(), v);
    }
    Ok(res)
}


pub async fn read_headers<T: AsyncRead + Unpin>(socket: &mut T) -> Result<Vec<u8>, HandshakeError> {
    let mut handshake: Vec<u8> = Vec::with_capacity(2048);
    let mut buff = vec![0; 2048].into_boxed_slice();
    let mut prev_packet_ind: usize = 0;
    loop {
        let n = socket.read(&mut buff).await?;

        handshake.extend_from_slice(&buff[0..n]);

        let mut ind = if prev_packet_ind == 0 {
            0
        } else {
            if prev_packet_ind > 3 { prev_packet_ind-3 } else { 0 }
        };
        let mut done = false;
        loop {
            match memchr(b'\r', &handshake[ind..handshake.len()]) {
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
            return Err(HandshakeError::IOError(std::io::Error::new(ErrorKind::UnexpectedEof, "EOF")));
        }
    }

    return Ok(handshake)
}