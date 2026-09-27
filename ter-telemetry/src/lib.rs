//! Venues, instruments and the event stream.
//!
//! A venue is where a program runs (a local board, a simulator, a remote
//! bench) and an instrument observes or drives lines alongside it. Both
//! record into one event stream that `ter-check` evaluates.
//!
//! This crate must never depend on anything that reaches the network; the
//! local check enforces it.
