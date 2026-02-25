use async_trait::async_trait;
use futures::{AsyncRead, AsyncWrite, io};
use libp2p::request_response::Codec;

use crate::reqresp::message::ResponseType;

use super::{message::RequestType, protocol::ProtocolId};

#[derive(Debug, Clone)]
pub struct LeanCodec {
    max_request_size: usize,
    max_response_size: usize,
}

#[async_trait]
impl Codec for LeanCodec {
    /// The type of protocol(s) or protocol versions being negotiated.
    type Protocol = ProtocolId;
    /// The type of inbound and outbound requests.
    type Request = RequestType;
    /// The type of inbound and outbound responses.
    type Response = ResponseType;

    /// Reads a request from the given I/O stream according to the
    /// negotiated protocol.
    async fn read_request<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        io.rea

        todo!()
    }

    /// Reads a response from the given I/O stream according to the
    /// negotiated protocol.
    async fn read_response<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        todo!()
    }

    /// Writes a request to the given I/O stream according to the
    /// negotiated protocol.
    async fn write_request<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        todo!()
    }

    /// Writes a response to the given I/O stream according to the
    /// negotiated protocol.
    async fn write_response<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        todo!()
    }
}
