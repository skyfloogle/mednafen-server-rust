use std::path::Path;
use tokio::time::Duration;

#[derive(Clone, Copy, Default)]
pub struct Config {
    pub max_clients: usize,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    pub port: u16,
    pub password: Option<[u8; 16]>,
    pub min_send_queue_size: usize,
    pub max_send_queue_size: usize,
    pub max_command_payload: usize,
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Self {
        Self {
            max_clients: 50,
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(30),
            port: 4046,
            password: None,
            min_send_queue_size: 262144,
            max_send_queue_size: 8388608,
            max_command_payload: 5242880,
        }
    }
}
