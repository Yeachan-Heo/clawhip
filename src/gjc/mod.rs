//! Authoritative GJC SDK session query and control API (#323).
//!
//! Typed queries and mutation verbs over a narrow transport boundary.
//! The transport itself (endpoint discovery, authentication, websocket
//! frames) is owned by the #322 transport track; this module only defines
//! the envelope contract and the control plane on top of it. Until #322
//! provides a real implementation the daemon fails closed with
//! [`GjcError::TransportUnavailable`] rather than guessing.

pub mod api;
pub mod cli;
pub mod control;
pub mod model;
