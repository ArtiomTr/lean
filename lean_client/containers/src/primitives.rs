use std::{
    fmt::{self, Display},
    str::FromStr,
};

use ethereum_types::H32;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, Visitor},
};
use ssz::{Ssz, U1};

pub type Version = H32;
pub type SubnetId = u64;

pub type AttestationSubnetCount = U1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Ssz, Default, Hash)]
#[ssz(transparent)]
pub struct ForkDigest(H32);

impl ForkDigest {
    pub fn devnet0() -> Self {
        // TODO(networking): remove this once in production. This is a custom
        //   value for `devnet0` fork, not specified anywhere. It is needed
        //   because "devnet0" cannot be encoded as a valid hex digest, thus
        //   this value needs special handling both here and inside `Display`.
        Self(H32([0xde, 0x59, 0xe1, 0x00]))
    }
}

impl Display for ForkDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self == &Self::devnet0() {
            f.write_str("devnet0")
        } else {
            f.write_str(&hex::encode(self.0))
        }
    }
}

impl FromStr for ForkDigest {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "devnet0" => Ok(ForkDigest::devnet0()),
            _ => {
                let mut res = H32::zero();
                hex::decode_to_slice(s, &mut res.0)?;

                Ok(Self(res))
            }
        }
    }
}

impl Serialize for ForkDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}

impl<'de> Deserialize<'de> for ForkDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ForkDigestVisitor;

        impl Visitor<'_> for ForkDigestVisitor {
            type Value = ForkDigest;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("fork digest")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                v.parse().map_err(de::Error::custom)
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(v.as_str())
            }
        }

        deserializer.deserialize_str(ForkDigestVisitor)
    }
}
