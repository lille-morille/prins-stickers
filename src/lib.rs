//! The Prins command line: draw stickers on a 312x216 one-bit grid and print them on an M110.
//! See CONTRACT.md for how the modules fit together.

// Wave 1: building blocks that depend only on the `image` crate's types.
pub mod raster;
pub mod draw;
pub mod pyrandom;
pub mod patterns;
pub mod figures;
pub mod frames;
pub mod formula;
pub mod fonts;
pub mod assets;
pub mod ble;
pub mod m110;
pub mod config;

// Wave 2: the design engine and the command line.
pub mod craft;
pub mod poster;
pub mod canvas;
pub mod tools;
pub mod store;
pub mod commands; // commands/mod.rs wires; commands/printer.rs holds the printer subcommands
