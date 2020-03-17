use crate::websocket::{RawMessage, MAX_MESSAGE_SIZE, MessageOpCode};
use tokio::io::{BufWriter, AsyncWriteExt, BufReader, AsyncReadExt, AsyncRead, AsyncWrite};
use tokio::net::tcp::{ WriteHalf, ReadHalf };
use bytes::{BufMut, Buf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MessageError{
    #[error("Socket error while reading message: {0:?}")]
    Socket(#[from] std::io::Error),
    #[error("Bad message format")]
    Format,
    #[error("Message size: {size} bytes too big. Max size is {max_size}")]
    MessageTooBig { size: u64, max_size: u64 }
}

pub async fn receive_message<T: AsyncReadExt + Unpin>(reader: &mut T) -> Result<RawMessage, MessageError> {
    let mut mask_key = [0u8; 4];
    let v = reader.read_u16().await?;

    let fin = v & (1 << 15) != 0;

    let opcode_v = ((v & (0b0000_1111 << 8)) >> 8) as u8;
    let opcode = MessageOpCode::from(opcode_v);

    let mask = v & 0b1000_0000 != 0;

    let short_payload_len = (v & 0b0111_1111) as u8;

    // println!("fin: {}, opcode: {:?} - {}, short_payload_len: {}", fin, opcode, opcode_v as u8, short_payload_len);

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

    reader.read(payload_buff.as_mut_slice()).await?;

    return Ok(RawMessage {
        fin,
        mask,
        mask_key,
        opcode,
        payload: payload_buff
    });
}

pub async fn send_message<T: AsyncWrite + Unpin>(mut msg: RawMessage, writer: &mut T) {
    let mut out = bytes::BytesMut::new();
    out.put_u8((msg.fin as u8) << 7 | (msg.opcode as u8));

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
        // for (ind, v) in msg.payload.iter_mut().enumerate() {
        //     *v = *v ^ msg.mask_key[ind % 4];
        // }
        out.put_slice(&msg.mask_key);
    }
    out.put(msg.payload.as_slice());
    writer.write(out.bytes()).await.unwrap();
}