//! `ues` — the research prototype binary.
//!
//! All logic lives in the library, so the integration tests exercise exactly the
//! same code paths as the CLI.
fn main() {
    // The crash hook is checked before anything else and dispatches on the
    // environment only, so it cannot be reached by a command line, never appears
    // in `--help`, and does not need the argument parser to be reachable at all.
    ues::cli::crash_child();
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(i32::from(ues::cli::main_with_args(&args) == 0));
}
