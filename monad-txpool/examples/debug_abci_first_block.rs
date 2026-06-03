use std::{env, path::PathBuf, process::ExitCode};

use monad_txpool::debug_abci_first_block;

fn main() -> ExitCode {
    let endpoint =
        env::var("MONAD_ABCI_ENDPOINT").unwrap_or_else(|_| "unix:///tmp/biyachain-abci.sock".to_owned());
    let genesis_path = env::var("MONAD_COSMOS_GENESIS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
                .join(".biyachaind/config/genesis.json")
        });

    match debug_abci_first_block(&endpoint, &genesis_path) {
        Ok(info) => {
            println!("{}", serde_json::to_string_pretty(&info).unwrap());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
