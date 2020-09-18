use crate::types::{
    commands, ClientData, Command, GameID, GameInfo, JoinRequest, Message, Payload, MAX_CLIENTS_PER_GAME, MAX_NICK_LEN,
};
use futures::stream::{FuturesUnordered, StreamExt};
use rand::prelude::*;
use std::{collections::HashMap, sync::Arc};
use tokio::{
    sync::{mpsc, oneshot},
    time,
};

const FPS_FACTOR: f64 = 16777216.0;
const DEFAULT_FPS: f64 = FPS_FACTOR / 1008307711.0;

struct ControllerData {
    size: usize,
    owners: Vec<usize>,
}

struct Room {
    clients: Vec<ClientData>,
    game_info: GameInfo,
    controllers: Vec<ControllerData>,
    join_request_rx: mpsc::Receiver<JoinRequest>,
    interval: time::Interval,
}

impl Room {
    async fn new(mut join_request_rx: mpsc::Receiver<JoinRequest>) -> Self {
        let initial_request = join_request_rx.recv().await.unwrap();
        let game_info = initial_request.game_info;
        let controllers = game_info.controller_data_size[..game_info.total_controllers]
            .iter()
            .map(|&size| ControllerData { size: size.into(), owners: Vec::new() })
            .collect();
        let interval = time::interval(time::Duration::from_secs_f64(DEFAULT_FPS));
        let mut room = Self { game_info, join_request_rx, controllers, clients: Vec::with_capacity(2), interval };
        room.add_client(initial_request).await;
        room
    }

    fn nickname_is_unique(&self, nick: &[u8]) -> bool {
        let case_insensitive_eq = |other: &[u8]| {
            other.len() == nick.len()
                && other.iter().zip(nick).all(|(&a, &b)| {
                    let a = if a >= b'a' && a <= b'z' { a + b'A' - b'a' } else { a };
                    let b = if b >= b'a' && b <= b'z' { b + b'A' - b'a' } else { b };
                    a == b
                })
        };
        for c in &self.clients {
            if case_insensitive_eq(&c.nickname) {
                return false
            }
        }
        return true
    }

    fn clean_nickname(&self, nick: &mut Vec<u8>) {
        if let Some(len) = nick.iter().position(|&c| c == b'<' || c == b'>' || c == b'*' || c < 0x20) {
            nick.resize(len, 0);
        }
        if nick.is_empty() || !self.nickname_is_unique(nick) {
            while !self.nickname_is_unique(nick) {
                let mut rng = rand::thread_rng();
                nick.clear();
                for _ in 0..MAX_NICK_LEN.min(8) {
                    let c = (rng.gen::<u8>() as u16 * (26 * 2 - 1)).to_be_bytes()[0];
                    if c < 26 {
                        nick.push(b'A' + c);
                    } else {
                        nick.push(b'a' + c - 26);
                    }
                }
            }
        }
    }

    fn make_player_announce_buf(&self, client: &ClientData) -> Arc<[u8]> {
        let mut buf = Vec::with_capacity(4 + 4 + client.nickname.len());
        buf.extend_from_slice(&self.make_mps(client.id).to_le_bytes());
        buf.resize(8, 0);
        buf.extend_from_slice(&client.nickname);
        buf.into()
    }

