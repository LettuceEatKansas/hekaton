//! `hekatond`: the hekaton daemon with no console window.
//!
//! The same daemon as `hekaton daemon`, built for the Windows GUI subsystem so the scheduled task
//! `hekaton daemon` (or `hekaton mcp`, which looks for this file next to itself) can start it
//! without a console flashing up or lingering. Its messages and panics go to `daemon.log` in the data
//! directory. On other systems it is an ordinary program with the same behaviour, so the workspace
//! builds and tests the same everywhere. `hekaton mcp` and the command line keep the console build.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    hekaton_lib::cli::move_old_dir();
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(hekaton_lib::cli::main_hidden(args));
}
