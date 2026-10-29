//! Hekaton, the product name. `winhands`, `winhandsd` and `wh` stay as compatible names, and so do
//! the MCP tool name `computer`, the ports, the data directory and the scheduled task.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(winhands_lib::cli::main(args));
}
