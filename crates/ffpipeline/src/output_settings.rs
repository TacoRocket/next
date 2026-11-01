use crate::frame_rate::FrameRate;
use crate::frame_size::FrameSize;
use crate::hw_accel::HardwareAccel;
use crate::output_format::OutputFormat;
use crate::pipeline::{AudioFormat, EncodeFormat, Hz, Kbps, PtsOffset, VideoFormat};

#[derive(Debug)]
pub struct OutputSettings {
    pub audio: AudioOutputSettings,
    pub video: VideoOutputSettings,
    pub accel: Option<HardwareAccel>,
    /// The accel the channel asked for when it could not be set up, by name. The pipeline runs on
    /// the cpu and reports that against it.
    pub unavailable_accel: Option<String>,
    pub format: OutputFormat,
    pub pts_offset: Option<PtsOffset>,
    pub realtime: bool,
    pub is_live: bool,
    pub frame_rate: Option<FrameRate>,
    pub subtitle_mode: SubtitleMode,
    pub fonts_folder: Option<String>,
    pub subtitle_force_style: Option<String>,
    pub reports_folder: Option<String>,
    pub report_id: Option<String>,
}

/// Source codecs a stream may copy; items that can't be copied use the transcode settings.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyPolicy<T> {
    pub formats: Vec<T>,
}

impl Default for CopyPolicy<VideoFormat> {
    fn default() -> Self {
        Self {
            formats: vec![VideoFormat::H264, VideoFormat::Hevc],
        }
    }
}

/// ffprobe codec names
impl Default for CopyPolicy<String> {
    fn default() -> Self {
        Self {
            formats: ["aac", "ac3", "eac3", "mp3"].map(String::from).to_vec(),
        }
    }
}

#[derive(Debug)]
pub struct VideoOutputSettings {
    /// `None` always transcodes
    pub copy: Option<CopyPolicy<VideoFormat>>,
    pub transcode: VideoTranscodeSettings,
}

#[derive(Debug)]
pub struct VideoTranscodeSettings {
    pub format: EncodeFormat,
    pub bit_depth: u8,
    pub hdr_vaapi_lut: bool,
    pub bitrate: Option<Kbps>,
    pub buffer: Option<Kbps>,
    pub size: Option<FrameSize>,
    pub scaling_mode: ScalingMode,
    pub deinterlace: bool,
    pub filter_options: VideoFilterOptions,
}

#[derive(Debug, Default)]
pub struct VideoFilterOptions {
    pub bwdif: BwdifOptions,
    pub bwdif_cuda: BwdifCudaOptions,
    pub deinterlace_qsv: DeinterlaceQsvOptions,
    pub deinterlace_vaapi: DeinterlaceVaapiOptions,
    pub libplacebo: LibplaceboOptions,
    pub scale: ScaleOptions,
    pub tonemap: TonemapOptions,
    pub tonemap_opencl: TonemapOpenclOptions,
    pub w3fdif: W3fdifOptions,
    pub yadif: YadifOptions,
    pub yadif_cuda: YadifCudaOptions,
    /// Drawn while scaling to the output picture; see [`CustomShader`].
    pub custom_shader: Option<CustomShader>,
}

/// A libplacebo user shader (mpv-style `//!HOOK` blocks). It is passed inline as
/// `custom_shader_bin`, so no shader file is needed.
#[derive(Clone, PartialEq)]
pub struct CustomShader {
    hex: std::sync::Arc<str>,
    input_height: Option<u32>,
    options: CustomShaderOptions,
}

/// How the custom shader stage runs. Each option applies only where ffmpeg supports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CustomShaderOptions {
    /// `scale_vaapi` `mode` for the input height scale, e.g. `default` for the driver's scaler.
    pub input_scale_mode: Option<String>,
    /// libplacebo writes nv12 straight into the output frame (`nv12_output=native`) instead of
    /// going through `scale_vulkan`, when the picture fills the frame.
    pub native_nv12_output: bool,
    /// With native nv12 output, point-sample the output chroma instead of a Mitchell filter.
    pub point_chroma: bool,
    /// When native nv12 output doesn't apply (the picture doesn't fill the frame), still hand
    /// libplacebo's nv12 straight to VAAPI without `scale_vulkan`.
    pub direct_nv12_handoff: bool,
    /// Draw HDR10 sources too: scale to the input height in 10-bit, tone-map with
    /// `tonemap_vaapi_lut`, then draw. Needs an input height.
    pub hdr_vaapi_lut: bool,
}

impl CustomShader {
    pub fn new(glsl: &str) -> Self {
        use std::fmt::Write;
        let mut hex = String::with_capacity(glsl.len() * 2);
        for byte in glsl.bytes() {
            let _ = write!(hex, "{byte:02x}");
        }
        Self {
            hex: hex.into(),
            input_height: None,
            options: CustomShaderOptions::default(),
        }
    }

    pub fn with_options(self, options: CustomShaderOptions) -> Self {
        Self { options, ..self }
    }

    pub(crate) fn options(&self) -> &CustomShaderOptions {
        &self.options
    }

    /// Scale the picture to this many lines, keeping its shape, before the shader draws it.
    pub fn with_input_height(self, input_height: Option<u32>) -> Self {
        Self {
            input_height,
            ..self
        }
    }

    pub(crate) fn hex(&self) -> &str {
        &self.hex
    }

    pub(crate) fn input_height(&self) -> Option<u32> {
        self.input_height
    }
}

impl std::fmt::Debug for CustomShader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CustomShader({} bytes, input height {:?}, {:?})",
            self.hex.len() / 2,
            self.input_height,
            self.options
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct BwdifOptions {
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DeinterlaceQsvOptions {
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DeinterlaceVaapiOptions {
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BwdifCudaOptions {
    pub mode: Option<String>,
}

#[derive(Debug, Default)]
pub struct LibplaceboOptions {
    pub tonemapping: Option<String>,
}

#[derive(Debug, Default)]
pub struct ScaleOptions {
    pub flags: Option<String>,
}

#[derive(Debug, Default)]
pub struct TonemapOptions {
    pub tonemap: Option<String>,
}

#[derive(Debug, Default)]
pub struct TonemapOpenclOptions {
    pub tonemap: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct W3fdifOptions {
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct YadifOptions {
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct YadifCudaOptions {
    pub mode: Option<String>,
}

#[derive(Debug)]
pub struct AudioOutputSettings {
    /// `None` always transcodes
    pub copy: Option<CopyPolicy<String>>,
    pub transcode: AudioTranscodeSettings,
}

#[derive(Debug)]
pub struct AudioTranscodeSettings {
    pub format: AudioFormat,
    pub bitrate: Option<Kbps>,
    pub buffer: Option<Kbps>,
    pub channels: Option<u32>,
    pub sample_rate: Option<Hz>,
    pub loudness: Option<AudioLoudnessSettings>,
}

#[derive(Debug, Clone)]
pub struct AudioLoudnessSettings {
    pub integrated_target: f64,
    pub range_target: f64,
    pub true_peak: f64,
}

impl Default for AudioLoudnessSettings {
    fn default() -> Self {
        Self {
            integrated_target: -16f64,
            true_peak: -1.5f64,
            range_target: 11f64,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum SubtitleMode {
    Burn,
    Convert,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ScalingMode {
    ScaleAndPad,
    Stretch,
    Crop,
}
