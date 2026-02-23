use super::RPCError;
use super::RequestType;
use super::protocol::ProtocolId;
use crate::rpc::codec::SSZSnappyOutboundCodec;
use crate::rpc::protocol::Encoding;
use futures::future::BoxFuture;
use futures::prelude::{AsyncRead, AsyncWrite};
use futures::{FutureExt, SinkExt};
use libp2p::core::{OutboundUpgrade, UpgradeInfo};
use tokio_util::{
    codec::Framed,
    compat::{Compat, FuturesAsyncReadCompatExt},
};

/// Container for an outbound RPC request.
#[derive(Debug, Clone)]
pub struct OutboundRequestContainer {
    pub req: RequestType,
    pub max_rpc_size: usize,
}

impl UpgradeInfo for OutboundRequestContainer {
    type Info = ProtocolId;
    type InfoIter = Vec<Self::Info>;

    fn protocol_info(&self) -> Self::InfoIter {
        self.req.supported_protocols()
    }
}

pub type OutboundFramed<TSocket> = Framed<Compat<TSocket>, SSZSnappyOutboundCodec>;

impl<TSocket> OutboundUpgrade<TSocket> for OutboundRequestContainer
where
    TSocket: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Output = OutboundFramed<TSocket>;
    type Error = RPCError;
    type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn upgrade_outbound(self, socket: TSocket, protocol: Self::Info) -> Self::Future {
        let socket = socket.compat();
        let codec = match protocol.encoding {
            Encoding::SSZSnappy => SSZSnappyOutboundCodec::new(protocol, self.max_rpc_size),
        };

        let mut socket = Framed::new(socket, codec);

        async {
            socket.send(self.req).await?;
            socket.close().await?;
            Ok(socket)
        }
        .boxed()
    }
}
