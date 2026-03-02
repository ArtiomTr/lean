use core::{
    convert::TryFrom,
    fmt::{self, Debug, Display},
    str::FromStr,
};

use anyhow::{Error, anyhow, Result};
use eth_ssz::DecodeError;
use leansig::{serialization::Serializable, signature::SignatureScheme};
use leansig::signature::generalized_xmss::instantiations_poseidon_top_level::lifetime_2_to_the_32::hashing_optimized::SIGTopLevelTargetSumLifetime32Dim64Base8;
use serde::de;
use serde::{Deserialize, Serialize};
use ssz::{ByteList, H256, Ssz};
use crate::public_key::PublicKey;
use typenum::{Diff, U984, U4096};

type U3112 = Diff<U4096, U984>;

type SignatureSizeLimit = U3112;

type LeanSigSignature = <SIGTopLevelTargetSumLifetime32Dim64Base8 as SignatureScheme>::Signature;

// todo(xmss): default implementation doesn't make sense here, and is needed only for tests
#[derive(Clone, Default, PartialEq, Ssz)]
#[ssz(transparent)]
pub struct Signature(ByteList<SignatureSizeLimit>);

impl Signature {
    pub fn new(inner: &[u8]) -> Result<Self, DecodeError> {
        LeanSigSignature::from_bytes(inner)?;

        let bytes = ByteList::try_from(inner.to_vec()).map_err(|_| {
            DecodeError::BytesInvalid("signature exceeds maximum allowed length".into())
        })?;

        Ok(Self(bytes))
    }

    pub fn verify(&self, public_key: &PublicKey, epoch: u32, message: H256) -> Result<()> {
        let is_valid = <SIGTopLevelTargetSumLifetime32Dim64Base8 as SignatureScheme>::verify(
            &public_key.as_lean(),
            epoch,
            message.as_fixed_bytes(),
            &self.as_lean(),
        );

        is_valid.then_some(()).ok_or(anyhow!("invalid signature"))
    }

    pub(crate) fn from_lean(signature: LeanSigSignature) -> Self {
        let bytes = signature.to_bytes();

        Self::new(bytes.as_slice())
            .expect("signature bytes produced by leansig should always be valid")
    }

    pub(crate) fn as_lean(&self) -> LeanSigSignature {
        LeanSigSignature::from_bytes(self.0.as_bytes())
            .expect("signature internal representation must be valid leansig signature")
    }
}

impl Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0.as_bytes()))
    }
}

impl Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0.as_bytes()))
    }
}

impl FromStr for Signature {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let data = s.strip_prefix("0x").unwrap_or(s);

        let bytes = hex::decode(data)?;

        Self::new(&bytes).map_err(|err| anyhow!("{err:?}"))
    }
}

impl TryFrom<&[u8]> for Signature {
    type Error = DecodeError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for Signature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum SignatureInput {
            Hex(String),
            Structured(XmssSignature),
        }

        #[derive(Deserialize)]
        struct DataWrapper<T> {
            data: T,
        }

        #[derive(Deserialize)]
        struct XmssSignature {
            path: XmssPath,
            rho: DataWrapper<Vec<u32>>,
            hashes: DataWrapper<Vec<DataWrapper<Vec<u32>>>>,
        }

        #[derive(Deserialize)]
        struct XmssPath {
            siblings: DataWrapper<Vec<DataWrapper<Vec<u32>>>>,
        }

        let input = SignatureInput::deserialize(deserializer)?;

        if let SignatureInput::Hex(value) = input {
            return value.parse().map_err(de::Error::custom);
        }

        let SignatureInput::Structured(xmss_sig) = input else {
            unreachable!();
        };

        let mut rho_bytes = Vec::new();
        for val in &xmss_sig.rho.data {
            rho_bytes.extend_from_slice(&val.to_le_bytes());
        }
        let rho_len = rho_bytes.len();

        let mut path_bytes = Vec::new();
        let inner_offset: u32 = 4;
        path_bytes.extend_from_slice(&inner_offset.to_le_bytes());
        for sibling in &xmss_sig.path.siblings.data {
            for val in &sibling.data {
                path_bytes.extend_from_slice(&val.to_le_bytes());
            }
        }

        let mut hashes_bytes = Vec::new();
        for hash in &xmss_sig.hashes.data {
            for val in &hash.data {
                hashes_bytes.extend_from_slice(&val.to_le_bytes());
            }
        }

        let fixed_part_size = 4 + rho_len + 4;
        let offset_path = fixed_part_size as u32;
        let offset_hashes = offset_path + (path_bytes.len() as u32);

        let mut ssz_bytes = Vec::new();
        ssz_bytes.extend_from_slice(&offset_path.to_le_bytes());
        ssz_bytes.extend_from_slice(&rho_bytes);
        ssz_bytes.extend_from_slice(&offset_hashes.to_le_bytes());
        ssz_bytes.extend_from_slice(&path_bytes);
        ssz_bytes.extend_from_slice(&hashes_bytes);

        Signature::try_from(ssz_bytes.as_slice())
            .map_err(|err| de::Error::custom(format!("invalid signature: {err:?}")))
    }
}

#[cfg(test)]
mod test {
    use super::SignatureSizeLimit;
    use typenum::Unsigned;

    #[test]
    fn valid_signature_size() {
        assert_eq!(SignatureSizeLimit::U64, 3112);
    }
}
