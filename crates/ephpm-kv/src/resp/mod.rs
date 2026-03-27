//! RESP2 (Redis Serialization Protocol) implementation.
//!
//! Provides frame types and a streaming parser for the RESP wire protocol.
//! This module handles encoding and decoding — command semantics live in
//! [`crate::command`].

mod frame;
mod parse;

pub use frame::Frame;
pub use parse::{parse_frame, ParseError};
