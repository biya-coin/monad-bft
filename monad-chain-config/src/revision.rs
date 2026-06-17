// Copyright (C) 2025 Category Labs, Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::{fmt::Debug, time::Duration};

use monad_eth_types::MAX_TRANSACTIONS_PER_BLOCK;

macro_rules! chain_params {
    (
        tx_limit: $tx_limit:expr,
        proposal_gas_limit: $proposal_gas_limit:expr,
        proposal_byte_limit: $proposal_byte_limit:expr,
        max_reserve_balance: $max_reserve_balance:expr,
        vote_pace: $vote_pace:expr $(,)?
    ) => {{
        const _: () = assert!(
            $tx_limit <= MAX_TRANSACTIONS_PER_BLOCK,
            "tx_limit must not exceed MAX_TRANSACTIONS_PER_BLOCK"
        );
        ChainParams {
            tx_limit: $tx_limit,
            proposal_gas_limit: $proposal_gas_limit,
            proposal_byte_limit: $proposal_byte_limit,
            max_reserve_balance: $max_reserve_balance,
            vote_pace: $vote_pace,
        }
    }};
}

pub const CHAIN_PARAMS_LATEST: ChainParams = CHAIN_PARAMS_V_0_11_0;

pub trait ChainRevision: Copy + Clone {
    fn chain_params(&self) -> &'static ChainParams;
}

#[allow(non_camel_case_types)]
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub enum MonadChainRevision {
    V_0_7_0,
    V_0_8_0,
    V_0_10_0,
    V_0_11_0,
}

impl ChainRevision for MonadChainRevision {
    fn chain_params(&self) -> &'static ChainParams {
        match &self {
            MonadChainRevision::V_0_7_0 => &CHAIN_PARAMS_V_0_7_0,
            MonadChainRevision::V_0_8_0 => &CHAIN_PARAMS_V_0_8_0,
            MonadChainRevision::V_0_10_0 => &CHAIN_PARAMS_V_0_10_0,
            MonadChainRevision::V_0_11_0 => &CHAIN_PARAMS_V_0_11_0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MockChainRevision {
    pub chain_params: &'static ChainParams,
}

impl MockChainRevision {
    pub const DEFAULT: Self = Self {
        chain_params: &CHAIN_PARAMS_LATEST,
    };
}

impl ChainRevision for MockChainRevision {
    fn chain_params(&self) -> &'static ChainParams {
        self.chain_params
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainParams {
    pub tx_limit: usize,
    pub proposal_gas_limit: u64,
    // Max proposal size in bytes (average transactions ~400 bytes)
    pub proposal_byte_limit: u64,
    pub max_reserve_balance: u128,
    pub vote_pace: Duration,
}

const CHAIN_PARAMS_V_0_7_0: ChainParams = chain_params! {
    tx_limit: 10_000,
    proposal_gas_limit: 300_000_000,
    proposal_byte_limit: 4_000_000,
    max_reserve_balance: 10_000_000_000_000_000_000, // 10 MON
    vote_pace: Duration::from_millis(1000),
};

const CHAIN_PARAMS_V_0_8_0: ChainParams = chain_params! {
    tx_limit: 5_000,
    proposal_gas_limit: 150_000_000,
    proposal_byte_limit: 2_000_000,
    max_reserve_balance: 10_000_000_000_000_000_000, // 10 MON
    vote_pace: Duration::from_millis(500),
};

const CHAIN_PARAMS_V_0_10_0: ChainParams = chain_params! {
    tx_limit: 5_000,
    proposal_gas_limit: 150_000_000,
    proposal_byte_limit: 2_000_000,
    max_reserve_balance: 10_000_000_000_000_000_000, // 10 MON
    vote_pace: Duration::from_millis(400),
};

const CHAIN_PARAMS_V_0_11_0: ChainParams = chain_params! {
    // proposal_byte_limit 只限制当前块交易字节；提案还会内嵌延迟执行结果
    // (delayed_execution_results 里的完整 FinalizeBlockResponse)，两者相加 + QC/TC 签名
    // 在满载时会超过 raptorcast 的 MAX_MESSAGE_SIZE(3 MiB)导致 FinalMsgTooLarge 广播失败。
    // 故将交易预算与笔数限制下调，给延迟执行结果留出空间。
    tx_limit: 10_000,
    proposal_gas_limit: 200_000_000,
    proposal_byte_limit: 20_000_000,
    max_reserve_balance: 10_000_000_000_000_000_000, // 10 MON
    vote_pace: Duration::from_millis(100), // 粗暴止血：拉到1s，放慢出块节奏让执行/QC跟上，并把时间戳容忍窗口扩到 ±(500ms+vote_pace)
};

// NOTE: when adding a new revision, chain_params! asserts that tx_limit is <= MAX_TRANSACTIONS_PER_BLOCK

#[cfg(test)]
mod test {
    use crate::revision::MonadChainRevision;

    #[test]
    fn chain_revision_ord() {
        assert!(MonadChainRevision::V_0_7_0 < MonadChainRevision::V_0_8_0);
        assert!(MonadChainRevision::V_0_8_0 < MonadChainRevision::V_0_10_0);
        assert!(MonadChainRevision::V_0_10_0 < MonadChainRevision::V_0_11_0);
    }
}
