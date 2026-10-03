//! Host test crate for src/resources.rs, which depends on src/mac.rs.
#![allow(dead_code)]

#[path = "../src/mac.rs"]
mod mac;
#[path = "../src/resources.rs"]
mod resources;
