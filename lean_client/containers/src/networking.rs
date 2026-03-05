use serde::{Deserialize, Serialize};
use ssz::{ContiguousList, H256, Ssz};
use typenum::U1024;

pub type BlocksByRootRequestLimit = U1024;

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize, Ssz)]
#[serde(rename_all = "camelCase")]
pub struct BlocksByRootRequestV1 {
    #[serde(with = "crate::serde_helpers")]
    pub roots: ContiguousList<H256, BlocksByRootRequestLimit>,
}
