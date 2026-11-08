//! The hekaton binary: `hekaton daemon | mcp | hk | status | start | stop | pause | resume |
//! acknowledge | doctor`. Run under the name `hk` it is the `hk` command line.

fn main() {
    hekaton_lib::cli::move_old_dir();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if std::env::args_os()
        .next()
        .is_some_and(|argv0| hekaton_lib::cli::invoked_as_hk(&argv0))
    {
        args.insert(0, "hk".to_string());
    }
    std::process::exit(hekaton_lib::cli::main(args));
}
