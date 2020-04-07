#[derive(Debug, Clone, Copy)]
pub enum MessageOpCode {
    ContinuationFrame = 0,
    TextFrame = 1,
    BinaryFrame = 2,
    Close = 8,
    Ping = 9,
    Pong = 10,
    Unknown = 15
}

impl From<u8> for MessageOpCode {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::ContinuationFrame,
            1 => Self::TextFrame,
            2 => Self::BinaryFrame,
            8 => Self::Close,
            9 => Self::Ping,
            10 => Self::Pong,
            _ => Self::Unknown
        }
    }
}

#[derive(Debug)]
pub struct RawMessage {
    pub fin: bool,
    pub opcode: MessageOpCode,
    pub mask: bool,
    pub mask_key: [u8; 4],
    pub payload: Vec<u8>
}

impl RawMessage {
    pub fn unmask(&mut self) {
        if self.mask {
            for (ind, v) in self.payload.iter_mut().enumerate() {
                *v = *v ^ self.mask_key[ind % 4];
            }
        }
    }

    pub fn close_message() -> Self {
        Self {
            fin: true,
            opcode: MessageOpCode::Close,
            mask: false,
            mask_key: [0; 4],
            payload: vec![0x0f, 0xa0],
        }
    }

    pub fn text_message(text: Vec<u8>) -> RawMessage {
        Self {
            fin: true,
            opcode: MessageOpCode::TextFrame,
            mask: false,
            mask_key: [0; 4],
            payload: text
        }
    }
}