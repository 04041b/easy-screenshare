pub mod codec;
pub mod sender;
pub mod viewer;

pub const STUN_SERVERS: &[&str] = &[
    "stun:stun.cloudflare.com:3478",
    "stun:stun.l.google.com:19302",
];
