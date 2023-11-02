// SPDX-License-Identifier: MPL-2.0

mod config;
mod packet_header;

pub mod depay;
pub mod pay;

#[allow(clippy::module_inception)]
#[cfg(test)]
mod tests;
