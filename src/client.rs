use crate::{
    config::Config,
    types::{
        commands, ClientData, Command, GameID, GameInfo, JoinRequest, JoinRequestSink, Message, Payload, Protocol,
        LOGIN_DATA_MINSIZE, MAX_CONTROLLERS_PER_GAME, MAX_NICK_LEN, MAX_TOTAL_CONTROLLERS_DATA_SIZE,
    },
};
use parking_lot::Mutex;
use std::{convert::TryInto, sync::Arc};
use tokio::{
    net::TcpStream,
    prelude::*,
    sync::{mpsc, oneshot},
    time::timeout,
};

pub struct LoginData {
    pub game_id: GameID,
    pub password: [u8; 16],
    pub protocol: Protocol,
    pub total_controllers: usize,
    pub controller_data_size: [u8; 16],
    pub controller_type: [u8; 16],
    pub local_players: usize,
    pub nickname: Vec<u8>,
    pub emu_id: [u8; 64],
}

async fn send_connect_error(stream: &mut (impl AsyncWrite + Unpin), cmd_buf: &mut [u8], text: &str) -> io::Result<()> {
    cmd_buf[cmd_buf.len() - 1] = commands::TEXT;
    let mut data_buf = vec![0; text.len() + 4];
    data_buf[4..].copy_from_slice(text.as_bytes());
    cmd_buf[..4].copy_from_slice(&(text.len() as u32).to_le_bytes());
    stream.write_all(cmd_buf).await?;
    stream.write_all(&data_buf).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn manage_client(
    mut stream: TcpStream,
    mut join_request_sink: JoinRequestSink,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
    // avoid lifetime errors
    let Config { connect_timeout, idle_timeout, .. } = *config;
    // get login data length
    let mut login_len_buf = [0; 4];
    timeout(connect_timeout, stream.read_exact(&mut login_len_buf)).await??;
    let login_len = u32::from_le_bytes(login_len_buf) as usize;
    if login_len < LOGIN_DATA_MINSIZE || login_len > LOGIN_DATA_MINSIZE + MAX_NICK_LEN + 8192 {
        return Err(format!("Login len({}) out of range.", login_len).into())
    }
    // get login data
    let mut login_buf = vec![0; login_len];
    timeout(connect_timeout, stream.read_exact(&mut login_buf)).await??;
    // parse login data
    let login_data = parse(&login_buf)?;

    // allocate this here in case there's an error
    let controller_buffer_len = login_data.controller_data_size[..login_data.total_controllers]
        .iter()
        .map(|&x| usize::from(x))
        .sum::<usize>()
        .max(4);
    let mut cmd_buf = vec![0; controller_buffer_len + 1];

    if let Some(password) = config.password {
        if login_data.password != password {
            send_connect_error(&mut stream, &mut cmd_buf, "Invalid server password.").await?;
            return Err("Invalid server password.".into())
        }
    }

    let game_info = GameInfo {
        protocol: login_data.protocol,
        emu_id: login_data.emu_id,
        total_controllers: login_data.total_controllers,
        controller_type: login_data.controller_type,
        controller_data_size: login_data.controller_data_size,
    };

    let controller_buffer: Arc<Mutex<Vec<u8>>> = Arc::new(vec![0; controller_buffer_len].into());

    let (mut command_tx, command_rx) = mpsc::channel::<Command>(8);

    let (message_tx, mut message_rx) = mpsc::channel::<Message>(8);

    let (local_input_size_tx, mut local_input_size_rx) = mpsc::channel::<usize>(1);

    let (result_tx, result_rx) = oneshot::channel::<Result<(), String>>();

    join_request_sink
        .send(JoinRequest {
            game_id: login_data.game_id,
            game_info,
            client_data: ClientData {
                id: 0, // set by the room
                dead: false,
                nickname: login_data.nickname,
                controller_buffer: controller_buffer.clone(),
                command_rx,
                message_tx,
                local_input_size_tx,
            },
            local_players: login_data.local_players,
            result_tx,
        })
        .await?;

    if let Err(e) = result_rx.await? {
        send_connect_error(&mut stream, &mut cmd_buf, &e).await?;
        return Err(e.into())
    }

    let (sock_read, sock_write) = stream.into_split();
    // spawn client->game comms task
    let client_read = async move {
        let mut sock_read = io::BufReader::new(sock_read);
        let input_buf_len = local_input_size_rx.recv().await.unwrap();
        controller_buffer.lock().resize(input_buf_len, 0);
        let mut input_buf = vec![0; input_buf_len + 1];
        loop {
            timeout(idle_timeout, sock_read.read_exact(&mut input_buf)).await??;
            let cmd = input_buf[0];
            if cmd == 0 {
                // just controller data
                controller_buffer.lock().copy_from_slice(&input_buf[1..]);
            } else {
                // it's a command
                let mut cmd_len_buf = [0; 4];
                timeout(idle_timeout, sock_read.read_exact(&mut cmd_len_buf)).await??;
                let cmd_len = u32::from_le_bytes(cmd_len_buf);
                if cmd == commands::INTEGRITY || (cmd & 0x80) == 0 {
                    // no payload, all we need is that number
                    match cmd {
                        commands::NOP => (),
                        _ => command_tx.send(Command { cmd, payload: Payload::Number(cmd_len) }).await?,
                    }
                    if cmd == commands::CTRL_CHANGE_ACK {
                        // input buffer size changed, adjust
                        let input_buf_len = local_input_size_rx.recv().await.unwrap();
                        controller_buffer.lock().resize(input_buf_len, 0);
                        input_buf.resize(input_buf_len + 1, 0);
                    }
                } else {
                    // we got a payload coming in boys
                    let mut payload_buf = vec![0; cmd_len as usize];
                    timeout(idle_timeout, sock_read.read_exact(&mut payload_buf)).await??;
                    match cmd {
                        commands::INTEGRITY_RES => {
                            // the original server did this for ??? reasons
                            println!("{:x}", md5::compute(payload_buf));
                        },
                        _ => command_tx.send(Command { cmd, payload: Payload::Data(payload_buf.into()) }).await?,
                    }
                }
            }
        }
    };
    tokio::spawn(async move {
        match client_read.await {
            Ok::<(), Box<dyn std::error::Error>>(()) => (),
            Err(e) => println!("{}", e),
        }
    });

    // game->client comms
    let mut sock_write = io::BufWriter::new(sock_write);
    while let Some(message) = message_rx.recv().await {
        match message {
            Message::Command(command) => {
                // send initial buffer with payload length and command
                cmd_buf[controller_buffer_len] = command.cmd;
                cmd_buf[0..4].copy_from_slice(
                    &match command.payload {
                        Payload::Number(x) => x,
                        Payload::Data(ref data) => data.len() as u32,
                    }
                    .to_le_bytes(),
                );
                sock_write.write_all(&cmd_buf).await?;
                // send payload
                if let Payload::Data(data) = command.payload {
                    sock_write.write_all(&data).await?;
                }
            },
            Message::AllGamepads(data) => {
                // only send data once a frame to save packets
                sock_write.write_all(&data).await?;
                sock_write.write_all(b"\0").await?;
                sock_write.flush().await?;
            },
        }
    }
    Ok(())
}

/// Parse login info into a struct, and verify its integrity.
pub fn parse(buf: &[u8]) -> Result<LoginData, String> {
    let protocol = match buf[32] {
        1 => Protocol::V1,
        2 => Protocol::V2,
        3 => Protocol::V3,
        p => return Err(format!("Protocol {} not supported.", p).into()),
    };
    let total_controllers: usize = buf[33].into();
    if protocol < Protocol::V3 && total_controllers > 8 {
        return Err(format!(
            "That number of controllers({}) isn't supported with that protocol version.",
            total_controllers
        )
        .into())
    }
    if total_controllers > MAX_CONTROLLERS_PER_GAME {
        return Err(format!("That number of controllers({}) isn't supported.", total_controllers).into())
    }

    let mut password = [0; 16];
    password.copy_from_slice(&buf[16..32]);

    let mut controller_data_size = [1; 16];
    let mut controller_type = [1; 16];
    if protocol >= Protocol::V3 {
        controller_data_size.copy_from_slice(&buf[48..64]);
        controller_type.copy_from_slice(&buf[80..96]);
        if MAX_TOTAL_CONTROLLERS_DATA_SIZE < controller_data_size.iter().map(|x| usize::from(*x)).sum() {
            return Err("Exceeded MaxTotalControllersDataSize".into())
        }
    }

    let mut game_id = [0; 16];
    game_id.copy_from_slice(&buf[..16]);

    let local_players = usize::from(buf[96]);

    let emu_id_len = u32::from_le_bytes(buf[36..40].try_into().unwrap()) as usize;
    let nickname_len = buf.len().saturating_sub(LOGIN_DATA_MINSIZE + emu_id_len).min(MAX_NICK_LEN);

    let mut emu_id = [0; 64];
    if protocol == Protocol::V3
        && emu_id_len > 0
        && emu_id_len <= 64
        && LOGIN_DATA_MINSIZE + nickname_len + emu_id_len <= buf.len()
    {
        emu_id[0..emu_id_len]
            .copy_from_slice(&buf[LOGIN_DATA_MINSIZE + nickname_len..LOGIN_DATA_MINSIZE + nickname_len + emu_id_len]);
        for c in emu_id.iter_mut() {
            if *c > 0 && *c < 0x20 {
                *c = 0x20;
            }
        }
    }

    let nickname = buf[LOGIN_DATA_MINSIZE..LOGIN_DATA_MINSIZE + nickname_len].to_vec();

    Ok(LoginData {
        game_id,
        password,
        protocol,
        total_controllers,
        controller_data_size,
        controller_type,
        local_players,
        nickname,
        emu_id,
    })
}
