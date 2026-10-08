//! `winhandsd`: the winhands daemon with no console window.
//!
//! The same daemon as `winhands daemon`, built for the Windows GUI subsystem so the scheduled task
//! `winhands daemon` (or `winhands mcp`, which looks for this file next to itself) can start it
//! without a console flashing up or lingering. Its messages and panics go to `daemon.log` in the data
//! directory. On other systems it is an ordinary program with the same behaviour, so the workspace
//! builds and tests the same everywhere. `winhands mcp` and the command line keep the console build.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(winhands_lib::cli::main_hidden(args));
}
