#[cfg(not(feature = "clap"))]
mod default_args;
#[cfg(not(feature = "clap"))]
pub use default_args::*;

#[cfg(feature = "clap")]
mod clap_args;
#[cfg(feature = "clap")]
pub use clap_args::*;
