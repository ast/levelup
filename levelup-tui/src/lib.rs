//! Shared TUI toolkit for the levelup pickers: `/dev/tty` terminal setup, the
//! readline query-editing helpers, the `‹›`→spans highlighter, and the config
//! primitives (named-ANSI palette, layout, generic loader).
//!
//! A *toolkit*, not a framework: each picker keeps its own `State`, event loop
//! and row rendering — they just call these functions, so the divergent parts
//! (valkyrie's tree, heimdall's table, the modal choosers) stay simple.

pub mod config;
pub mod editing;
pub mod highlight;
pub mod terminal;
