use ethereum_types::H32;

// TODO(networking): due to fork digest for devnets being "devnet0" string, that
//   is obviously not valid hex string, the fork digest currently is being
//   encoded suboptimally as a string. In production, this should be replaced
//   with H32 (like in eth2).
pub type ForkDigest = String;
pub type Version = H32;
pub type SubnetId = u64;
