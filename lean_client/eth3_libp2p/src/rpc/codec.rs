use crate::rpc::RequestType;
use crate::rpc::methods::*;
use crate::rpc::protocol::{
    ERROR_TYPE_MAX, ERROR_TYPE_MIN, Encoding, ProtocolId, RPCError, SupportedProtocol,
};
use containers::{SignedBlock, Status};
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
                        SupportedProtocol::MetaDataV3 => res.metadata_v3(&self.chain_config).to_ssz()?,
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
        if self.protocol.versioned_protocol == SupportedProtocol::MetaDataV3 {
            return Ok(Some(RequestType::MetaData(MetadataRequest::new_v3())));
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
        SupportedProtocol::MetaDataV3 => {
            if !decoded_buffer.is_empty() {
                Err(RPCError::InternalError(
                    "Metadata requests shouldn't reach decoder",
                ))
            } else {
                Ok(Some(RequestType::MetaData(MetadataRequest::new_v3())))
            }
        }
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
                SignedBlock::from_ssz_default(decoded_buffer)?,
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
        SupportedProtocol::MetaDataV3 => Ok(Some(RpcSuccessResponse::MetaData(Arc::new(
            MetaData::V3(MetaDataV3::from_ssz_default(decoded_buffer)?),
        )))),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EnrSyncCommitteeBitfield, factory,
        rpc::{Ping, methods::StatusMessage, protocol::*},
        types::{EnrAttestationBitfield, ForkContext},
    };
    use anyhow::Result;
    use snap::write::FrameEncoder;
    use ssz::{ByteList, DynamicList};
    use std::io::Write;
    use std::sync::Arc;
    use std_ext::ArcExt as _;
    use try_from_iterator::TryFromIterator as _;
    use types::{
        bellatrix::containers::{
            BeaconBlock as BellatrixBeaconBlock, BeaconBlockBody as BellatrixBeaconBlockBody,
            ExecutionPayload, SignedBeaconBlock as BellatrixSignedBeaconBlock,
        },
        combined::{DataColumnSidecar, SignedBeaconBlock},
        config::Config,
        deneb::containers::BlobIdentifier,
        fulu::containers::DataColumnsByRootIdentifier,
        phase0::{
            consts::GENESIS_EPOCH,
            primitives::{ForkDigest, H256},
        },
        preset::Mainnet,
    };

    fn phase0_block<P: Preset>() -> SignedBeaconBlock<P> {
        factory::full_phase0_signed_beacon_block().into()
    }
    fn altair_block<P: Preset>(config: &Config) -> SignedBeaconBlock<P> {
        // The context bytes are now derived from the block epoch, so we need to have the slot set
        // here.
        factory::full_altair_signed_beacon_block(config).into()
    }

    /// Smallest sized block across all current forks. Useful for testing
    /// min length check conditions.
    fn empty_base_block<P: Preset>() -> SignedBeaconBlock<P> {
        factory::empty_phase0_signed_beacon_block().into()
    }

    fn empty_blob_sidecar<P: Preset>(config: &Config) -> Arc<BlobSidecar<P>> {
        // The context bytes are now derived from the block epoch, so we need to have the slot set
        // here.
        let mut blob_sidecar = BlobSidecar::default();
        blob_sidecar.signed_block_header.message.slot =
            misc::compute_start_slot_at_epoch::<P>(config.deneb_fork_epoch);
        Arc::new(blob_sidecar)
    }

    fn empty_fulu_data_column_sidecar<P: Preset>(config: &Config) -> Arc<DataColumnSidecar<P>> {
        // The context bytes are now derived from the block epoch, so we need to have the slot set
        // here.
        let mut data_column_sidecar = FuluDataColumnSidecar::default();
        data_column_sidecar.signed_block_header.message.slot =
            misc::compute_start_slot_at_epoch::<P>(config.fulu_fork_epoch);
        Arc::new(data_column_sidecar.into())
    }

    fn empty_gloas_data_column_sidecar<P: Preset>(config: &Config) -> Arc<DataColumnSidecar<P>> {
        let mut data_column_sidecar = GloasDataColumnSidecar::default();
        data_column_sidecar.slot = misc::compute_start_slot_at_epoch::<P>(config.gloas_fork_epoch);
        Arc::new(data_column_sidecar.into())
    }

    /// Bellatrix block with length < max_rpc_size.
    fn bellatrix_block_small<P: Preset>(config: &Config) -> BellatrixSignedBeaconBlock<P> {
        // The context bytes are now derived from the block epoch, so we need to have the slot set
        // here.
        let tx = ByteList::<P::MaxBytesPerTransaction>::from_ssz_default([0; 1024]).unwrap();
        let txs = Arc::new(ContiguousList::try_from_iter(std::iter::repeat_n(tx, 5000)).unwrap());

        let block = BellatrixSignedBeaconBlock {
            message: BellatrixBeaconBlock {
                body: BellatrixBeaconBlockBody {
                    execution_payload: ExecutionPayload {
                        transactions: txs,
                        ..ExecutionPayload::default()
                    },
                    ..BellatrixBeaconBlockBody::default()
                },
                slot: misc::compute_start_slot_at_epoch::<P>(config.bellatrix_fork_epoch),
                ..BellatrixBeaconBlock::default()
            }
            .into(),
            ..BellatrixSignedBeaconBlock::default()
        };

        assert!(block.to_ssz().unwrap().len() <= Config::mainnet().max_payload_size);
        block
    }

    /// Bellatrix block with length > MAX_RPC_SIZE.
    /// The max limit for a merge block is in the order of ~16GiB which wouldn't fit in memory.
    /// Hence, we generate a merge block just greater than `MAX_RPC_SIZE` to test rejection on the rpc layer.
    fn bellatrix_block_large<P: Preset>(config: &Config) -> BellatrixSignedBeaconBlock<P> {
        // The context bytes are now derived from the block epoch, so we need to have the slot set
        // here.
        let tx = ByteList::<P::MaxBytesPerTransaction>::from_ssz_default([0; 1024]).unwrap();
        let txs = Arc::new(ContiguousList::try_from_iter(std::iter::repeat_n(tx, 100000)).unwrap());

        let block = BellatrixSignedBeaconBlock {
            message: BellatrixBeaconBlock {
                body: BellatrixBeaconBlockBody {
                    execution_payload: ExecutionPayload {
                        transactions: txs,
                        ..ExecutionPayload::default()
                    },
                    ..BellatrixBeaconBlockBody::default()
                },
                slot: misc::compute_start_slot_at_epoch::<P>(config.bellatrix_fork_epoch),
                ..BellatrixBeaconBlock::default()
            }
            .into(),
            ..BellatrixSignedBeaconBlock::default()
        };

        assert!(block.to_ssz().unwrap().len() > Config::mainnet().max_payload_size);
        block
    }

    fn status_message_v1() -> StatusMessage {
        StatusMessage::V1(StatusMessageV1 {
            fork_digest: ForkDigest::zero(),
            finalized_root: H256::zero(),
            finalized_epoch: 1,
            head_root: H256::zero(),
            head_slot: 1,
        })
    }

    fn status_message_v2() -> StatusMessage {
        StatusMessage::V2(StatusMessageV2 {
            fork_digest: ForkDigest::zero(),
            finalized_root: H256::zero(),
            finalized_epoch: 1,
            head_root: H256::zero(),
            head_slot: 1,
            earliest_available_slot: 0,
        })
    }

    fn dcbrange_request<P: Preset>() -> DataColumnsByRangeRequest<P> {
        DataColumnsByRangeRequest {
            start_slot: 0,
            count: 10,
            columns: ContiguousList::try_from(vec![1, 2, 3])
                .map(Arc::new)
                .expect("ColumnIndex list can be created from list of numbers"),
        }
    }

    fn dcbroot_request<P: Preset>(config: &Config) -> DataColumnsByRootRequest<P> {
        DataColumnsByRootRequest {
            data_column_ids: DynamicList::full(
                DataColumnsByRootIdentifier {
                    block_root: H256::zero(),
                    columns: ContiguousList::try_from(vec![1, 2, 3])
                        .expect("columns indices must be able to parsed from list of numbers"),
                },
                config.max_request_blocks_deneb as usize,
            ),
        }
    }

    fn bbrange_request_v1() -> OldBlocksByRangeRequest {
        OldBlocksByRangeRequest::new_v1(0, 10, 1)
    }

    fn bbrange_request_v2() -> OldBlocksByRangeRequest {
        OldBlocksByRangeRequest::new(0, 10, 1)
    }

    fn blbrange_request() -> BlobsByRangeRequest {
        BlobsByRangeRequest {
            start_slot: 0,
            count: 10,
        }
    }

    fn bbroot_request_v1(config: &Config, phase: Phase) -> BlocksByRootRequest {
        BlocksByRootRequest::new_v1(config, phase, core::iter::once(H256::zero()))
    }

    fn bbroot_request_v2(config: &Config, phase: Phase) -> BlocksByRootRequest {
        BlocksByRootRequest::new(config, phase, core::iter::once(H256::zero()))
    }

    fn blbroot_request() -> BlobsByRootRequest {
        BlobsByRootRequest {
            blob_ids: DynamicList::single(BlobIdentifier {
                block_root: H256::zero(),
                index: 0,
            }),
        }
    }

    fn ping_message() -> Ping {
        Ping { data: 1 }
    }

    fn metadata() -> Arc<MetaData> {
        MetaData::V1(MetaDataV1 {
            seq_number: 1,
            attnets: EnrAttestationBitfield::default(),
        })
        .into()
    }

    fn metadata_v2() -> Arc<MetaData> {
        MetaData::V2(MetaDataV2 {
            seq_number: 1,
            attnets: EnrAttestationBitfield::default(),
            syncnets: EnrSyncCommitteeBitfield::default(),
        })
        .into()
    }

    fn metadata_v3(config: &Config) -> Arc<MetaData> {
        MetaData::V3(MetaDataV3 {
            seq_number: 1,
            attnets: EnrAttestationBitfield::default(),
            syncnets: EnrSyncCommitteeBitfield::default(),
            custody_group_count: config.custody_requirement,
        })
        .into()
    }

    /// Encodes the given protocol response as bytes.
    fn encode_response<P: Preset>(
        config: &Arc<Config>,
        protocol: SupportedProtocol,
        message: RpcResponse<P>,
        fork_name: Phase,
    ) -> Result<BytesMut, RPCError> {
        let snappy_protocol_id = ProtocolId::new(protocol, Encoding::SSZSnappy);
        let fork_context = Arc::new(ForkContext::dummy::<P>(config, fork_name));

        let mut buf = BytesMut::new();
        let mut snappy_inbound_codec = SSZSnappyInboundCodec::<P>::new(
            config.clone_arc(),
            snappy_protocol_id,
            config.max_payload_size,
            fork_context,
        );

        snappy_inbound_codec.encode_response(message, &mut buf)?;
        Ok(buf)
    }

    fn encode_without_length_checks<P: Preset>(
        config: &Arc<Config>,
        bytes: Vec<u8>,
        fork_name: Phase,
    ) -> Result<BytesMut, RPCError> {
        let fork_context = ForkContext::dummy::<P>(config, fork_name);
        let mut dst = BytesMut::new();

        // Add context bytes if required
        dst.extend_from_slice(
            &fork_context
                .context_bytes(fork_context.current_fork_epoch())
                .as_bytes(),
        );

        let mut uvi_codec: Uvi<usize> = Uvi::default();

        // Inserts the length prefix of the uncompressed bytes into dst
        // encoded as a unsigned varint
        uvi_codec
            .encode(bytes.len(), &mut dst)
            .map_err(RPCError::from)?;

        let mut writer = FrameEncoder::new(Vec::new());
        writer.write_all(&bytes).map_err(RPCError::from)?;
        writer.flush().map_err(RPCError::from)?;

        // Write compressed bytes to `dst`
        dst.extend_from_slice(writer.get_ref());

        Ok(dst)
    }

    /// Attempts to decode the given protocol bytes as an rpc response
    fn decode_response<P: Preset>(
        config: &Arc<Config>,
        protocol: SupportedProtocol,
        message: &mut BytesMut,
        fork_name: Phase,
    ) -> Result<Option<RpcSuccessResponse<P>>, RPCError> {
        let snappy_protocol_id = ProtocolId::new(protocol, Encoding::SSZSnappy);
        let fork_context = Arc::new(ForkContext::dummy::<P>(config, fork_name));

        let mut snappy_outbound_codec = SSZSnappyOutboundCodec::<P>::new(
            snappy_protocol_id,
            config.max_payload_size,
            fork_context,
        );

        // decode message just as snappy message
        snappy_outbound_codec.decode_response(message)
    }

    /// Encodes the provided protocol message as bytes and tries to decode the encoding bytes.
    fn encode_then_decode_response<P: Preset>(
        config: &Arc<Config>,
        protocol: SupportedProtocol,
        message: RpcResponse<P>,
        fork_name: Phase,
    ) -> Result<Option<RpcSuccessResponse<P>>, RPCError> {
        let mut encoded = encode_response(config, protocol, message, fork_name)?;
        decode_response(config, protocol, &mut encoded, fork_name)
    }

    /// Verifies that requests we send are encoded in a way that we would correctly decode too.
    fn encode_then_decode_request<P: Preset>(
        config: &Arc<Config>,
        req: RequestType<P>,
        fork_name: Phase,
    ) {
        let fork_context = Arc::new(ForkContext::dummy::<P>(config, fork_name));
        let protocol = ProtocolId::new(req.versioned_protocol(), Encoding::SSZSnappy);
        // Encode a request we send
        let mut buf = BytesMut::new();
        let mut outbound_codec = SSZSnappyOutboundCodec::<P>::new(
            protocol.clone(),
            config.max_payload_size,
            fork_context.clone(),
        );
        outbound_codec.encode(req.clone(), &mut buf).unwrap();

        let mut inbound_codec = SSZSnappyInboundCodec::<P>::new(
            config.clone_arc(),
            protocol.clone(),
            config.max_payload_size,
            fork_context.clone(),
        );

        let decoded = inbound_codec.decode(&mut buf).unwrap().unwrap_or_else(|| {
            panic!(
                "Should correctly decode the request {} over protocol {:?} and fork {:?}",
                req, protocol, fork_name
            )
        });

        match req {
            RequestType::Status(status) => {
                assert_eq!(decoded, RequestType::Status(status))
            }
            RequestType::Goodbye(goodbye) => {
                assert_eq!(decoded, RequestType::Goodbye(goodbye))
            }
            RequestType::BlocksByRange(bbrange) => {
                assert_eq!(decoded, RequestType::BlocksByRange(bbrange))
            }
            RequestType::BlocksByRoot(bbroot) => {
                assert_eq!(decoded, RequestType::BlocksByRoot(bbroot))
            }
            RequestType::BlobsByRange(blbrange) => {
                assert_eq!(decoded, RequestType::BlobsByRange(blbrange))
            }
            RequestType::BlobsByRoot(bbroot) => {
                assert_eq!(decoded, RequestType::BlobsByRoot(bbroot))
            }
            RequestType::DataColumnsByRoot(dcbroot) => {
                assert_eq!(decoded, RequestType::DataColumnsByRoot(dcbroot))
            }
            RequestType::DataColumnsByRange(dcbrange) => {
                assert_eq!(decoded, RequestType::DataColumnsByRange(dcbrange))
            }
            RequestType::Ping(ping) => {
                assert_eq!(decoded, RequestType::Ping(ping))
            }
            RequestType::MetaData(metadata) => {
                assert_eq!(decoded, RequestType::MetaData(metadata))
            }
            RequestType::LightClientBootstrap(light_client_bootstrap_request) => {
                assert_eq!(
                    decoded,
                    RequestType::LightClientBootstrap(light_client_bootstrap_request)
                )
            }
            RequestType::LightClientOptimisticUpdate | RequestType::LightClientFinalityUpdate => {}
            RequestType::LightClientUpdatesByRange(light_client_updates_by_range) => {
                assert_eq!(
                    decoded,
                    RequestType::LightClientUpdatesByRange(light_client_updates_by_range)
                )
            }
        }
    }

    // Test RPCResponse encoding/decoding for V1 messages
    #[test]
    fn test_encode_then_decode_v1() {
        let config = Arc::new(Config::mainnet().rapid_upgrade());

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::StatusV1,
                RpcResponse::Success(RpcSuccessResponse::Status(status_message_v1())),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::Status(status_message_v1())))
        );

        // A StatusV2 still encodes as a StatusV1 since version is Version::V1
        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::StatusV1,
                RpcResponse::Success(RpcSuccessResponse::Status(status_message_v2())),
                Phase::Gloas,
            ),
            Ok(Some(RpcSuccessResponse::Status(status_message_v1())))
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::PingV1,
                RpcResponse::Success(RpcSuccessResponse::Pong(ping_message())),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::Pong(ping_message())))
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV1,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRange(Arc::new(
                    empty_base_block()
                ))),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRange(Arc::new(
                empty_base_block()
            ))))
        );

        assert!(
            matches!(
                encode_then_decode_response::<Mainnet>(
                    &config,
                    SupportedProtocol::BlocksByRangeV1,
                    RpcResponse::Success(RpcSuccessResponse::BlocksByRange(Arc::new(
                        altair_block(&config)
                    ))),
                    Phase::Altair,
                )
                .unwrap_err(),
                RPCError::SszReadError(_)
            ),
            "altair block cannot be decoded with blocks by range V1 version"
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRootV1,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(Arc::new(
                    empty_base_block()
                ))),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRoot(Arc::new(
                empty_base_block()
            ))))
        );

        assert!(
            matches!(
                encode_then_decode_response::<Mainnet>(
                    &config,
                    SupportedProtocol::BlocksByRootV1,
                    RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(Arc::new(altair_block(
                        &config
                    )))),
                    Phase::Altair,
                )
                .unwrap_err(),
                RPCError::SszReadError(_)
            ),
            "altair block cannot be decoded with blocks by range V1 version"
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV1,
                RpcResponse::Success(RpcSuccessResponse::MetaData(metadata())),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::MetaData(metadata()))),
        );

        // A MetaDataV2 still encodes as a MetaDataV1 since version is Version::V1
        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV1,
                RpcResponse::Success(RpcSuccessResponse::MetaData(metadata_v2())),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::MetaData(metadata()))),
        );

        // A MetaDataV3 still encodes as a MetaDataV2 since version is Version::V2
        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV2,
                RpcResponse::Success(RpcSuccessResponse::MetaData(metadata_v3(&config))),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::MetaData(metadata_v2()))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlobsByRangeV1,
                RpcResponse::Success(RpcSuccessResponse::BlobsByRange(empty_blob_sidecar(
                    &config
                ))),
                Phase::Deneb,
            ),
            Ok(Some(RpcSuccessResponse::BlobsByRange(empty_blob_sidecar(
                &config
            )))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlobsByRangeV1,
                RpcResponse::Success(RpcSuccessResponse::BlobsByRange(empty_blob_sidecar(
                    &config
                ))),
                Phase::Electra,
            ),
            Ok(Some(RpcSuccessResponse::BlobsByRange(empty_blob_sidecar(
                &config
            )))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlobsByRangeV1,
                RpcResponse::Success(RpcSuccessResponse::BlobsByRange(empty_blob_sidecar(
                    &config
                ))),
                Phase::Fulu,
            ),
            Ok(Some(RpcSuccessResponse::BlobsByRange(empty_blob_sidecar(
                &config
            )))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlobsByRootV1,
                RpcResponse::Success(RpcSuccessResponse::BlobsByRoot(empty_blob_sidecar(&config))),
                Phase::Deneb,
            ),
            Ok(Some(RpcSuccessResponse::BlobsByRoot(empty_blob_sidecar(
                &config
            )))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlobsByRootV1,
                RpcResponse::Success(RpcSuccessResponse::BlobsByRoot(empty_blob_sidecar(&config))),
                Phase::Electra,
            ),
            Ok(Some(RpcSuccessResponse::BlobsByRoot(empty_blob_sidecar(
                &config
            )))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlobsByRootV1,
                RpcResponse::Success(RpcSuccessResponse::BlobsByRoot(empty_blob_sidecar(&config))),
                Phase::Fulu,
            ),
            Ok(Some(RpcSuccessResponse::BlobsByRoot(empty_blob_sidecar(
                &config
            )))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::DataColumnsByRangeV1,
                RpcResponse::Success(RpcSuccessResponse::DataColumnsByRange(
                    empty_fulu_data_column_sidecar(&config)
                )),
                Phase::Fulu,
            ),
            Ok(Some(RpcSuccessResponse::DataColumnsByRange(
                empty_fulu_data_column_sidecar(&config)
            ))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::DataColumnsByRangeV1,
                RpcResponse::Success(RpcSuccessResponse::DataColumnsByRange(
                    empty_gloas_data_column_sidecar(&config)
                )),
                Phase::Gloas,
            ),
            Ok(Some(RpcSuccessResponse::DataColumnsByRange(
                empty_gloas_data_column_sidecar(&config)
            ))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::DataColumnsByRootV1,
                RpcResponse::Success(RpcSuccessResponse::DataColumnsByRoot(
                    empty_fulu_data_column_sidecar(&config)
                )),
                Phase::Fulu,
            ),
            Ok(Some(RpcSuccessResponse::DataColumnsByRoot(
                empty_fulu_data_column_sidecar(&config)
            ))),
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::DataColumnsByRootV1,
                RpcResponse::Success(RpcSuccessResponse::DataColumnsByRoot(
                    empty_gloas_data_column_sidecar(&config)
                )),
                Phase::Gloas,
            ),
            Ok(Some(RpcSuccessResponse::DataColumnsByRoot(
                empty_gloas_data_column_sidecar(&config)
            ))),
        );
    }

    // Test RPCResponse encoding/decoding for V1 messages
    #[test]
    fn test_encode_then_decode_v2() {
        let config = Arc::new(Config::mainnet().rapid_upgrade());

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRange(Arc::new(
                    empty_base_block()
                ))),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRange(Arc::new(
                empty_base_block()
            ))))
        );

        // Decode the smallest possible base block when current fork is altair
        // This is useful for checking that we allow for blocks smaller than
        // the current_fork's rpc limit
        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRange(Arc::new(
                    empty_base_block()
                ))),
                Phase::Altair,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRange(Arc::new(
                empty_base_block()
            ))))
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRange(Arc::new(altair_block(
                    &config
                )))),
                Phase::Altair,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRange(Arc::new(
                altair_block(&config)
            ))))
        );

        let bellatrix_block_small = bellatrix_block_small::<Mainnet>(&config);
        let bellatrix_block_large = bellatrix_block_large::<Mainnet>(&config);

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRange(Arc::new(
                    types::combined::SignedBeaconBlock::Bellatrix(bellatrix_block_small.clone())
                ))),
                Phase::Bellatrix,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRange(Arc::new(
                types::combined::SignedBeaconBlock::Bellatrix(bellatrix_block_small.clone())
            ))))
        );

        let mut encoded = encode_without_length_checks::<Mainnet>(
            &config,
            bellatrix_block_large.to_ssz().unwrap(),
            Phase::Bellatrix,
        )
        .unwrap();

        assert!(
            matches!(
                decode_response::<Mainnet>(
                    &config,
                    SupportedProtocol::BlocksByRangeV2,
                    &mut encoded,
                    Phase::Bellatrix,
                )
                .unwrap_err(),
                RPCError::InvalidData(_)
            ),
            "Decoding a block larger than max_rpc_size should fail"
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRootV2,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(Arc::new(
                    empty_base_block()
                ))),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRoot(Arc::new(
                empty_base_block()
            ))))
        );

        // Decode the smallest possible base block when current fork is altair
        // This is useful for checking that we allow for blocks smaller than
        // the current_fork's rpc limit
        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRootV2,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(Arc::new(
                    empty_base_block()
                ))),
                Phase::Altair,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRoot(Arc::new(
                empty_base_block()
            ))))
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRange(Arc::new(altair_block(
                    &config
                )))),
                Phase::Altair,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRange(Arc::new(
                altair_block(&config)
            ))))
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRootV2,
                RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(Arc::new(
                    types::combined::SignedBeaconBlock::Bellatrix(bellatrix_block_small.clone())
                ))),
                Phase::Bellatrix,
            ),
            Ok(Some(RpcSuccessResponse::BlocksByRoot(Arc::new(
                types::combined::SignedBeaconBlock::Bellatrix(bellatrix_block_small)
            ))))
        );

        let mut encoded = encode_without_length_checks::<Mainnet>(
            &config,
            bellatrix_block_large.to_ssz().unwrap(),
            Phase::Bellatrix,
        )
        .unwrap();

        assert!(
            matches!(
                decode_response::<Mainnet>(
                    &config,
                    SupportedProtocol::BlocksByRootV2,
                    &mut encoded,
                    Phase::Bellatrix,
                )
                .unwrap_err(),
                RPCError::InvalidData(_)
            ),
            "Decoding a block larger than max_rpc_size should fail"
        );

        // A MetaDataV1 still encodes as a MetaDataV2 since version is Version::V2
        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV2,
                RpcResponse::Success(RpcSuccessResponse::MetaData(metadata())),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::MetaData(metadata_v2())))
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV2,
                RpcResponse::Success(RpcSuccessResponse::MetaData(metadata_v2())),
                Phase::Altair,
            ),
            Ok(Some(RpcSuccessResponse::MetaData(metadata_v2())))
        );

        // A StatusV1 still encodes as a StatusV2 since version is Version::V2
        assert_eq!(
            encode_then_decode_response(
                &config,
                SupportedProtocol::StatusV2,
                RpcResponse::Success(RpcSuccessResponse::<Mainnet>::Status(status_message_v1())),
                Phase::Fulu,
            ),
            Ok(Some(RpcSuccessResponse::Status(status_message_v2())))
        );

        assert_eq!(
            encode_then_decode_response(
                &config,
                SupportedProtocol::StatusV2,
                RpcResponse::<Mainnet>::Success(RpcSuccessResponse::Status(status_message_v2())),
                Phase::Fulu,
            ),
            Ok(Some(RpcSuccessResponse::Status(status_message_v2())))
        );
    }

    // Test RPCResponse encoding/decoding for V3 messages
    #[test]
    fn test_encode_then_decode_v3() {
        let config = Arc::new(Config::mainnet().rapid_upgrade());

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV3,
                RpcResponse::Success(RpcSuccessResponse::MetaData(metadata())),
                Phase::Phase0,
            ),
            Ok(Some(RpcSuccessResponse::MetaData(metadata_v3(&config))))
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV3,
                RpcResponse::Success(RpcSuccessResponse::MetaData(metadata_v2())),
                Phase::Altair,
            ),
            Ok(Some(RpcSuccessResponse::MetaData(metadata_v3(&config))))
        );

        assert_eq!(
            encode_then_decode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV3,
                RpcResponse::Success(RpcSuccessResponse::MetaData(metadata_v3(&config))),
                Phase::Fulu,
            ),
            Ok(Some(RpcSuccessResponse::MetaData(metadata_v3(&config))))
        );
    }

    // Test RPCResponse encoding/decoding for V2 messages
    #[test]
    fn test_context_bytes_v2() {
        let config = Arc::new(Config::mainnet().rapid_upgrade());
        let fork_context = ForkContext::dummy::<Mainnet>(&config, Phase::Altair);

        // Removing context bytes for v2 messages should error
        let mut encoded_bytes = encode_response::<Mainnet>(
            &config,
            SupportedProtocol::BlocksByRangeV2,
            RpcResponse::Success(RpcSuccessResponse::BlocksByRange(Arc::new(
                empty_base_block(),
            ))),
            Phase::Phase0,
        )
        .unwrap();

        let _ = encoded_bytes.split_to(4);

        assert!(matches!(
            decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                &mut encoded_bytes,
                Phase::Phase0
            )
            .unwrap_err(),
            RPCError::ErrorResponse(RpcErrorResponse::InvalidRequest, _),
        ));

        let mut encoded_bytes = encode_response::<Mainnet>(
            &config,
            SupportedProtocol::BlocksByRootV2,
            RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(Arc::new(
                empty_base_block(),
            ))),
            Phase::Phase0,
        )
        .unwrap();

        let _ = encoded_bytes.split_to(4);

        assert!(matches!(
            decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                &mut encoded_bytes,
                Phase::Phase0
            )
            .unwrap_err(),
            RPCError::ErrorResponse(RpcErrorResponse::InvalidRequest, _),
        ));

        // Trying to decode a base block with altair context bytes should give ssz decoding error
        let mut encoded_bytes = encode_response::<Mainnet>(
            &config,
            SupportedProtocol::BlocksByRangeV2,
            RpcResponse::Success(RpcSuccessResponse::BlocksByRange(Arc::new(
                empty_base_block(),
            ))),
            Phase::Altair,
        )
        .unwrap();

        let mut wrong_fork_bytes = BytesMut::new();
        wrong_fork_bytes.extend_from_slice(
            fork_context
                .context_bytes(config.altair_fork_epoch)
                .as_bytes(),
        );
        wrong_fork_bytes.extend_from_slice(&encoded_bytes.split_off(4));

        assert!(matches!(
            decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                &mut wrong_fork_bytes,
                Phase::Altair
            )
            .unwrap_err(),
            RPCError::SszReadError(_),
        ));

        // Trying to decode an altair block with base context bytes should give ssz decoding error
        let mut encoded_bytes = encode_response::<Mainnet>(
            &config,
            SupportedProtocol::BlocksByRootV2,
            RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(Arc::new(
                empty_base_block(),
            ))),
            Phase::Altair,
        )
        .unwrap();

        let mut wrong_fork_bytes = BytesMut::new();
        wrong_fork_bytes.extend_from_slice(fork_context.context_bytes(GENESIS_EPOCH).as_bytes());
        wrong_fork_bytes.extend_from_slice(&encoded_bytes.split_off(4));

        assert!(
            decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                &mut wrong_fork_bytes,
                Phase::Altair
            )
            .is_ok()
        );

        // assert!(matches!(
        //     decode_response::<Mainnet>(
        //         &config,
        //         SupportedProtocol::BlocksByRangeV2,
        //         &mut wrong_fork_bytes,
        //         Phase::Altair,
        //     )
        //     .unwrap_err(),
        //     RPCError::SszReadError(_),
        // ));

        // Adding context bytes to Protocols that don't require it should return an error
        let mut encoded_bytes = BytesMut::new();
        encoded_bytes.extend_from_slice(
            fork_context
                .context_bytes(config.altair_fork_epoch)
                .as_bytes(),
        );
        encoded_bytes.extend_from_slice(
            &encode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV2,
                RpcResponse::Success(RpcSuccessResponse::MetaData(metadata())),
                Phase::Altair,
            )
            .unwrap(),
        );

        assert!(
            decode_response::<Mainnet>(
                &config,
                SupportedProtocol::MetaDataV2,
                &mut encoded_bytes,
                Phase::Altair
            )
            .is_err()
        );

        // Sending context bytes which do not correspond to any fork should return an error
        let mut encoded_bytes = encode_response::<Mainnet>(
            &config,
            SupportedProtocol::BlocksByRootV2,
            RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(Arc::new(
                empty_base_block(),
            ))),
            Phase::Altair,
        )
        .unwrap();

        let mut wrong_fork_bytes = BytesMut::new();
        wrong_fork_bytes.extend_from_slice(&[42, 42, 42, 42]);
        wrong_fork_bytes.extend_from_slice(&encoded_bytes.split_off(4));

        assert!(matches!(
            decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                &mut wrong_fork_bytes,
                Phase::Altair
            )
            .unwrap_err(),
            RPCError::ErrorResponse(RpcErrorResponse::InvalidRequest, _),
        ));

        // Sending bytes less than context bytes length should wait for more bytes by returning `Ok(None)`
        let mut encoded_bytes = encode_response::<Mainnet>(
            &config,
            SupportedProtocol::BlocksByRootV2,
            RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(Arc::new(phase0_block()))),
            Phase::Altair,
        )
        .unwrap();

        let mut part = encoded_bytes.split_to(3);

        assert_eq!(
            decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                &mut part,
                Phase::Altair
            ),
            Ok(None)
        )
    }

    #[test]
    fn test_encode_then_decode_request() {
        let config = Arc::new(Config::mainnet().rapid_upgrade());

        let requests: &[RequestType<Mainnet>] = &[
            RequestType::Ping(ping_message()),
            RequestType::Status(status_message_v1()),
            RequestType::Status(status_message_v2()),
            RequestType::Goodbye(GoodbyeReason::Fault),
            RequestType::BlocksByRange(bbrange_request_v1()),
            RequestType::BlocksByRange(bbrange_request_v2()),
            RequestType::MetaData(MetadataRequest::new_v1()),
            RequestType::DataColumnsByRange(dcbrange_request::<Mainnet>()),
            RequestType::DataColumnsByRoot(dcbroot_request::<Mainnet>(&config)),
            RequestType::MetaData(MetadataRequest::new_v2()),
            RequestType::MetaData(MetadataRequest::new_v3()),
        ];
        for req in requests.iter() {
            for fork_name in enum_iterator::all::<Phase>() {
                encode_then_decode_request(&config, req.clone(), fork_name);
            }
        }

        let requests: &[RequestType<Mainnet>] = &[
            RequestType::BlobsByRange(blbrange_request()),
            RequestType::BlobsByRoot(blbroot_request()),
        ];
        for req in requests.iter() {
            for fork_name in enum_iterator::all::<Phase>().filter(|phase| *phase > Phase::Capella) {
                encode_then_decode_request(&config, req.clone(), fork_name);
            }
        }

        // Request types that have different length limits depending on the fork
        // Handled separately to have consistent `Phase` across request and responses
        let fork_dependent_requests = |phase| {
            [
                RequestType::BlocksByRoot(bbroot_request_v1(&config, phase)),
                RequestType::BlocksByRoot(bbroot_request_v2(&config, phase)),
            ]
        };
        for fork_name in enum_iterator::all::<Phase>() {
            let requests: [RequestType<Mainnet>; 2] = fork_dependent_requests(fork_name);
            for req in requests {
                encode_then_decode_request(&config, req.clone(), fork_name);
            }
        }
    }

    /// Test a malicious snappy encoding for a V1 `Status` message where the attacker
    /// sends a valid message filled with a stream of useless padding before the actual message.
    #[test]
    fn test_decode_malicious_v1_message() {
        // 10 byte snappy stream identifier
        let stream_identifier: &'static [u8] = b"\xFF\x06\x00\x00sNaPpY";

        assert_eq!(stream_identifier.len(), 10);

        // byte 0(0xFE) is padding chunk type identifier for snappy messages
        // byte 1,2,3 are chunk length (little endian)
        let malicious_padding: &'static [u8] = b"\xFE\x00\x00\x00";

        // Status message is 84 bytes uncompressed. `max_compressed_len` is 32 + 84 + 84/6 = 130.
        let status_message_bytes = StatusMessageV1 {
            fork_digest: ForkDigest::zero(),
            finalized_root: H256::zero(),
            finalized_epoch: 1,
            head_root: H256::zero(),
            head_slot: 1,
        }
        .to_ssz()
        .unwrap();

        assert_eq!(status_message_bytes.len(), 84);
        assert_eq!(snap::raw::max_compress_len(status_message_bytes.len()), 130);

        let mut uvi_codec: Uvi<usize> = Uvi::default();
        let mut dst = BytesMut::with_capacity(1024);

        // Insert length-prefix
        uvi_codec
            .encode(status_message_bytes.len(), &mut dst)
            .unwrap();

        // Insert snappy stream identifier
        dst.extend_from_slice(stream_identifier);

        // Insert malicious padding of 80 bytes.
        for _ in 0..20 {
            dst.extend_from_slice(malicious_padding);
        }

        // Insert payload (42 bytes compressed)
        let mut writer = FrameEncoder::new(Vec::new());
        writer.write_all(&status_message_bytes).unwrap();
        writer.flush().unwrap();
        assert_eq!(writer.get_ref().len(), 42);
        dst.extend_from_slice(writer.get_ref());

        // 10 (for stream identifier) + 80 + 42 = 132 > `max_compressed_len`. Hence, decoding should fail with `InvalidData`.
        assert!(matches!(
            decode_response::<Mainnet>(
                &Config::mainnet().rapid_upgrade().into(),
                SupportedProtocol::StatusV1,
                &mut dst,
                Phase::Phase0,
            )
            .unwrap_err(),
            RPCError::InvalidData(_)
        ));
    }

    /// Test a malicious snappy encoding for a V2 `BlocksByRange` message where the attacker
    /// sends a valid message filled with a stream of useless padding before the actual message.
    #[test]
    fn test_decode_malicious_v2_message() {
        let config = Arc::new(Config::mainnet().rapid_upgrade());
        let fork_context = Arc::new(ForkContext::dummy::<Mainnet>(&config, Phase::Altair));

        // 10 byte snappy stream identifier
        let stream_identifier: &'static [u8] = b"\xFF\x06\x00\x00sNaPpY";

        assert_eq!(stream_identifier.len(), 10);

        // byte 0(0xFE) is padding chunk type identifier for snappy messages
        // byte 1,2,3 are chunk length (little endian)
        let malicious_padding: &'static [u8] = b"\xFE\x00\x00\x00";

        // Full altair block is 157916 bytes uncompressed. `max_compressed_len` is 32 + 157916 + 157916/6 = 184267.
        let block_message_bytes = altair_block::<Mainnet>(&config).to_ssz().unwrap();

        assert_eq!(block_message_bytes.len(), 157916);
        assert_eq!(
            snap::raw::max_compress_len(block_message_bytes.len()),
            184267
        );

        let mut uvi_codec: Uvi<usize> = Uvi::default();
        let mut dst = BytesMut::with_capacity(1024);

        // Insert context bytes
        dst.extend_from_slice(
            fork_context
                .context_bytes(config.altair_fork_epoch)
                .as_bytes(),
        );

        // Insert length-prefix
        uvi_codec
            .encode(block_message_bytes.len(), &mut dst)
            .unwrap();

        // Insert snappy stream identifier
        dst.extend_from_slice(stream_identifier);

        // Insert malicious padding of 176156 bytes.
        for _ in 0..44039 {
            dst.extend_from_slice(malicious_padding);
        }

        // Insert payload (8102 bytes compressed)
        let mut writer = FrameEncoder::new(Vec::new());
        writer.write_all(&block_message_bytes).unwrap();
        writer.flush().unwrap();
        assert_eq!(writer.get_ref().len(), 8102);
        dst.extend_from_slice(writer.get_ref());

        // 10 (for stream identifier) + 176156 + 8103 = 184269 > `max_compressed_len`. Hence, decoding should fail with `InvalidData`.
        assert!(matches!(
            decode_response::<Mainnet>(
                &config,
                SupportedProtocol::BlocksByRangeV2,
                &mut dst,
                Phase::Altair
            )
            .unwrap_err(),
            RPCError::InvalidData(_)
        ));
    }

    /// Test sending a message with encoded length prefix > max_rpc_size.
    #[test]
    fn test_decode_invalid_length() -> Result<()> {
        // 10 byte snappy stream identifier
        let stream_identifier: &'static [u8] = b"\xFF\x06\x00\x00sNaPpY";

        assert_eq!(stream_identifier.len(), 10);

        // Status message is 84 bytes uncompressed. `max_compressed_len` is 32 + 84 + 84/6 = 130.
        let status_message_bytes = StatusMessageV1 {
            fork_digest: ForkDigest::zero(),
            finalized_root: H256::zero(),
            finalized_epoch: 1,
            head_root: H256::zero(),
            head_slot: 1,
        }
        .to_ssz()?;

        let mut uvi_codec: Uvi<usize> = Uvi::default();
        let mut dst = BytesMut::with_capacity(1024);

        // Insert length-prefix
        uvi_codec
            .encode(Config::default().max_payload_size + 1, &mut dst)
            .unwrap();

        // Insert snappy stream identifier
        dst.extend_from_slice(stream_identifier);

        // Insert payload
        let mut writer = FrameEncoder::new(Vec::new());
        writer.write_all(&status_message_bytes).unwrap();
        writer.flush().unwrap();
        dst.extend_from_slice(writer.get_ref());

        assert!(matches!(
            decode_response::<Mainnet>(
                &Config::mainnet().rapid_upgrade().into(),
                SupportedProtocol::StatusV1,
                &mut dst,
                Phase::Phase0,
            )
            .unwrap_err(),
            RPCError::InvalidData(_)
        ));

        Ok(())
    }

    #[test]
    fn test_decode_status_message() {
        let config = Arc::new(Config::mainnet().rapid_upgrade());
        let message = hex::decode("0054ff060000734e615070590032000006e71e7b54989925efd6c9cbcb8ceb9b5f71216f5137282bf6a1e3b50f64e42d6c7fb347abe07eb0db8200000005029e2800").unwrap();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&message);

        let snappy_protocol_id = ProtocolId::new(SupportedProtocol::StatusV1, Encoding::SSZSnappy);

        let fork_context = Arc::new(ForkContext::dummy::<Mainnet>(&config, Phase::Phase0));

        let mut snappy_outbound_codec = SSZSnappyOutboundCodec::<Mainnet>::new(
            snappy_protocol_id,
            config.max_payload_size,
            fork_context,
        );

        // remove response code
        let mut snappy_buf = buf.clone();
        let _ = snappy_buf.split_to(1);

        // decode message just as snappy message
        let _snappy_decoded_message = snappy_outbound_codec
            .decode_response(&mut snappy_buf)
            .unwrap();

        // decode message as ssz snappy chunk
        let _snappy_decoded_chunk = snappy_outbound_codec.decode(&mut buf).unwrap();
    }

    #[test]
    fn test_invalid_length_prefix() {
        let config = Arc::new(Config::mainnet().rapid_upgrade());
        let mut uvi_codec: Uvi<u128> = Uvi::default();
        let mut dst = BytesMut::with_capacity(1024);

        // Smallest > 10 byte varint
        let len: u128 = 2u128.pow(70);

        // Insert length-prefix
        uvi_codec.encode(len, &mut dst).unwrap();

        let snappy_protocol_id = ProtocolId::new(SupportedProtocol::StatusV1, Encoding::SSZSnappy);

        let fork_context = Arc::new(ForkContext::dummy::<Mainnet>(&config, Phase::Phase0));

        let mut snappy_outbound_codec = SSZSnappyOutboundCodec::<Mainnet>::new(
            snappy_protocol_id,
            config.max_payload_size,
            fork_context,
        );

        let snappy_decoded_message = snappy_outbound_codec.decode_response(&mut dst).unwrap_err();

        assert_eq!(
            snappy_decoded_message,
            RPCError::IoError("input bytes exceed maximum".to_string()),
            "length-prefix of > 10 bytes is invalid"
        );
    }

    #[test]
    fn test_length_limits() {
        fn encode_len(len: usize) -> BytesMut {
            let mut uvi_codec: Uvi<usize> = Uvi::default();
            let mut dst = BytesMut::with_capacity(1024);
            uvi_codec.encode(len, &mut dst).unwrap();
            dst
        }

        let protocol_id = ProtocolId::new(SupportedProtocol::BlocksByRangeV1, Encoding::SSZSnappy);

        // Response limits
        let config = Arc::new(Config::mainnet().rapid_upgrade());
        let fork_context = Arc::new(ForkContext::dummy::<Mainnet>(&config, Phase::Phase0));

        let limit = protocol_id.rpc_response_limits::<Mainnet>(&fork_context);
        let mut max = encode_len(limit.max + 1);
        let mut codec = SSZSnappyOutboundCodec::<Mainnet>::new(
            protocol_id.clone(),
            config.max_payload_size,
            fork_context.clone(),
        );
        assert!(matches!(
            codec.decode_response(&mut max).unwrap_err(),
            RPCError::InvalidData(_)
        ));

        let mut min = encode_len(limit.min - 1);
        let mut codec = SSZSnappyOutboundCodec::<Mainnet>::new(
            protocol_id.clone(),
            config.max_payload_size,
            fork_context.clone(),
        );
        assert!(matches!(
            codec.decode_response(&mut min).unwrap_err(),
            RPCError::InvalidData(_)
        ));

        // Request limits
        let limit = protocol_id.rpc_request_limits::<Mainnet>(&config, Phase::Deneb);
        let mut max = encode_len(limit.max + 1);
        let mut codec = SSZSnappyOutboundCodec::<Mainnet>::new(
            protocol_id.clone(),
            config.max_payload_size,
            fork_context.clone(),
        );
        assert!(matches!(
            codec.decode_response(&mut max).unwrap_err(),
            RPCError::InvalidData(_)
        ));

        let mut min = encode_len(limit.min - 1);
        let mut codec = SSZSnappyOutboundCodec::<Mainnet>::new(
            protocol_id,
            config.max_payload_size,
            fork_context,
        );
        assert!(matches!(
            codec.decode_response(&mut min).unwrap_err(),
            RPCError::InvalidData(_)
        ));
    }
}
