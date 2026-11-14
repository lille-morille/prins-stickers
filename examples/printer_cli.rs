//! Stand-alone runner for `commands/printer.rs` until `prins printer` is wired into `main.rs`.
//!
//!     cargo run --example printer_cli -- scan
//!     cargo run --example printer_cli -- test --dry-run
//!     cargo test --example printer_cli          # the unit tests of commands/printer.rs
//!
//! `printer.rs` refers to its siblings as `crate::ble`, `crate::config` and `crate::m110`, so this
//! file re-exports them from the library under those names.
//! Set `PRINS_HOME` to a temp folder to keep away from the real `~/.prins/config.json`.

#![allow(dead_code)]

mod ble {
    pub use prins::ble::*;
}
mod config {
    pub use prins::config::*;
}
mod m110 {
    pub use prins::m110::*;
}
#[path = "../src/commands/printer.rs"]
mod printer;

use clap::Parser;

#[derive(Parser)]
#[command(name = "printer_cli", about = "prins printer ... (stand-alone)")]
struct Cli {
    #[command(subcommand)]
    cmd: printer::PrinterCmd,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = printer::run(cli.cmd) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
