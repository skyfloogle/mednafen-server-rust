mod client;
mod config;
mod room;
mod types;

use client::manage_client;
use config::Config;
use room::manage_rooms;
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    path::PathBuf,
};
use tokio::{net::TcpListener, sync::mpsc};

#[cfg(target_pointer_width = "16")]
error!("cannot build on a 16-bit architecture");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os();
    if args.len() < 2 {
        let path = PathBuf::from(args.next().unwrap());
        let needs_quotes = {
            // assume it needs quotes if the path isn't valid unicode
            if let Some(path) = path.to_str() { path.contains(' ') } else { true }
        };
        if needs_quotes {
            println!("Usage: \"{}\" <configfile>", path.display());
        } else {
            println!("Usage: {} <configfile>", path.display());
        }
        return Err("No arguments provided".into())
    }
    let config = Config::from_file(args.nth(1).unwrap());
    let mut listener_v4 = TcpListener::bind((Ipv4Addr::UNSPECIFIED, config.port)).await?;
    let mut listener_v6 = TcpListener::bind((Ipv6Addr::UNSPECIFIED, config.port)).await?;
    let (join_request_sink, join_requests) = mpsc::channel(8);
    // clone the config because otherwise it'd have to be static and that means unsafe blocks
    tokio::spawn(manage_rooms(join_requests));
    loop {
        // if these return an error the listeners died so it's fine to crash here
        let stream = tokio::select! {
            res = listener_v4.accept() => res?.0,
            res = listener_v6.accept() => res?.0,
        };

        let join_request_sink = join_request_sink.clone();
        tokio::spawn(async move {
            match manage_client(stream, join_request_sink, &config).await {
                Ok(()) => (),
                Err(e) => println!("{}", e),
            }
        });
    }
}
