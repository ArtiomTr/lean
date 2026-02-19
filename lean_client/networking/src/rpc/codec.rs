use std::io;
use std::io::{Read, Write};

use async_trait::async_trait;
use containers::{SignedBlockWithAttestation, Status};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::request_response::Codec;
use snap::read::FrameDecoder;
use snap::write::FrameEncoder;
use ssz::{H256, SszReadDefault as _, SszWrite as _};

use super::methods::{LeanRequest, LeanResponse, MAX_REQUEST_BLOCKS};
use super::protocol::LeanProtocol;

/// Maximum uncompressed payload size (10 MiB per leanSpec).
const MAX_PAYLOAD_SIZE: usize = 10 * 1024 * 1024;
const RESPONSE_CODE_SUCCESS: u8 = 0;

/// Codec implementing the lean ethereum req/resp wire format.
///
/// Wire formats per spec:
/// - Request:  `[varint: uncompressed_length][snappy_framed_payload]`
/// - Response: `[response_code: 1 byte][varint: uncompressed_length][snappy_framed_payload]`
#[derive(Clone, Default)]
pub struct LeanCodec;

impl LeanCodec {
    pub fn compress(data: &[u8]) -> io::Result<Vec<u8>> {
        let mut encoder = FrameEncoder::new(Vec::new());
        encoder.write_all(data)?;
        encoder
            .into_inner()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("snappy framing: {e}")))
    }

    fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
        let mut decoder = FrameDecoder::new(data);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed)?;
        Ok(decompressed)
    }

    pub fn encode_request(ssz: &[u8]) -> io::Result<Vec<u8>> {
        if ssz.len() > MAX_PAYLOAD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload too large: {} > {MAX_PAYLOAD_SIZE}", ssz.len()),
            ));
        }
        let compressed = Self::compress(ssz)?;
        let mut wire = encode_varint(ssz.len());
        wire.extend_from_slice(&compressed);
        Ok(wire)
    }

    pub fn decode_request_payload(data: &[u8]) -> io::Result<Vec<u8>> {
        if data.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty request"));
        }
        let (declared_len, varint_size) = decode_varint(data)?;
        if declared_len > MAX_PAYLOAD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("declared length too large: {declared_len} > {MAX_PAYLOAD_SIZE}"),
            ));
        }
        let decompressed = Self::decompress(&data[varint_size..])?;
        if decompressed.len() != declared_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "length mismatch: declared {declared_len}, got {}",
                    decompressed.len()
                ),
            ));
        }
        Ok(decompressed)
    }

    pub fn encode_response_chunk(ssz: &[u8]) -> io::Result<Vec<u8>> {
        if ssz.len() > MAX_PAYLOAD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload too large: {} > {MAX_PAYLOAD_SIZE}", ssz.len()),
            ));
        }
        let compressed = Self::compress(ssz)?;
        let mut wire = vec![RESPONSE_CODE_SUCCESS];
        wire.extend_from_slice(&encode_varint(ssz.len()));
        wire.extend_from_slice(&compressed);
        Ok(wire)
    }

    pub fn decode_response_chunk(data: &[u8]) -> io::Result<Vec<u8>> {
        if data.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response too short",
            ));
        }
        let response_code = data[0];
        if response_code != RESPONSE_CODE_SUCCESS {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("non-success response code: {response_code}"),
            ));
        }
        let (declared_len, varint_size) = decode_varint(&data[1..])?;
        if declared_len > MAX_PAYLOAD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("declared length too large: {declared_len} > {MAX_PAYLOAD_SIZE}"),
            ));
        }
        let decompressed = Self::decompress(&data[1 + varint_size..])?;
        if decompressed.len() != declared_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "length mismatch: declared {declared_len}, got {}",
                    decompressed.len()
                ),
            ));
        }
        Ok(decompressed)
    }

    fn encode_blocks(blocks: &[SignedBlockWithAttestation]) -> io::Result<Vec<u8>> {
        let mut payload = Vec::new();
        for block in blocks {
            let block_ssz = block.to_ssz().map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("SSZ encode block: {e}"))
            })?;
            payload.extend_from_slice(&encode_varint(block_ssz.len()));
            payload.extend_from_slice(&block_ssz);
        }
        Ok(payload)
    }

    fn decode_blocks(data: &[u8]) -> io::Result<Vec<SignedBlockWithAttestation>> {
        let mut blocks = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let (block_len, varint_size) = decode_varint(&data[pos..])?;
            pos += varint_size;
            if pos + block_len > data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated block payload",
                ));
            }
            let block = SignedBlockWithAttestation::from_ssz_default(&data[pos..pos + block_len])
                .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("SSZ decode block: {e:?}"),
                )
            })?;
            blocks.push(block);
            pos += block_len;
        }
        Ok(blocks)
    }

    fn encode_lean_request(request: &LeanRequest) -> io::Result<Vec<u8>> {
        let ssz_bytes = match request {
            LeanRequest::Status(status) => status
                .to_ssz()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("SSZ encode: {e}")))?,
            LeanRequest::BlocksByRoot(roots) => {
                let mut bytes = Vec::with_capacity(roots.len() * 32);
                for root in roots {
                    bytes.extend_from_slice(root.as_bytes());
                }
                bytes
            }
        };
        Self::encode_request(&ssz_bytes)
    }

    fn decode_lean_request(protocol: &str, data: &[u8]) -> io::Result<LeanRequest> {
        let ssz_bytes = Self::decode_request_payload(data)?;
        if protocol.contains("status") {
            let status = Status::from_ssz_default(&ssz_bytes).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("SSZ decode Status: {e:?}"))
            })?;
            Ok(LeanRequest::Status(status))
        } else if protocol.contains("blocks_by_root") {
            let mut roots = Vec::new();
            for chunk in ssz_bytes.chunks(32) {
                if chunk.len() == 32 {
                    let mut root = [0u8; 32];
                    root.copy_from_slice(chunk);
                    roots.push(H256::from(root));
                }
            }
            if roots.len() > MAX_REQUEST_BLOCKS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "too many block roots: {} > {MAX_REQUEST_BLOCKS}",
                        roots.len()
                    ),
                ));
            }
            Ok(LeanRequest::BlocksByRoot(roots))
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("unknown protocol: {protocol}"),
            ))
        }
    }

    fn encode_lean_response(response: &LeanResponse) -> io::Result<Vec<u8>> {
        match response {
            LeanResponse::Status(status) => {
                let ssz = status.to_ssz().map_err(|e| {
                    io::Error::new(io::ErrorKind::Other, format!("SSZ encode: {e}"))
                })?;
                Self::encode_response_chunk(&ssz)
            }
            LeanResponse::BlocksByRoot(blocks) => {
                let payload = Self::encode_blocks(blocks)?;
                Self::encode_response_chunk(&payload)
            }
            LeanResponse::Empty => Ok(Vec::new()),
        }
    }

    fn decode_lean_response(protocol: &str, data: &[u8]) -> io::Result<LeanResponse> {
        if data.is_empty() {
            return Ok(LeanResponse::Empty);
        }
        let payload = Self::decode_response_chunk(data)?;
        if protocol.contains("status") {
            let status = Status::from_ssz_default(&payload).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("SSZ decode Status: {e:?}"))
            })?;
            Ok(LeanResponse::Status(status))
        } else if protocol.contains("blocks_by_root") {
            let blocks = Self::decode_blocks(&payload)?;
            Ok(LeanResponse::BlocksByRoot(blocks))
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("unknown protocol: {protocol}"),
            ))
        }
    }
}

