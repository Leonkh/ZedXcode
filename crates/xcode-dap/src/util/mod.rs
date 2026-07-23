//! Small process/file utilities.
//!
//! Invariant for anything that prints diagnostics: in DAP mode stdout is
//! reserved for DAP frames, so diagnostics must go to stderr or to files
//! under `~/.zedxcode/`, never to stdout.

pub mod hash;
pub mod logging;
pub mod paths;
pub mod pidfile;
pub mod procgroup;
