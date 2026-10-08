//! The winhands binary: `winhands daemon | mcp | wh | status | start | stop | pause | resume |
//! acknowledge | doctor`. Run under the name `wh` it is the `wh` command line.

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let invoked_as_wh = std::env::args_os()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_stem()
                .map(|s| s.to_ascii_lowercase())
        })
        .is_some_and(|stem| stem == "wh");
    if invoked_as_wh {
        args.insert(0, "wh".to_string());
    }
    std::process::exit(winhands_lib::cli::main(args));
}
