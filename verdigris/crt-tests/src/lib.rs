//! Host tests for the pure parts of Verdigris's C runtime: printf's
//! formatter and the command-line and number readers. kernel32 is a ring 3
//! program for CorrodeOS, which the build host cannot run, so this crate
//! builds the same source files (`#[path]`, unchanged) as an ordinary
//! library and checks them against the strings the C standard and
//! Microsoft's runtime give.

#![no_std]

extern crate alloc;

#[path = "../../kernel32/src/format.rs"]
pub mod format;
#[path = "../../kernel32/src/text.rs"]
pub mod text;

#[cfg(test)]
mod tests;
