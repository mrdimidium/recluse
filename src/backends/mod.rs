// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod go;
pub mod zig;

pub use go::{GoBackend, GoConfig};
pub use zig::{ZigBackend, ZigConfig};

/// Trait for backend-specific logic (parsing, URL building).
pub trait Backend: Send + Sync + 'static {
    /// Fixed unique identifier for storage
    const ID: &'static str;

    /// Validates filename and returns the upstream URL.
    fn upstream_url(&self, filename: &str) -> Result<String, ()>;
}