    // (id, buf)
    fn get_player_list_bufs<'a>(&'a self) -> impl Iterator<Item = (usize, Arc<[u8]>)> + 'a {
        self.clients.iter().map(move |c| (c.id, self.make_player_announce_buf(c)))
    }

    async fn add_client(&mut self, request: JoinRequest) {
        // verify game info before this
        let mut client = request.client_data;
        let id = self.clients.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        client.id = id;
        self.clean_nickname(&mut client.nickname);
        self.controllers
            .iter_mut()
            .filter(|x| x.owners.is_empty())
            .take(request.local_players)
            .for_each(|x| x.owners.push(id));
        client.local_input_size_tx.send(self.get_controller_buffer_size(id)).await.ok();
        if let Some(client) = self.clients.first_mut() {
            client
                .message_tx
                .send(Message::Command(Command { cmd: commands::REQUEST_STATE, payload: Default::default() }))
                .await
                .ok();
        }
        for (_, buf) in self.get_player_list_bufs() {
            client
                .message_tx
                .send(Message::Command(Command { cmd: commands::PLAYERJOINED, payload: buf.into() }))
                .await
                .ok();
        }
        let player_joined_buf = self.make_player_announce_buf(&client);
        self.clients.push(client);
        for c in &mut self.clients {
            let cmd = if c.id == id { commands::YOUJOINED } else { commands::PLAYERJOINED };
            c.message_tx
                .send(Message::Command(Command { cmd, payload: Payload::Data(player_joined_buf.clone()) }))
                .await
                .ok();
        }
        request.result_tx.send(Ok(())).ok();
    }

    async fn run_loop(&mut self) {
        loop {
            // timing
            self.interval.tick().await;
            // handle join requests
            while let Ok(join_request) = self.join_request_rx.try_recv() {
                if self.clients.len() >= MAX_CLIENTS_PER_GAME {
                    join_request.result_tx.send(Err("Sorry, game is full.".into())).ok();
                    continue
                }
                if join_request.game_info != self.game_info {
                    // TODO: more specific errors
                    join_request.result_tx.send(Err("Your game info is invalid.".into())).ok();
                    continue
                }
                self.add_client(join_request).await;
            }
            // handle commands (non-borrowing iterator)
            for i in 0..self.clients.len() {
                while let Ok(command) = self.clients[i].command_rx.try_recv() {
                    // any suggestions of a nicer way to do this are more than welcome
                    match command.payload {
                        Payload::Number(n) => match command.cmd {
                            0..=0x3F => {
                                // emulator commands (power, reset, etc)
                                for c in &mut self.clients {
                                    c.message_tx.send(Message::Command(command.clone())).await.ok();
                                }
                            },
                            commands::SETFPS => {
                                // TODO: limit to 1-130 fps
                                self.interval =
                                    time::interval(time::Duration::from_secs_f64(FPS_FACTOR / f64::from(n)));
                            },
                            commands::REQUEST_LIST => {
                                for (id, buf) in self.get_player_list_bufs().collect::<Vec<_>>() {
                                    let cmd = if self.clients[i].id == id {
                                        commands::YOUJOINED
                                    } else {
                                        commands::PLAYERJOINED
                                    };
                                    self.clients[i]
                                        .message_tx
                                        .send(Message::Command(Command { cmd, payload: buf.into() }))
                                        .await
                                        .ok();
                                }
                            },
                            cmd => {
                                println!("Unknown command {:#x}", cmd);
                            },
                        },
                        Payload::Data(ref data) => match command.cmd {
                            commands::LOADSTATE => {
                                for c in &mut self.clients {
                                    c.message_tx.send(Message::Command(command.clone())).await.ok();
                                }
                            },
                            commands::TEXT => {
                                let text_buf = data;
                                let client = &self.clients[i];
                                let mut cmd_buf = Vec::with_capacity(4 + client.nickname.len() + text_buf.len());
                                cmd_buf.extend_from_slice(&(client.nickname.len() as u32).to_le_bytes());
                                cmd_buf.extend_from_slice(&client.nickname);
                                cmd_buf.extend_from_slice(text_buf);
                                let cmd_buf: Arc<[u8]> = Arc::from(cmd_buf);
                                for c in &mut self.clients {
                                    c.message_tx
                                        .send(Message::Command(Command {
                                            cmd: commands::TEXT,
                                            payload: crate::types::Payload::Data(cmd_buf.clone()),
                                        }))
                                        .await
                                        .ok();
                                }
                            },
                            commands::ECHO => {
                                let command = if data.len() <= 256 {
                                    command
                                } else {
                                    Command { payload: Payload::Data(data[..256].to_vec().into()), ..command }
                                };
                                self.clients[i].message_tx.send(Message::Command(command)).await.ok();
                            },
                            commands::SETNICK => {
                                let mut nick = data.to_vec();
                                self.clean_nickname(&mut nick);
                                let old_nick = std::mem::replace(&mut self.clients[i].nickname, nick);
                                let nick = &self.clients[i].nickname;
                                let mut name_change_buf = Vec::with_capacity(old_nick.len() + nick.len() + 2);
                                name_change_buf.extend_from_slice(&old_nick);
                                name_change_buf.push(b'\n');
                                name_change_buf.extend_from_slice(&nick);
                                name_change_buf.push(0);
                                let name_change_buf: Arc<[u8]> = name_change_buf.into();
                                for c in &mut self.clients {
                                    c.message_tx
                                        .send(Message::Command(Command {
                                            cmd: commands::NICKCHANGED,
                                            payload: Payload::Data(name_change_buf.clone()),
                                        }))
                                        .await
                                        .ok();
                                }
                            },
                            commands::SET_MEDIA => {
                                // TODO: length must equal 16
                                for c in &mut self.clients {
                                    c.message_tx.send(Message::Command(command.clone())).await.ok();
                                }
                            },
                            commands::QUIT => {
                                // TODO: use quit message
                                self.clients[i].dead = true;
                            },
                            cmd => {
                                println!("Unknown command {:#x}", cmd);
                            },
                        },
                    }
                }
            }
            // collect inputs
            let mut inputs = vec![0; self.controllers.iter().map(|c| c.size).sum()];
            for c in self.clients.iter_mut().filter(|c| c.changes_pending == 0) {
                let local_buf = c.controller_buffer.lock();
                let mut local_offset = 0usize;
                let mut global_offset = 0usize;
                for controller in &self.controllers {
                    if controller.owners.contains(&c.id) {
                        for j in 0..controller.size {
                            inputs[global_offset + j] |= local_buf[local_offset + j];
                        }
                        local_offset += controller.size;
                    }
                    global_offset += controller.size;
                }
            }
            let inputs: Arc<[u8]> = inputs.into();
            let mut dead_clients = Vec::new();
            // send inputs and collect dead clients
            // i would LOVE to use drain_filter for this but i want this to work on stable
            {
                let mut i = 0;
                while i < self.clients.len() {
                    if self.clients[i].dead
                        || self.clients[i].message_tx.send(Message::AllGamepads(inputs.clone())).await.is_err()
                    {
                        dead_clients.push(self.clients.remove(i));
                    } else {
                        i += 1;
                    }
                }
            }
            if !dead_clients.is_empty() {
                // quit if everyone left
                if self.clients.is_empty() {
                    return
                }
                // send quit messages and take back controllers
                for dead_client in &dead_clients {
                    let announce_buf = self.make_player_announce_buf(&dead_client);
                    for c in &mut self.clients {
                        c.message_tx
                            .send(Message::Command(Command {
                                cmd: commands::PLAYERLEFT,
                                payload: announce_buf.clone().into(),
                            }))
                            .await
                            .ok();
                    }
                    self.controllers.iter_mut().for_each(|co| co.owners.retain(|&id| id != dead_client.id));
                }
            }
        }
    }

    fn get_controller_buffer_size(&self, player_id: usize) -> usize {
        self.controllers.iter().filter(|c| c.owners.contains(&player_id)).map(|c| c.size).sum()
    }

    fn make_mps(&self, player_id: usize) -> u32 {
        let mut mps = 0u32;
        self.controllers
            .iter()
            .enumerate()
            .filter(|(_, c)| c.owners.contains(&player_id))
            .for_each(|(i, _)| mps |= 1 << i);
        mps
    }
}

