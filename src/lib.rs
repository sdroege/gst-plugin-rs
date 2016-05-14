#![crate_type="dylib"]

extern crate libc;
extern crate url;

#[macro_use]
pub mod utils;
pub mod rssource;
pub mod rsfilesrc;
pub mod rsfilesink;
