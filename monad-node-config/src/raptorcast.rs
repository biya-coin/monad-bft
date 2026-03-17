use std::fmt;

use serde::{Deserialize, Serialize};

// Gradual rollout from v0 (regular) to v1 (deterministic) raptorcast.
//
// The rollout proceeds through four stages, bumped on release:
//
//                    AcceptBoth        AcceptBoth
//   AlwaysV0         PublishV0         PublishV1          AlwaysV1
//  -----------------+------------------+------------------+----------
//   v0 only         |  accept both,    |  accept both,    |  v1 only
//                   |  publish v0      |  publish v1      |

pub const CURRENT_STAGE: DeterministicProtocolRolloutStage =
    DeterministicProtocolRolloutStage::AlwaysV0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeterministicProtocolRolloutStage {
    AlwaysV0,
    AcceptBothPublishV0,
    AcceptBothPublishV1,
    AlwaysV1,
}

impl Default for DeterministicProtocolRolloutStage {
    fn default() -> Self {
        CURRENT_STAGE
    }
}

impl fmt::Display for DeterministicProtocolRolloutStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlwaysV0 => write!(f, "always_v0"),
            Self::AcceptBothPublishV0 => write!(f, "accept_both_publish_v0"),
            Self::AcceptBothPublishV1 => write!(f, "accept_both_publish_v1"),
            Self::AlwaysV1 => write!(f, "always_v1"),
        }
    }
}
