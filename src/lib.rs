//! photo-clearout: a local web tool for clearing out old photos in Immich.
//!
//! The browser only ever talks to this program on 127.0.0.1. This program
//! holds the Immich API key and talks to Immich on the browser's behalf.

pub mod config;
pub mod immich;
pub mod paging;
pub mod progress;
pub mod tunnel;
pub mod web;
