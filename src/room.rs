use crate::types::{
    commands, ClientData, Command, GameID, GameInfo, JoinRequest, Message, Payload, MAX_CLIENTS_PER_GAME,
};
use futures::stream::{FuturesUnordered, StreamExt};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    sync::{mpsc, oneshot},
    time,
};

const FPS_FACTOR: f64 = 16777216.0;
const DEFAULT_FPS: f64 = FPS_FACTOR / 1008307711.0;

async fn manage_room(mut join_request_rx: mpsc::Receiver<JoinRequest>) {
    let initial_request = join_request_rx.recv().await.unwrap();
    let game_info = initial_request.game_info;
    let total_controllers = game_info.total_controllers;
    let controller_sizes = &game_info.controller_data_size[..total_controllers];
    let mut controller_owners = vec![Vec::new(); total_controllers];
    let get_controller_buffer_size = |p, owners: &[Vec<usize>]| {
        owners.iter().zip(controller_sizes).filter(|(x, _)| x.contains(&p)).map(|(_, &x)| usize::from(x)).sum()
    };
    let make_mps = |p, owners: &[Vec<usize>]| {
        let mut mps = 0u32;
        owners.iter().enumerate().filter(|(_, x)| x.contains(&p)).for_each(|(i, _)| mps |= 1 << i);
        mps
    };
    let mut interval = time::interval(time::Duration::from_secs_f64(DEFAULT_FPS));
    let mut clients = Vec::new();
    {
        // handle initial client
        // TODO: standardize this a bit
        let mut initial_client = initial_request.client_data;
        controller_owners[0..initial_request.local_players].iter_mut().for_each(|x| x.push(0));
        initial_client.local_input_size_tx.send(get_controller_buffer_size(0, &controller_owners)).await.unwrap();
        let mut player_joined_buf = Vec::with_capacity(4 + 4 + initial_client.nickname.len());
        player_joined_buf.extend_from_slice(&make_mps(0, &controller_owners).to_le_bytes());
        player_joined_buf.resize(8, 0);
        player_joined_buf.extend_from_slice(&initial_client.nickname);
        initial_client
            .message_tx
            .send(Message::Command(Command {
                cmd: commands::YOUJOINED,
                payload: crate::types::Payload::Data(player_joined_buf.into()),
            }))
            .await
            .unwrap();
        clients.push(initial_client); // its id is 0 and will stay as such
    }
    initial_request.result_tx.send(Ok(())).unwrap();
    loop {
        // timing
        interval.tick().await;
        // handle join requests
        while let Ok(join_request) = join_request_rx.try_recv() {
            if clients.len() >= MAX_CLIENTS_PER_GAME {
                join_request.result_tx.send(Err("Sorry, game is full.".into())).ok();
                continue
            }
            if join_request.game_info != game_info {
                // TODO: more specific errors
                join_request.result_tx.send(Err("Your game info is invalid.".into())).ok();
                continue
            }
            let mut client = join_request.client_data;
            let id = clients.iter().map(|c| c.id).max().unwrap_or(0) + 1;
            client.id = id;
            controller_owners
                .iter_mut()
                .filter(|x| x.is_empty())
                .take(join_request.local_players)
                .for_each(|x| x.push(id));
            client.local_input_size_tx.send(get_controller_buffer_size(id, &controller_owners)).await.ok();
            let mut player_joined_buf = Vec::with_capacity(4 + 4 + client.nickname.len());
            player_joined_buf.extend_from_slice(&make_mps(id, &controller_owners).to_le_bytes());
            player_joined_buf.resize(8, 0);
            player_joined_buf.extend_from_slice(&client.nickname);
            let player_joined_buf: Arc<[u8]> = Arc::from(player_joined_buf);
            clients.push(client);
            join_request.result_tx.send(Ok(())).ok();
            for c in &mut clients {
                let cmd = if c.id == id { commands::YOUJOINED } else { commands::PLAYERJOINED };
                c.message_tx
                    .send(Message::Command(Command { cmd, payload: Payload::Data(player_joined_buf.clone()) }))
                    .await
                    .ok();
            }
        }
        // handle commands (non-borrowing iterator)
        for i in 0..clients.len() {
            while let Ok(command) = clients[i].command_rx.try_recv() {
                // any suggestions of a nicer way to do this are more than welcome
                match command.payload {
                    Payload::Number(n) => match command.cmd {
                        0..=0x3F => {
                            // emulator commands (power, reset, etc)
                            for c in &mut clients {
                                c.message_tx.send(Message::Command(command.clone())).await.ok();
                            }
                        },
                        commands::SETFPS => {
                            // TODO: limit to 1-130 fps
                            interval = time::interval(time::Duration::from_secs_f64(FPS_FACTOR / f64::from(n)));
                        },
                        cmd => {
                            println!("Unknown command {:#x}", cmd);
                        },
                    },
                    Payload::Data(ref data) => match command.cmd {
                        commands::TEXT => {
                            let text_buf = data;
                            let client = &clients[i];
                            let mut cmd_buf = Vec::with_capacity(4 + client.nickname.len() + text_buf.len());
                            cmd_buf.extend_from_slice(&(client.nickname.len() as u32).to_le_bytes());
                            cmd_buf.extend_from_slice(&client.nickname);
                            cmd_buf.extend_from_slice(text_buf);
                            let cmd_buf: Arc<[u8]> = Arc::from(cmd_buf);
                            for c in &mut clients {
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
                            clients[i].message_tx.send(Message::Command(command)).await.ok();
                        },
                        commands::QUIT => {
                            // TODO: use quit message
                            clients[i].dead = true;
                        },
                        cmd => {
                            println!("Unknown command {:#x}", cmd);
                        },
                    },
                }
            }
        }
        // collect inputs
        let mut inputs = vec![0; controller_sizes.iter().map(|&x| usize::from(x)).sum()];
        for c in &mut clients {
            let local_buf = c.controller_buffer.lock();
            let mut local_offset = 0usize;
            let mut global_offset = 0usize;
            for i in 0..total_controllers {
                let c_size = usize::from(controller_sizes[i]);
                if controller_owners[i].contains(&c.id) {
                    for j in 0..c_size {
                        let inp = local_buf[local_offset + j];
                        inputs[global_offset + j] |= inp;
                    }
                    local_offset += c_size;
                }
                global_offset += c_size;
            }
        }
        let inputs: Arc<[u8]> = inputs.into();
        // send inputs and mark dead clients
        for c in &mut clients {
            if c.message_tx.send(Message::AllGamepads(inputs.clone())).await.is_err() {
                c.dead = true;
            }
        }
        // remove dead clients
        // i would use drain_filter for this but i want this to work on stable
        clients.retain(|c| {
            if c.dead {
                controller_owners.iter_mut().for_each(|v| v.retain(|&id| id != c.id));
            }
            !c.dead
        });
        // quit if everyone left
        if clients.is_empty() {
            return
        }
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
                    let task_handle = tokio::spawn(manage_room(join_rx));

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
