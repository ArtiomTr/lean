use crate::rpc::RequestType;
use crate::rpc::methods::*;
use crate::rpc::protocol::{
    ERROR_TYPE_MAX, ERROR_TYPE_MIN, Encoding, ProtocolId, RPCError, SupportedProtocol,
};
use containers::{SignedBlockWithAttestation, Status};
use libp2p::bytes::BufMut;
use libp2p::bytes::BytesMut;
use snap::read::FrameDecoder;
use snap::write::FrameEncoder;
use ssz::{ContiguousList, DynamicList, H256, SszRead as _, SszReadDefault, SszWrite as _};
use std::io::Cursor;
use std::io::ErrorKind;
use std::io::{Read, Write};
use std::sync::Arc;
use tokio_util::codec::{Decoder, Encoder};
use types::config::Config as ChainConfig;

use unsigned_varint::codec::Uvi;

/* Inbound Codec */

pub struct SSZSnappyInboundCodec {
    chain_config: Arc<ChainConfig>,
    protocol: ProtocolId,
    inner: Uvi<usize>,
    len: Option<usize>,
    /// Maximum bytes that can be sent in one req/resp chunked responses.
    max_packet_size: usize,
}

impl SSZSnappyInboundCodec {
    pub fn new(
        chain_config: Arc<ChainConfig>,
        protocol: ProtocolId,
        max_packet_size: usize,
    ) -> Self {
        let uvi_codec = Uvi::default();
        // this encoding only applies to ssz_snappy.
        debug_assert_eq!(protocol.encoding, Encoding::SSZSnappy);

        SSZSnappyInboundCodec {
            chain_config,
            inner: uvi_codec,
            protocol,
            len: None,
            max_packet_size,
        }
    }

    /// Encodes RPC Responses sent to peers.
    fn encode_response(
        &mut self,
        item: RpcResponse,
        dst: &mut BytesMut,
    ) -> Result<(), RPCError> {
        let bytes = match &item {
            RpcResponse::Success(resp) => match &resp {
                RpcSuccessResponse::Status(res) => res.to_ssz()?,
                RpcSuccessResponse::BlocksByRoot(res) => res.to_ssz()?,
                RpcSuccessResponse::Pong(res) => res.data.to_ssz()?,
                RpcSuccessResponse::MetaData(res) => {
                    // Encode the correct version of the MetaData response based on the negotiated version.
                    match self.protocol.versioned_protocol {
                        SupportedProtocol::MetaDataV1 => res.metadata_v1().to_ssz()?,
                        SupportedProtocol::MetaDataV2 => res.metadata_v2().to_ssz()?,
                        _ => unreachable!(
                            "We only send metadata responses on negotiating metadata requests"
                        ),
                    }
                }
            },
            RpcResponse::Error(_, err) => err.to_ssz()?,
            RpcResponse::StreamTermination(_) => {
                unreachable!("Code error - attempting to encode a stream termination")
            }
        };

        // SSZ encoded bytes should be within `max_packet_size`
        if bytes.len() > self.max_packet_size {
            return Err(RPCError::InternalError(
                "attempting to encode data > max_packet_size",
            ));
        }

        // Inserts the length prefix of the uncompressed bytes into dst
        // encoded as a unsigned varint
        self.inner
            .encode(bytes.len(), dst)
            .map_err(RPCError::from)?;

        let mut writer = FrameEncoder::new(Vec::new());
        writer.write_all(&bytes).map_err(RPCError::from)?;
        writer.flush().map_err(RPCError::from)?;

        // Write compressed bytes to `dst`
        dst.extend_from_slice(writer.get_ref());
        Ok(())
    }
}

// Encoder for inbound streams: Encodes RPC Responses sent to peers.
impl Encoder<RpcResponse> for SSZSnappyInboundCodec {
    type Error = RPCError;

