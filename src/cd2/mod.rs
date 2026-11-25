// CD2 接入层：客户端 + 目的地实现。
pub mod client;
pub mod destination;

pub use client::Cd2Client;
pub use destination::Cd2Destination;
