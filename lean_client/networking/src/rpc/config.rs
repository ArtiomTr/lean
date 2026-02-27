use super::{Protocol, rate_limiter::Quota};
use std::num::NonZeroU64;
use std::{
    fmt::{Debug, Display},
    str::FromStr,
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Auxiliary struct to aid on configuration parsing.
///
/// A protocol's quota is specified as `protocol_name:tokens/time_in_seconds`.
#[derive(Debug, PartialEq, Eq)]
struct ProtocolQuota {
    protocol: Protocol,
    quota: Quota,
}

impl Display for ProtocolQuota {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}/{}",
            self.protocol.as_ref(),
            self.quota.max_tokens,
            self.quota.replenish_all_every.as_secs()
        )
    }
}

impl FromStr for ProtocolQuota {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (protocol_str, quota_str) = s
            .split_once(':')
            .ok_or("Missing ':' from quota definition.")?;
        let protocol = protocol_str
            .parse()
            .map_err(|_parse_err| "Wrong protocol representation in quota")?;
        let (tokens_str, time_str) = quota_str
            .split_once('/')
            .ok_or("Quota should be defined as \"n/t\" (t in seconds). Missing '/' from quota.")?;
        let tokens = tokens_str
            .parse()
            .map_err(|_| "Failed to parse tokens from quota.")?;
        let seconds = time_str
            .parse::<u64>()
            .map_err(|_| "Failed to parse time in seconds from quota.")?;
        Ok(ProtocolQuota {
            protocol,
            quota: Quota {
                replenish_all_every: Duration::from_secs(seconds),
                max_tokens: tokens,
            },
        })
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug, Default)]
pub struct OutboundRateLimiterConfig(pub RateLimiterConfig);

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug, Default)]
pub struct InboundRateLimiterConfig(pub RateLimiterConfig);

impl FromStr for OutboundRateLimiterConfig {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RateLimiterConfig::from_str(s).map(Self)
    }
}

impl FromStr for InboundRateLimiterConfig {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RateLimiterConfig::from_str(s).map(Self)
    }
}

/// Configurations for the rate limiter.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateLimiterConfig {
    pub(super) status_quota: Quota,
    pub(super) blocks_by_root_quota: Quota,
}

impl RateLimiterConfig {
    pub const DEFAULT_STATUS_QUOTA: Quota = Quota::n_every(NonZeroU64::new(5).unwrap(), 15);
    // The number is chosen to balance between upload bandwidth required to serve
    // blocks and a decent syncing rate for honest nodes. Malicious nodes would need to
    // spread out their requests over the time window to max out bandwidth on the server.
    pub const DEFAULT_BLOCKS_BY_ROOT_QUOTA: Quota =
        Quota::n_every(NonZeroU64::new(128).unwrap(), 10);
}

impl Default for RateLimiterConfig {
    fn default() -> Self {
        RateLimiterConfig {
            status_quota: Self::DEFAULT_STATUS_QUOTA,
            blocks_by_root_quota: Self::DEFAULT_BLOCKS_BY_ROOT_QUOTA,
        }
    }
}

impl Debug for RateLimiterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        macro_rules! fmt_q {
            ($quota:expr) => {
                &format_args!(
                    "{}/{}s",
                    $quota.max_tokens,
                    $quota.replenish_all_every.as_secs()
                )
            };
        }

        f.debug_struct("RateLimiterConfig")
            .field("status", fmt_q!(&self.status_quota))
            .field("blocks_by_root", fmt_q!(&self.blocks_by_root_quota))
            .finish()
    }
}

/// Parse configurations for the outbound rate limiter. Protocols that are not specified use
/// the default values. Protocol specified more than once use only the first given Quota.
///
/// The expected format is a ';' separated list of [`ProtocolQuota`].
impl FromStr for RateLimiterConfig {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut status_quota = None;
        let mut blocks_by_root_quota = None;

        for proto_def in s.split(';') {
            let ProtocolQuota { protocol, quota } = proto_def.parse()?;
            let quota = Some(quota);
            match protocol {
                Protocol::Status => status_quota = status_quota.or(quota),
                Protocol::BlocksByRoot => blocks_by_root_quota = blocks_by_root_quota.or(quota),
            }
        }
        Ok(RateLimiterConfig {
            status_quota: status_quota.unwrap_or(Self::DEFAULT_STATUS_QUOTA),
            blocks_by_root_quota: blocks_by_root_quota
                .unwrap_or(Self::DEFAULT_BLOCKS_BY_ROOT_QUOTA),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quota_inverse() {
        let quota = ProtocolQuota {
            protocol: Protocol::Status,
            quota: Quota {
                replenish_all_every: Duration::from_secs(10),
                max_tokens: NonZeroU64::new(8).unwrap(),
            },
        };
        assert_eq!(quota.to_string().parse(), Ok(quota))
    }
}
