use serde::Serialize;

use crate::hw_accel::{HardwareAccel, HwAccel};
use crate::pipeline::EncodeFormat;

/// A way one item's pipeline runs other than as configured. The pipeline records the choice where
/// it makes it, so a frontend can report it instead of reading the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradationCode {
    HardwareDecodeUnavailable,
    HardwareEncodeUnavailable,
    HardwareTonemapUnavailable,
    /// `vaapi_low_power: "always"` and the device has no low-power entrypoint for this output
    VaapiLowPowerUnavailable,
    /// The custom shader is not drawn on this HDR source
    CustomShaderHdrSkipped,
    /// The custom shader needs an accel that can draw it
    CustomShaderUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PipelineDegradation {
    pub code: DegradationCode,
    pub message: String,
}

/// One entry per code, in the order they were recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Degradations(Vec<PipelineDegradation>);

impl Degradations {
    pub(crate) fn add(&mut self, code: DegradationCode, message: String) {
        if !self.0.iter().any(|f| f.code == code) {
            self.0.push(PipelineDegradation { code, message });
        }
    }

    pub(crate) fn as_slice(&self) -> &[PipelineDegradation] {
        &self.0
    }
}

pub(crate) fn accel_name(accel: &HardwareAccel) -> String {
    accel
        .known_accel()
        .map(|a| a.to_string().to_uppercase())
        .unwrap_or_else(|| String::from("hardware"))
}

pub(crate) fn format_name(format: &EncodeFormat) -> &'static str {
    match format {
        EncodeFormat::H264 => "H.264",
        EncodeFormat::Hevc => "HEVC",
        EncodeFormat::Mpeg2Video => "MPEG-2",
    }
}