    fn encode(&mut self, item: RpcResponse, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.clear();
        dst.reserve(1);
        dst.put_u8(
            item.as_u8()
                .expect("Should never encode a stream termination"),
        );

        let result = self.encode_response(item, dst);
        let _count: u64 = dst
            .len()
            .try_into()
            .map_err(|_| RPCError::InvalidData("byte count does not fit in u64".into()))?;

        result
    }
}

// Decoder for inbound streams: Decodes RPC requests from peers
impl Decoder for SSZSnappyInboundCodec {
    type Item = RequestType;
    type Error = RPCError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let _count: u64 = src
            .len()
            .try_into()
            .map_err(|_| RPCError::InvalidData("byte count does not fit in u64".into()))?;

        if self.protocol.versioned_protocol == SupportedProtocol::MetaDataV1 {
            return Ok(Some(RequestType::MetaData(MetadataRequest::new_v1())));
        }
        if self.protocol.versioned_protocol == SupportedProtocol::MetaDataV2 {
            return Ok(Some(RequestType::MetaData(MetadataRequest::new_v2())));
        }
        let Some(length) = handle_length(&mut self.inner, &mut self.len, src)? else {
            return Ok(None);
        };

        // Should not attempt to decode rpc chunks with `length > max_packet_size` or not within bounds of
        // packet size for ssz container corresponding to `self.protocol`.
        let ssz_limits = self.protocol.rpc_request_limits(&self.chain_config);

        if ssz_limits.is_out_of_bounds(length, self.max_packet_size) {
            return Err(RPCError::InvalidData(format!(
                "RPC request length for protocol {:?} is out of bounds, length {}",
                self.protocol.versioned_protocol, length
            )));
        }
        // Calculate worst case compression length for given uncompressed length
        let max_compressed_len = snap::raw::max_compress_len(length) as u64;

        // Create a limit reader as a wrapper that reads only upto `max_compressed_len` from `src`.
        let limit_reader = Cursor::new(src.as_ref()).take(max_compressed_len);
        let mut reader = FrameDecoder::new(limit_reader);
        let mut decoded_buffer = vec![0; length];

        match reader.read_exact(&mut decoded_buffer) {
            Ok(()) => {
                // `n` is how many bytes the reader read in the compressed stream
                let n = reader.get_ref().get_ref().position();
                self.len = None;
                let _read_bytes = src.split_to(n as usize);
                handle_rpc_request(
                    &self.chain_config,
                    self.protocol.versioned_protocol,
                    &decoded_buffer,
                )
            }
            Err(e) => handle_error(e, reader.get_ref().get_ref().position(), max_compressed_len),
        }
    }
}

/* Outbound Codec: Codec for initiating RPC requests */
pub struct SSZSnappyOutboundCodec {
    inner: Uvi<usize>,
    len: Option<usize>,
    protocol: ProtocolId,
    /// Maximum bytes that can be sent in one req/resp chunked responses.
    max_packet_size: usize,
    /// Keeps track of the current response code for a chunk.
    current_response_code: Option<u8>,
}

impl SSZSnappyOutboundCodec {
    pub fn new(
        protocol: ProtocolId,
        max_packet_size: usize,
    ) -> Self {
        let uvi_codec = Uvi::default();
        // this encoding only applies to ssz_snappy.
        debug_assert_eq!(protocol.encoding, Encoding::SSZSnappy);

        SSZSnappyOutboundCodec {
            inner: uvi_codec,
            protocol,
            max_packet_size,
            len: None,
            current_response_code: None,
        }
    }

