//! Neutral product alias. Existing winhands commands and installation identities stay compatible.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(winhands_lib::cli::main(args));
}
