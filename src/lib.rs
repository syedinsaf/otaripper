#[allow(clippy::all, dead_code)]
mod chromeos_update_engine {
    include!(concat!(env!("OUT_DIR"), "/chromeos_update_engine.rs"));
}

pub mod cmd;
pub mod payload;
// Re-export commonly-benchmarked types
pub use crate::cmd::ExtentsWriter;