    // Decode an Rpc response.
    fn decode_response(
        &mut self,
        src: &mut BytesMut,
    ) -> Result<Option<RpcSuccessResponse>, RPCError> {
        let Some(length) = handle_length(&mut self.inner, &mut self.len, src)? else {
            return Ok(None);
        };

        // Should not attempt to decode rpc chunks with `length > max_packet_size` or not within bounds of
        // packet size for ssz container corresponding to `self.protocol`.
        let ssz_limits = self.protocol.rpc_response_limits();
        if ssz_limits.is_out_of_bounds(length, self.max_packet_size) {
            return Err(RPCError::InvalidData(format!(
                "RPC response length is out of bounds, length {}, max {}, min {}, max_packet_size: {}",
                length, ssz_limits.max, ssz_limits.min, self.max_packet_size,
            )));
        }
        // Calculate worst case compression length for given uncompressed length
        let max_compressed_len = snap::raw::max_compress_len(length) as u64;
        // Create a limit reader as a wrapper that reads only upto `max_compressed_len` from `src`.
        let limit_reader = Cursor::new(src.as_ref()).take(max_compressed_len);
        let mut reader = FrameDecoder::new(limit_reader);

        let mut decoded_buffer = vec![0; length];

        match reader.read_exact(&mut decoded_buffer) {
            Ok(()) => {
                // `n` is how many bytes the reader read in the compressed stream
                let n = reader.get_ref().get_ref().position();
                self.len = None;
                let _read_bytes = src.split_to(n as usize);
                handle_rpc_response(self.protocol.versioned_protocol, &decoded_buffer)
            }
            Err(e) => handle_error(e, reader.get_ref().get_ref().position(), max_compressed_len),
        }
    }

    fn decode_error(&mut self, src: &mut BytesMut) -> Result<Option<ErrorType>, RPCError> {
        let Some(length) = handle_length(&mut self.inner, &mut self.len, src)? else {
            return Ok(None);
        };

        // Should not attempt to decode rpc chunks with `length > max_packet_size` or not within bounds of
        // packet size for ssz container corresponding to `ErrorType`.
        if length > self.max_packet_size || length > ERROR_TYPE_MAX || length < ERROR_TYPE_MIN {
            return Err(RPCError::InvalidData(format!(
                "RPC Error length is out of bounds, length {}",
                length
            )));
        }

        // Calculate worst case compression length for given uncompressed length
        let max_compressed_len = snap::raw::max_compress_len(length) as u64;
        // Create a limit reader as a wrapper that reads only upto `max_compressed_len` from `src`.
        let limit_reader = Cursor::new(src.as_ref()).take(max_compressed_len);
        let mut reader = FrameDecoder::new(limit_reader);
        let mut decoded_buffer = vec![0; length];
        match reader.read_exact(&mut decoded_buffer) {
            Ok(()) => {
                // `n` is how many bytes the reader read in the compressed stream
                let n = reader.get_ref().get_ref().position();
                self.len = None;
                let _read_bytes = src.split_to(n as usize);
                Ok(Some(ErrorType(ContiguousList::from_ssz_default(
                    &decoded_buffer,
                )?)))
            }
            Err(e) => handle_error(e, reader.get_ref().get_ref().position(), max_compressed_len),
        }
    }
}

// Encoder for outbound streams: Encodes RPC Requests to peers
impl Encoder<RequestType> for SSZSnappyOutboundCodec {
    type Error = RPCError;

    fn encode(&mut self, item: RequestType, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let bytes = match item {
            RequestType::Status(req) => req.to_ssz()?,
            RequestType::Goodbye(req) => req.to_ssz()?,
            RequestType::BlocksByRoot(r) => r.block_roots.to_ssz()?,
            RequestType::Ping(req) => req.to_ssz()?,
            // no metadata to encode
            RequestType::MetaData(_) => return Ok(()),
        };

        // SSZ encoded bytes should be within `max_packet_size`
        if bytes.len() > self.max_packet_size {
            return Err(RPCError::InternalError(
                "attempting to encode data > max_packet_size",
            ));
        }

        // Inserts the length prefix of the uncompressed bytes into dst
        // encoded as a unsigned varint
        self.inner
            .encode(bytes.len(), dst)
            .map_err(RPCError::from)?;

        let mut writer = FrameEncoder::new(Vec::new());
        writer.write_all(&bytes).map_err(RPCError::from)?;
        writer.flush().map_err(RPCError::from)?;

        // Write compressed bytes to `dst`
        dst.extend_from_slice(writer.get_ref());

        let _count: u64 = dst
            .len()
            .try_into()
            .map_err(|_| RPCError::InvalidData("byte count does not fit in u64".into()))?;

        Ok(())
    }
}