pub async fn manage_rooms(mut join_request_receiver: mpsc::Receiver<JoinRequest>) {
    let mut rooms: HashMap<GameID, mpsc::Sender<JoinRequest>> = HashMap::new();
    let mut lifelines = FuturesUnordered::new();
    // this function should never panic once it's entered the loop
    loop {
        tokio::select! {
            Some(request) = join_request_receiver.recv() => {
                let game_id = request.game_id;
                // this is a closure as it may need calling in multiple places
                let create_room = || {
                    let (join_tx, join_rx) = mpsc::channel(2);
                    let task_handle = tokio::spawn(async {
                        Room::new(join_rx).await.run_loop().await
                    });

                    lifelines.push(async move {
                        let _ = task_handle.await;
                        game_id
                    });
                    join_tx
                };
                let room = rooms.entry(game_id).or_insert_with(create_room);

                // inject a result receiver so we can send errors if we failed to join
                let result_to_client_tx = request.result_tx;
                let (result_tx, result_rx) = oneshot::channel();
                let request = JoinRequest {result_tx, ..request};
                let result = if room.send(request).await.is_err() {
                    Err("The room closed while you were trying to join. Please try again.".into())
                } else {
                    result_rx.await.unwrap_or(
                        Err("The room did not respond to the join request. Please try again.".into())
                    )
                };
                result_to_client_tx.send(result).ok();
            },
            Some(game_id) = lifelines.next() => {
                rooms.remove(&game_id);
            },
            else => return,
        }
    }
}
