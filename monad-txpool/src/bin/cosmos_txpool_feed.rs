//! Feed raw Cosmos SDK transaction bytes to monad-node via the mempool Unix socket.

use std::env;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::process::ExitCode;

fn mempool_sock_from_env() -> Option<OsString> {
    env::var_os("MONAD_MEMPOOL_SOCK").filter(|p| !p.is_empty())
}

fn usage() {
    eprintln!("usage: cosmos-txpool-feed <mempool.sock> <tx.raw>");
    eprintln!("   or: MONAD_MEMPOOL_SOCK=<mempool.sock> cosmos-txpool-feed <tx.raw>");
    eprintln!("socket must match monad-node --mempool-ipc-path (empty path yields EINVAL)");
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = env::args_os();
    let _ = args.next();
    let mut rest = args.collect::<Vec<_>>();

    let (socket, path): (OsString, OsString) = match rest.len() {
        0 => {
            usage();
            return ExitCode::FAILURE;
        }
        1 => {
            let path = rest.pop().unwrap();
            let Some(sock) = mempool_sock_from_env() else {
                eprintln!("error: single-argument form requires MONAD_MEMPOOL_SOCK");
                usage();
                return ExitCode::FAILURE;
            };
            (sock, path)
        }
        2 => {
            let path = rest.pop().unwrap();
            let mut socket = rest.pop().unwrap();
            if socket.is_empty() {
                let Some(sock) = mempool_sock_from_env() else {
                    eprintln!("error: mempool socket path is empty; pass monad-node's --mempool-ipc-path or set MONAD_MEMPOOL_SOCK");
                    usage();
                    return ExitCode::FAILURE;
                };
                socket = sock;
            }
            (socket, path)
        }
        _ => {
            usage();
            return ExitCode::FAILURE;
        }
    };

    let raw = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read tx file: {e}");
            return ExitCode::FAILURE;
        }
    };
    let nbytes = raw.len();
    let sock_disp = socket.to_string_lossy().into_owned();

    match monad_txpool::cosmos_txpool_ipc::feed_raw_txs(&socket, vec![raw]).await {
        Ok(()) => {
            eprintln!("ok: sent 1 tx ({} bytes) to {} (exit 0)", nbytes, sock_disp);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("connect to monad-node mempool unix socket ({sock_disp}): {e}");
            if e.kind() == ErrorKind::NotFound {
                eprintln!("hint: socket file missing — start monad-node first, and use the same path as --mempool-ipc-path (not only MONAD_MEMPOOL_SOCK in your shell)");
            }
            ExitCode::FAILURE
        }
    }
}
