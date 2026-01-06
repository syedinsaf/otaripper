use clap::Parser;
use mimalloc::MiMalloc;

// Use MiMalloc for better performance in multi-threaded extraction
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use otaripper::cmd::Cmd;

fn main() {
    if let Err(e) = Cmd::parse().run() {
        eprintln!("\nERROR: {:#}", e);
        eprintln!(
            "The program has been halted. Any partially extracted partition images have been removed."
        );
        std::process::exit(1);
    }
}