#[async_trait]
impl Codec for LeanCodec {
    type Protocol = LeanProtocol;
    type Request = LeanRequest;
    type Response = LeanResponse;

    async fn read_request<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut data = Vec::new();
        io.read_to_end(&mut data).await?;
        Self::decode_lean_request(&protocol.0, &data)
    }

    async fn read_response<T>(
        &mut self,
        protocol: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut data = Vec::new();
        io.read_to_end(&mut data).await?;
        Self::decode_lean_response(&protocol.0, &data)
    }

    async fn write_request<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        request: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let data = Self::encode_lean_request(&request)?;
        io.write_all(&data).await?;
        io.close().await
    }

    async fn write_response<T>(
        &mut self,
        _protocol: &Self::Protocol,
        io: &mut T,
        response: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let data = Self::encode_lean_response(&response)?;
        io.write_all(&data).await?;
        io.close().await
    }
}

/// Encode `value` as unsigned LEB128 (varint).
pub fn encode_varint(mut value: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            buf.push(byte | 0x80);
        } else {
            buf.push(byte);
            break;
        }
    }
    buf
}

/// Decode an unsigned LEB128 varint from the start of `data`.
/// Returns `(value, bytes_consumed)`.
pub fn decode_varint(data: &[u8]) -> io::Result<(usize, usize)> {
    let mut result = 0usize;
    let mut shift = 0usize;
    for (i, &byte) in data.iter().enumerate() {
        let value = (byte & 0x7F) as usize;
        result |= value << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        if shift >= 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "varint overflow",
            ));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "truncated varint",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_small() {
        for value in [0, 1, 127, 128, 300, 1000, MAX_PAYLOAD_SIZE] {
            let encoded = encode_varint(value);
            let (decoded, consumed) = decode_varint(&encoded).unwrap();
            assert_eq!(decoded, value, "roundtrip failed for {value}");
            assert_eq!(consumed, encoded.len(), "consumed wrong for {value}");
        }
    }

    #[test]
    fn varint_multi_byte() {
        let encoded = encode_varint(300);
        assert_eq!(encoded, vec![0xAC, 0x02]);
        let (decoded, consumed) = decode_varint(&encoded).unwrap();
        assert_eq!(decoded, 300);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn request_wire_format_roundtrip() {
        let ssz_data = b"hello world";
        let wire = LeanCodec::encode_request(ssz_data).unwrap();
        let decoded = LeanCodec::decode_request_payload(&wire).unwrap();
        assert_eq!(decoded, ssz_data);
    }

    #[test]
    fn request_length_mismatch_rejected() {
        let compressed = LeanCodec::compress(b"data").unwrap();
        let mut wire = encode_varint(999);
        wire.extend_from_slice(&compressed);
        assert!(LeanCodec::decode_request_payload(&wire).is_err());
    }

    #[test]
    fn response_wire_format_roundtrip() {
        let ssz_data = b"response payload";
        let wire = LeanCodec::encode_response_chunk(ssz_data).unwrap();
        assert_eq!(wire[0], RESPONSE_CODE_SUCCESS);
        let decoded = LeanCodec::decode_response_chunk(&wire).unwrap();
        assert_eq!(decoded, ssz_data);
    }

    #[test]
    fn response_non_success_code_rejected() {
        let mut wire = vec![1u8];
        wire.extend_from_slice(&encode_varint(5));
        wire.extend_from_slice(&LeanCodec::compress(b"error").unwrap());
        assert!(LeanCodec::decode_response_chunk(&wire).is_err());
    }

    #[test]
    fn payload_too_large_rejected() {
        let big = vec![0u8; MAX_PAYLOAD_SIZE + 1];
        assert!(LeanCodec::encode_request(&big).is_err());
        assert!(LeanCodec::encode_response_chunk(&big).is_err());
    }

    #[test]
    fn empty_blocks_roundtrip() {
        let blocks: Vec<SignedBlockWithAttestation> = vec![];
        let encoded = LeanCodec::encode_blocks(&blocks).unwrap();
        let decoded = LeanCodec::decode_blocks(&encoded).unwrap();
        assert_eq!(decoded.len(), 0);
    }

    #[test]
    fn decode_varint_truncated() {
        assert!(decode_varint(&[0x80]).is_err());
    }
}
