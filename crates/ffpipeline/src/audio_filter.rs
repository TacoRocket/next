use std::time::Duration;

use crate::output_settings::AudioLoudnessSettings;
use crate::pipeline::{FrameState, Hz};

#[derive(Debug, Clone)]
pub enum AudioFilter {
    CreditSqueeze {
        layer_index: usize,
        stream_index: Option<u32>,
        start: Duration,
        channels: u32,
    },
    Resample,
    Pad,
    LoudNorm {
        settings: Option<AudioLoudnessSettings>,
        sample_rate: Option<Hz>,
        /// Channel layout to downmix to before `loudnorm`, so the true peak limit applies to
        /// the delivered channels.
        downmix: Option<&'static str>,
    },
}

impl AudioFilter {
    /// Determines whether the filter is needed given the input frame state. If so, the filter
    /// and its output frame state will be returned.
    pub(crate) fn evaluate(&self, _state: &FrameState) -> Option<AudioFilter> {
        Some(self.clone())
    }

    pub(crate) fn apply_to(&self, _state: &mut FrameState) {}

    pub(crate) fn as_arg(&self) -> Option<String> {
        match self {
            AudioFilter::CreditSqueeze { .. } => None,
            AudioFilter::Resample => Some(String::from("aresample=async=1")),
            AudioFilter::Pad => Some(String::from("apad")),
            AudioFilter::LoudNorm {
                settings,
                sample_rate,
                downmix,
            } => settings.as_ref().map(|s| {
                format!(
                    "{}loudnorm=I={}:TP={}:LRA={},aresample={}",
                    downmix
                        .map(|layout| format!("aformat=channel_layouts={layout},"))
                        .unwrap_or_default(),
                    s.integrated_target,
                    s.true_peak,
                    s.range_target,
                    sample_rate.unwrap_or(Hz(48_000)).0,
                )
            }),
        }
    }
}
