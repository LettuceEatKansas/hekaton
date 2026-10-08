//! The winhands binary: `winhands daemon | mcp | wh | status | start | stop | pause | resume |
//! acknowledge | doctor`. Run under the name `wh` it is the `wh` command line.

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if std::env::args_os()
        .next()
        .is_some_and(|argv0| winhands_lib::cli::invoked_as_wh(&argv0))
    {
        args.insert(0, "wh".to_string());
    }
    std::process::exit(winhands_lib::cli::main(args));
}
