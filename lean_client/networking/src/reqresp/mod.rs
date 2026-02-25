mod codec;
mod handler;
mod message;
mod protocol;

use async_trait::async_trait;
use futures::{AsyncRead, AsyncWrite, io};
use libp2p::{
    request_response::{self, Codec},
    swarm::NetworkBehaviour,
};
use strum::AsRefStr;

#[derive(NetworkBehaviour)]
pub struct LeanReqResp {
    inner: request_response::Behaviour<LeanCodec>,
}
