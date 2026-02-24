use containers::{ForkDigest, Version};
use ssz::Ssz;

#[derive(Debug, Clone, Default, Ssz)]
pub struct EnrForkId {
    pub fork_digest: ForkDigest,
    pub next_fork_version: Version,
    // TODO(networking): there is no such thing as "epoch" in the context of
    // lean consensus. Currently this is kept to match spec, but probably it
    // will be replaced/fixed once in production. So don't forget to check it
    // sometime in the future.
    pub next_fork_epoch: u64,
}
