#[allow(clippy::all, dead_code)]
mod chromeos_update_engine {
    include!(concat!(env!("OUT_DIR"), "/chromeos_update_engine.rs"));
}

mod cmd;
mod payload;

use clap::Parser;

use crate::cmd::Cmd;

fn main() {
    if let Err(e) = Cmd::parse().run() {
        eprintln!("\nERROR: {:#}", e);
        eprintln!(
            "The program has been halted. Any partially extracted partition images have been removed."
        );
        std::process::exit(1);
    }
}