// Decoder for outbound streams: Decodes RPC responses from peers.
//
// The majority of the decoding has now been pushed upstream due to the changing specification.
// We prefer to decode blocks and attestations with extra knowledge about the chain to perform
// faster verification checks before decoding entire blocks/attestations.
impl Decoder for SSZSnappyOutboundCodec {
    type Item = RpcResponse;
    type Error = RPCError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // if we have only received the response code, wait for more bytes
        if src.len() <= 1 {
            return Ok(None);
        }

        let _count: u64 = src
            .len()
            .try_into()
            .map_err(|_| RPCError::InvalidData("byte count does not fit in u64".into()))?;

        // using the response code determine which kind of payload needs to be decoded.
        let response_code = self.current_response_code.unwrap_or_else(|| {
            let resp_code = src.split_to(1)[0];
            self.current_response_code = Some(resp_code);
            resp_code
        });

        let inner_result = {
            if RpcResponse::is_response(response_code) {
                // decode an actual response and mutates the buffer if enough bytes have been read
                // returning the result.
                self.decode_response(src)
                    .map(|r| r.map(RpcResponse::Success))
            } else {
                // decode an error
                self.decode_error(src)
                    .map(|r| r.map(|resp| RpcResponse::from_error(response_code, resp)))
            }
        };
        // if the inner decoder was capable of decoding a chunk, we need to reset the current
        // response code for the next chunk
        if let Ok(Some(_)) = inner_result {
            self.current_response_code = None;
        }
        // return the result
        inner_result
    }
}

/// Handle errors that we get from decoding an RPC message from the stream.
/// `num_bytes_read` is the number of bytes the snappy decoder has read from the underlying stream.
/// `max_compressed_len` is the maximum compressed size for a given uncompressed size.
fn handle_error<T>(
    err: std::io::Error,
    num_bytes: u64,
    max_compressed_len: u64,
) -> Result<Option<T>, RPCError> {
    match err.kind() {
        ErrorKind::UnexpectedEof => {
            // If snappy has read `max_compressed_len` from underlying stream and still can't fill buffer, we have a malicious message.
            // Report as `InvalidData` so that malicious peer gets banned.
            if num_bytes >= max_compressed_len {
                Err(RPCError::InvalidData(format!(
                    "Received malicious snappy message, num_bytes {}, max_compressed_len {}",
                    num_bytes, max_compressed_len
                )))
            } else {
                // Haven't received enough bytes to decode yet, wait for more
                Ok(None)
            }
        }
        _ => Err(RPCError::from(err)),
    }
}


/// Decodes the length-prefix from the bytes as an unsigned protobuf varint.
///
/// Returns `Ok(Some(length))` by decoding the bytes if required.
/// Returns `Ok(None)` if more bytes are needed to decode the length-prefix.
/// Returns an `RPCError` for a decoding error.
fn handle_length(
    uvi_codec: &mut Uvi<usize>,
    len: &mut Option<usize>,
    bytes: &mut BytesMut,
) -> Result<Option<usize>, RPCError> {
    if let Some(length) = len {
        Ok(Some(*length))
    } else {
        // Decode the length of the uncompressed bytes from an unsigned varint
        // Note: length-prefix of > 10 bytes(uint64) would be a decoding error
        match uvi_codec.decode(bytes).map_err(RPCError::from)? {
            Some(length) => {
                *len = Some(length);
                Ok(Some(length))
            }
            None => Ok(None), // need more bytes to decode length
        }
    }
}

