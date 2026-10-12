//! Shared E2E fixtures for the GJC SDK lane (issue #326).
//!
//! Each integration-test crate compiles this module independently, so fixture
//! helpers exercised only by a sibling crate are intentionally dead here.

#![allow(dead_code)]

pub mod gjc_fake_server;
pub mod gjc_wire;
