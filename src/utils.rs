// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::net::SocketAddr;
use std::time::Duration;

use serde::de::{self, Deserialize, Visitor};

/// Deserializes a duration from seconds (u64).
pub fn deserialize_duration_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let secs = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(secs))
}

/// Deserializes a u64 from either a number or a string.
pub fn deserialize_size<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct SizeVisitor;
    impl<'de> Visitor<'de> for SizeVisitor {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a number or string")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            v.parse().map_err(de::Error::custom)
        }
    }

    deserializer.deserialize_any(SizeVisitor)
}

/// Deserializes a SocketAddr from a string.
pub fn deserialize_listener_addr<'de, D>(deserializer: D) -> Result<SocketAddr, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    let raw = raw.trim();

    // Hostnames are not supported, but localhost shorthands are useful
    if let Some(port) = raw.strip_prefix("localhost:") {
        return port
            .parse::<u16>()
            .map(|port| SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port))
            .map_err(|err| serde::de::Error::custom(format!("invalid port in '{raw}': {err}")));
    }

    raw.parse::<SocketAddr>()
        .map_err(|err| serde::de::Error::custom(format!("invalid address '{raw}': {err}",)))
}
