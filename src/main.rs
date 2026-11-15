//! Binary entry point.
//!
//! This file is the only place where `unwrap`/`expect` and `anyhow` are allowed
//! (`AGENTS.md` §2, §5). Everything below it returns typed errors.

use std::process::ExitCode;

use venera::cli;

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("venera: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(cli::run(args))
}
