use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub const MAX_NICK_LEN: usize = 150;
pub const MAX_CLIENTS_PER_GAME: usize = 32;
pub const MAX_CONTROLLERS_PER_GAME: usize = 16;

pub const MAX_TOTAL_CONTROLLERS_DATA_SIZE: usize = 512;

pub const LOGIN_DATA_MINSIZE: usize = 97;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    V1,
    V2,
    V3,
}
pub type GameID = [u8; 16];

pub type JoinRequestSink = mpsc::Sender<JoinRequest>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GameInfo {
    pub protocol: Protocol,
    pub emu_id: [u8; 64],
    pub total_controllers: usize,
    pub controller_type: [u8; 16],
    pub controller_data_size: [u8; 16],
}

#[derive(Debug)]
pub struct ClientData {
    pub id: usize, // set by the room
    pub dead: bool,
    pub nickname: Vec<u8>,
    pub controller_buffer: Arc<parking_lot::Mutex<Vec<u8>>>, // client -> room
    pub command_rx: mpsc::Receiver<Command>,                 // client -> room
    pub message_tx: mpsc::Sender<Message>,                   // room -> client
    pub local_input_size_tx: mpsc::Sender<usize>,
}

#[derive(Debug)]
pub struct JoinRequest {
    pub game_id: GameID,
    pub game_info: GameInfo,
    pub client_data: ClientData,
    pub local_players: usize,
    pub result_tx: oneshot::Sender<Result<(), String>>,
}

#[derive(Clone, Debug)]
pub enum Payload {
    Number(u32),
    Data(Arc<[u8]>),
}

#[derive(Clone, Debug)]
pub struct Command {
    pub cmd: u8,
    pub payload: Payload,
}

#[derive(Debug)]
pub enum Message {
    Command(Command),
    AllGamepads(Arc<[u8]>),
}

pub mod commands {
    pub const SETFPS: u8 = 0x40;
    pub const NOP: u8 = 0x41;
    pub const CTRL_CHANGE: u8 = 0x43;
    pub const CTRL_CHANGE_ACK: u8 = 0x44;
    pub const CTRLR_SWAPNOTIF: u8 = 0x68;
    pub const CTRLR_TAKE: u8 = 0x70;
    pub const CTRLR_DROP: u8 = 0x71;
    pub const CTRLR_DUPE: u8 = 0x72;
    pub const CTRLR_SWAP: u8 = 0x78;
    pub const REQUEST_LIST: u8 = 0x7F;
    pub const LOADSTATE: u8 = 0x80;
    pub const REQUEST_STATE: u8 = 0x81;
    pub const TEXT: u8 = 0x90;
    pub const SERVERTEXT: u8 = 0x93;
    pub const ECHO: u8 = 0x94;
    pub const INTEGRITY: u8 = 0x95;
    pub const INTEGRITY_RES: u8 = 0x96;
    pub const SETNICK: u8 = 0x98;
    pub const PLAYERJOINED: u8 = 0xA0;
    pub const PLAYERLEFT: u8 = 0xA1;
    pub const YOUJOINED: u8 = 0xB0;
    pub const YOULEFT: u8 = 0xB1;
    pub const NICKCHANGED: u8 = 0xB8;
    pub const LIST: u8 = 0xC0;
    pub const SET_MEDIA: u8 = 0xD0;
    pub const CTRLR_TAKE_NOTIF: u8 = 0xF0;
    pub const CTRLR_DROP_NOTIF: u8 = 0xF1;
    pub const CTRLR_DUPE_NOTIF: u8 = 0xF2;
    pub const QUIT: u8 = 0xFF;
}
