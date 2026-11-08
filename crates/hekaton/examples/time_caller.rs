//! Times the per-call caller lookup pieces on the live machine (read-only, no input).
use std::time::Instant;

fn main() {
    let b = hekaton_lib::platform::backend().expect("backend");
    let me = std::process::id();
    for round in 0..3 {
        let t = Instant::now();
        let table = b.process_table().unwrap();
        let t_table = t.elapsed();
        let t = Instant::now();
        let _ = b.peer_pid(50000, 47911);
        let t_peer = t.elapsed();
        let t = Instant::now();
        let _ = b.process_info(me);
        let t_info = t.elapsed();
        let t = Instant::now();
        let chain = hekaton_core::caller::process_chain(b.as_ref(), me);
        let t_chain = t.elapsed();
        let t = Instant::now();
        let c = hekaton_core::caller::identify(b.as_ref(), 50000, 47911, "127.0.0.1", false);
        let t_ident = t.elapsed();
        println!(
            "round {round}: table({} procs) {:?} | peer_pid {:?} | process_info {:?} | chain({}) {:?} | identify {:?} {c:?}",
            table.len(),
            t_table,
            t_peer,
            t_info,
            chain.len(),
            t_chain,
            t_ident
        );
    }
}
