//! The subnet predicate used for searching for a particular subnet.
use tracing::trace;

use crate::{Enr, EnrExt as _, Eth2Enr as _, Subnet};

/// Returns the predicate for a given subnet.
pub fn subnet_predicate(subnets: Vec<Subnet>) -> impl Fn(&Enr) -> bool + Send {
    move |enr| {
        let Ok(attestation_bitfield) = enr.attestation_bitfield() else {
            return false;
        };

        let predicate = subnets.iter().any(|subnet| match subnet {
            Subnet::Attestation(subnet_id) => attestation_bitfield
                .get(*subnet_id as usize)
                .unwrap_or_default(),
        });

        if !predicate {
            trace!(
                peer_id = %enr.peer_id(),
                "Peer found but not on any of the desired subnets"
            );
        }
        predicate
    }
}
