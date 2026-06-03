use std::{collections::BTreeMap, fs, io, path::{Path, PathBuf}};

use monad_cosmos_types::CosmosFinalizedHeader;
use monad_types::{FinalizedHeader, SeqNum, GENESIS_SEQ_NUM};
use serde_json::Value;
use tracing::info;

use crate::{
    block_on_async, build_init_chain_request, info, init_chain,
    CosmosTxPoolError,
};

#[derive(Debug)]
pub struct CosmosCommitStore {
    dir: PathBuf,
    commits: BTreeMap<SeqNum, CosmosFinalizedHeader>,
}

impl CosmosCommitStore {
    pub fn new(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let mut commits = BTreeMap::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rlp") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(height) = stem.parse::<u64>() else {
                continue;
            };
            let bytes = fs::read(&path)?;
            let header: CosmosFinalizedHeader = alloy_rlp::decode_exact(&bytes).map_err(io::Error::other)?;
            commits.insert(SeqNum(height), header);
        }
        Ok(Self { dir, commits })
    }

    pub fn commit(&mut self, header: CosmosFinalizedHeader) -> io::Result<()> {
        let seq_num = header.seq_num();
        let path = self.dir.join(format!("{}.rlp", seq_num.0));
        fs::write(path, alloy_rlp::encode(&header))?;
        self.commits.insert(seq_num, header);
        Ok(())
    }

    pub fn ensure_genesis_from_cosmos_genesis(
        &mut self,
        endpoint: &str,
        genesis_path: impl AsRef<Path>,
    ) -> Result<(), CosmosTxPoolError> {
        if !self.commits.is_empty() {
            return Ok(());
        }

        let genesis_path = genesis_path.as_ref();
        let app_hash = if genesis_path.exists() {
            let json: Value = serde_json::from_slice(&fs::read(genesis_path)?)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            let info_resp = block_on_async(async { info(endpoint).await })?;
            info!(
                endpoint,
                last_block_height = info_resp.last_block_height,
                last_block_app_hash_len = info_resp.last_block_app_hash.len(),
                app_version = info_resp.app_version,
                "received ABCI Info response before genesis initialization"
            );
            if info_resp.last_block_height <= 0 || info_resp.last_block_app_hash.is_empty() {
                let init_request = build_init_chain_request(&json)?;
                let init_response = block_on_async(async { init_chain(endpoint, init_request).await })?;
                info!(
                    endpoint,
                    init_app_hash_len = init_response.app_hash.len(),
                    init_validator_updates = init_response.validators.len(),
                    has_consensus_params = init_response.consensus_params.is_some(),
                    "received ABCI InitChain response"
                );
                init_response.app_hash
            } else {
                info_resp.last_block_app_hash
            }
        } else {
            Vec::new()
        };

        self.commit(CosmosFinalizedHeader {
            height: GENESIS_SEQ_NUM.0,
            app_hash,
            tx_results_hash: Vec::new(),
            validator_updates_hash: Vec::new(),
            finalize_block_response: Vec::new(),
            commit_response: Vec::new(),
            retain_height: GENESIS_SEQ_NUM.0,
        })?;
        Ok(())
    }

    pub fn get(&self, seq_num: &SeqNum) -> Option<&CosmosFinalizedHeader> {
        self.commits.get(seq_num)
    }

    pub fn earliest(&self) -> Option<SeqNum> {
        self.commits.first_key_value().map(|(seq, _)| *seq)
    }

    pub fn latest(&self) -> Option<SeqNum> {
        self.commits.last_key_value().map(|(seq, _)| *seq)
    }

    pub fn sync_with_abci_app(&mut self, endpoint: &str) -> Result<(), CosmosTxPoolError> {
        loop {
            let info = block_on_async(async { info(endpoint).await })?;
            let remote = info.last_block_height.max(0) as u64;
            let local = self.latest().map(|s| s.0).unwrap_or(0);
            if remote <= local {
                return Ok(());
            }
            if remote > local + 1 {
                return Err(CosmosTxPoolError::Transport(format!(
                    "ABCI last_block_height={remote} is ahead of local cosmos-commits latest={local} by more than one; reset the app home or remove cosmos-commits and use a fresh socket (common cause: running debug_abci_first_block against the same biyachaind before monad-node)"
                )));
            }
            info!(
                local,
                remote,
                app_hash_len = info.last_block_app_hash.len(),
                "catching up CosmosCommitStore from ABCI Info (app one block ahead of disk)"
            );
            self.commit(CosmosFinalizedHeader {
                height: remote,
                app_hash: info.last_block_app_hash.clone(),
                tx_results_hash: Vec::new(),
                validator_updates_hash: Vec::new(),
                finalize_block_response: Vec::new(),
                commit_response: Vec::new(),
                retain_height: 0,
            })?;
        }
    }
}
