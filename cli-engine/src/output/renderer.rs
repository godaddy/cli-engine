use std::{io::Write, str::FromStr};

use crate::{CliCoreError, DetailedError, Result};

use super::{
    Envelope, build_detailed_error_envelope, build_error_envelope, render_human, render_json,
    render_toon,
};

/// Supported output formats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    /// TOON renderer.
    Toon,
    /// Pretty JSON renderer.
    Json,
    /// Human-readable terminal renderer.
    Human,
}

impl FromStr for OutputFormat {
    type Err = CliCoreError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value == "human" {
            Ok(Self::Human)
        } else if value == "toon" {
            Ok(Self::Toon)
        } else {
            Ok(Self::Json)
        }
    }
}

/// Returns true when `format` is supported by the framework.
#[must_use]
pub fn is_valid_output_format(format: &str) -> bool {
    matches!(format, "toon" | "json" | "human")
}

/// Small rendering facade for callers that prefer an object-style renderer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RendererFactory;

impl RendererFactory {
    /// Creates a renderer factory.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Renders an envelope in the requested format.
    pub fn render(&self, format: &str, envelope: &Envelope) -> Result<String> {
        render_format(format, envelope)
    }

    /// Writes rendered envelope output to a writer.
    pub fn write(&self, mut writer: impl Write, format: &str, envelope: &Envelope) -> Result<()> {
        writer
            .write_all(self.render(format, envelope)?.as_bytes())
            .map_err(CliCoreError::from)
    }
}

/// Renders an envelope in the requested format.
pub fn render(format: OutputFormat, envelope: &Envelope) -> Result<String> {
    match format {
        OutputFormat::Human => {
            envelope.serialization_result()?;
            Ok(render_human(envelope))
        }
        OutputFormat::Json => render_json(envelope),
        OutputFormat::Toon => render_toon(envelope),
    }
}

/// Parses an output format string and renders an envelope.
pub fn render_format(format: &str, envelope: &Envelope) -> Result<String> {
    render(format.parse()?, envelope)
}

/// Writes rendered envelope output to a writer.
pub fn write_render(mut writer: impl Write, format: &str, envelope: &Envelope) -> Result<()> {
    writer
        .write_all(render_format(format, envelope)?.as_bytes())
        .map_err(CliCoreError::from)
}

/// Wraps data in a success envelope and renders it.
pub fn render_data(
    format: OutputFormat,
    data: impl serde::Serialize,
    system: impl Into<String>,
) -> Result<String> {
    let data = serde_json::to_value(data)?;
    render(format, &Envelope::success(data, system))
}

/// Parses an output format string, wraps data in a success envelope, and renders it.
pub fn render_data_format(
    format: &str,
    data: impl serde::Serialize,
    system: impl Into<String>,
) -> Result<String> {
    let data = serde_json::to_value(data)?;
    render_format(format, &Envelope::success(data, system))
}

/// Wraps an error in an error envelope and renders it.
pub fn render_error(
    format: OutputFormat,
    err: &(dyn std::error::Error + 'static),
    system: &str,
) -> Result<String> {
    render(format, &build_error_envelope(err, system))
}

/// Parses an output format string, wraps an error, and renders it.
pub fn render_error_format(
    format: &str,
    err: &(dyn std::error::Error + 'static),
    system: &str,
) -> Result<String> {
    render_format(format, &build_error_envelope(err, system))
}

/// Wraps a detailed error in an error envelope and renders it.
pub fn render_detailed_error(
    format: OutputFormat,
    err: &dyn DetailedError,
    system: &str,
) -> Result<String> {
    render(format, &build_detailed_error_envelope(err, system))
}

/// Parses an output format string, wraps a detailed error, and renders it.
pub fn render_detailed_error_format(
    format: &str,
    err: &dyn DetailedError,
    system: &str,
) -> Result<String> {
    render_format(format, &build_detailed_error_envelope(err, system))
}
