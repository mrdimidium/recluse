// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod serde;

#[derive(Debug, Clone, Copy)]
pub enum ContentType {
    TextPlain,
    OctetStream,
    ImageXIcon,
    ImagePng,
    ImageSvgXml,
    TextCss,
    FontWoff2,
    ApplicationManifestJson,
}

impl ContentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TextPlain => "text/plain",
            Self::OctetStream => "application/octet-stream",
            Self::ImageXIcon => "image/x-icon",
            Self::ImagePng => "image/png",
            Self::ImageSvgXml => "image/svg+xml",
            Self::TextCss => "text/css",
            Self::FontWoff2 => "font/woff2",
            Self::ApplicationManifestJson => "application/manifest+json",
        }
    }
}
