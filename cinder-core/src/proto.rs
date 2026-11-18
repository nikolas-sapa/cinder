use serde::{Deserialize, Serialize};

pub type PeerId = [u8; 32];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {}
