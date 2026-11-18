// Copyright 2020 - developers of the `grammers` project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

mod tcp;

pub use tcp::NetStream;

/// Represents a socket address which may be proxied.
#[derive(Clone)]
pub enum ServerAddr {
    /// Socket address whose connection should be proxied.
    #[cfg(feature = "proxy")]
    Proxied {
        address: std::net::SocketAddr,
        proxy: String,
    },
    /// Proxy address for direct connection.
    Tcp { address: std::net::SocketAddr },
}

impl std::fmt::Debug for ServerAddr {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "proxy")]
            Self::Proxied { address, .. } => formatter
                .debug_struct("Proxied")
                .field("address", address)
                .finish_non_exhaustive(),
            Self::Tcp { address } => formatter
                .debug_struct("Tcp")
                .field("address", address)
                .finish(),
        }
    }
}
