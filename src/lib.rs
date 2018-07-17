//! Event processing library used at Sentry.
//!
//! This crate contains types and utility functions for parsing Sentry event payloads, normalizing
//! them into the canonical protocol, and stripping PII.

#![warn(missing_docs)]

extern crate chrono;
extern crate failure;
#[macro_use]
extern crate failure_derive;
#[macro_use]
extern crate lazy_static;
extern crate regex;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate hmac;
extern crate serde_json;
extern crate sha1;
extern crate sha2;
extern crate uuid;

#[allow(unused_imports)]
#[macro_use]
extern crate marshal_derive;

#[cfg(test)]
extern crate difference;

#[macro_use]
mod macros;

mod builtinrules;
mod chunk;
mod common;
mod meta;
mod meta_ser;
mod processor;
mod rule;
mod tracked;
mod utils;

#[cfg(test)]
mod tests;

pub mod protocol;
pub use {chunk::*, meta::*, processor::*, rule::*};