/// Decodes an `InboundRequest` from the byte stream.
/// `decoded_buffer` should be an ssz-encoded bytestream with
// length = length-prefix received in the beginning of the stream.
fn handle_rpc_request(
    config: &ChainConfig,
    versioned_protocol: SupportedProtocol,
    decoded_buffer: &[u8],
) -> Result<Option<RequestType>, RPCError> {
    match versioned_protocol {
        SupportedProtocol::StatusV1 | SupportedProtocol::StatusV2 => {
            Ok(Some(RequestType::Status(
                Status::from_ssz_default(decoded_buffer)?,
            )))
        }
        SupportedProtocol::GoodbyeV1 => Ok(Some(RequestType::Goodbye(
            GoodbyeReason::from_ssz_default(decoded_buffer)?,
        ))),
        SupportedProtocol::BlocksByRootV1 | SupportedProtocol::BlocksByRootV2 => {
            Ok(Some(RequestType::BlocksByRoot(BlocksByRootRequest {
                block_roots: DynamicList::from_ssz(
                    &(config.max_request_blocks as usize),
                    decoded_buffer,
                )?,
            })))
        }
        SupportedProtocol::PingV1 => Ok(Some(RequestType::Ping(Ping {
            data: u64::from_ssz_default(decoded_buffer)?,
        }))),
        // MetaData requests return early from InboundUpgrade and do not reach the decoder.
        // Handle this case just for completeness.
        SupportedProtocol::MetaDataV2 => {
            if !decoded_buffer.is_empty() {
                Err(RPCError::InternalError(
                    "Metadata requests shouldn't reach decoder",
                ))
            } else {
                Ok(Some(RequestType::MetaData(MetadataRequest::new_v2())))
            }
        }
        SupportedProtocol::MetaDataV1 => {
            if !decoded_buffer.is_empty() {
                Err(RPCError::InvalidData("Metadata request".to_string()))
            } else {
                Ok(Some(RequestType::MetaData(MetadataRequest::new_v1())))
            }
        }
    }
}

/// Decodes a `RPCResponse` from the byte stream.
/// `decoded_buffer` should be an ssz-encoded bytestream with
/// length = length-prefix received in the beginning of the stream.
fn handle_rpc_response(
    versioned_protocol: SupportedProtocol,
    decoded_buffer: &[u8],
) -> Result<Option<RpcSuccessResponse>, RPCError> {
    match versioned_protocol {
        SupportedProtocol::StatusV1 | SupportedProtocol::StatusV2 => {
            Ok(Some(RpcSuccessResponse::Status(
                Status::from_ssz_default(decoded_buffer)?,
            )))
        }
        // This case should be unreachable as `Goodbye` has no response.
        SupportedProtocol::GoodbyeV1 => Err(RPCError::InvalidData(
            "Goodbye RPC message has no valid response".to_string(),
        )),
        SupportedProtocol::BlocksByRootV1 | SupportedProtocol::BlocksByRootV2 => {
            Ok(Some(RpcSuccessResponse::BlocksByRoot(Arc::new(
                SignedBlockWithAttestation::from_ssz_default(decoded_buffer)?,
            ))))
        }
        SupportedProtocol::PingV1 => Ok(Some(RpcSuccessResponse::Pong(Ping {
            data: u64::from_ssz_default(decoded_buffer)?,
        }))),
        SupportedProtocol::MetaDataV1 => Ok(Some(RpcSuccessResponse::MetaData(Arc::new(
            MetaData::V1(MetaDataV1::from_ssz_default(decoded_buffer)?),
        )))),
        SupportedProtocol::MetaDataV2 => Ok(Some(RpcSuccessResponse::MetaData(Arc::new(
            MetaData::V2(MetaDataV2::from_ssz_default(decoded_buffer)?),
        )))),
    }
}


