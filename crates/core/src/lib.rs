//! Frith's pure core: everything about panes that can be decided without a window, a GPU,
//! a PTY, or a clock. Edges (the `frith` crate) feed it events and draw what it decides.

pub mod colors;
pub mod config;
pub mod geom;
pub mod gesture;
pub mod layout;
pub mod motion;
pub mod scene;
