//! Feed raw Cosmos SDK transaction bytes to monad-node via the mempool Unix socket.
//!
//! One-shot feed (test_run.sh sendtx):
//!   cosmos-txpool-feed <mempool.sock> <tx.raw>
//!   MONAD_MEMPOOL_SOCK=<mempool.sock> cosmos-txpool-feed <tx.raw>
//!
//! Comet RPC for chain-stresser (port 26657):
//!   cosmos-txpool-feed serve --listen 127.0.0.1:26657 --cosmos-ipc-path <mempool.sock>

use std::env;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "cosmos-txpool-feed")]
struct ServeArgs {
    /// Comet RPC listen address (chain-stresser --node-addr)
    #[arg(long, default_value = "127.0.0.1:26657")]
    listen: String,

    /// monad-node --mempool-ipc-path
    #[arg(long)]
    cosmos_ipc_path: Option<PathBuf>,
}

fn mempool_sock_from_env() -> Option<OsString> {
    env::var_os("MONAD_MEMPOOL_SOCK").filter(|p| !p.is_empty())
}

fn feed_usage() {
    eprintln!("usage: cosmos-txpool-feed <mempool.sock> <tx.raw>");
    eprintln!("   or: MONAD_MEMPOOL_SOCK=<mempool.sock> cosmos-txpool-feed <tx.raw>");
    eprintln!("   or: cosmos-txpool-feed serve [--listen ADDR] --cosmos-ipc-path <mempool.sock>");
    eprintln!("socket must match monad-node --mempool-ipc-path (empty path yields EINVAL)");
}

fn parse_serve_args() -> Option<ServeArgs> {
    let args: Vec<String> = env::args().collect();
    if args.len() >= 2 && args[1] == "serve" {
        let mut serve_argv = vec![args[0].clone()];
        serve_argv.extend(args.iter().skip(2).cloned());
        Some(ServeArgs::parse_from(serve_argv))
    } else {
        None
    }
}

async fn run_feed() -> ExitCode {
    let mut args = env::args_os();
    let _ = args.next();
    let mut rest = args.collect::<Vec<_>>();

    let (socket, path): (OsString, OsString) = match rest.len() {
        0 => {
            feed_usage();
            return ExitCode::FAILURE;
        }
        1 => {
            let path = rest.pop().unwrap();
            let Some(sock) = mempool_sock_from_env() else {
                eprintln!("error: single-argument form requires MONAD_MEMPOOL_SOCK");
                feed_usage();
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
                    feed_usage();
                    return ExitCode::FAILURE;
                };
                socket = sock;
            }
            (socket, path)
        }
        _ => {
            feed_usage();
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

async fn run_serve(args: ServeArgs) -> ExitCode {
    let mempool = match args.cosmos_ipc_path.or_else(|| {
        mempool_sock_from_env().map(PathBuf::from)
    }) {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => {
            eprintln!("error: serve requires --cosmos-ipc-path or MONAD_MEMPOOL_SOCK");
            feed_usage();
            return ExitCode::FAILURE;
        }
    };

    match monad_txpool::comet_mempool_rpc::run_server(&args.listen, mempool).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cosmos-txpool-feed serve: {e}");
            ExitCode::FAILURE
        }
    }
}

#[actix_web::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt::init();
    if let Some(serve_args) = parse_serve_args() {
        run_serve(serve_args).await
    } else {
        run_feed().await
    }
}
