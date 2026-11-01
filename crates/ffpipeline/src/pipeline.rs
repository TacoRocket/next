use std::fmt::Formatter;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use strum::{Display, EnumString};

use crate::ArgVec;
use crate::audio_codec::AudioCodec;
use crate::audio_decoder::AudioDecoder;
use crate::audio_filter::AudioFilter;
use crate::color::FrameColor;
use crate::copy_decision::{
    CopyBlocker, CopyDecision, CopyDecisions, VideoCopyContext, audio_copy_decision,
    video_copy_decision,
};
use crate::credit_squeeze::{CreditSqueeze, CreditSqueezeBranches, CreditSqueezeColor};
use crate::degradation::{
    DegradationCode, Degradations, PipelineDegradation, accel_name, format_name,
};
use crate::error::FFPipelineError;
use crate::ffmpeg_info::{FfmpegInfo, KnownVideoFilter};
use crate::filter_chain::{EncoderTarget, FilterChain, PipelineFilter};
use crate::frame_rate::FrameRate;
use crate::frame_size::{FrameSize, parse_aspect_ratio};
use crate::global_option::{GlobalOption, LogLevel};
use crate::hw_accel::{HardwareAccel, HwAccel};
use crate::input::{
    CanvasLayout, FfmpegInputArgs, FfmpegInputRequestContext, GraphicsInput, GraphicsKind,
    InputSettings, InputSource, ProbedInput,
};
use crate::output_option::OutputOption;
use crate::output_settings::{
    OutputSettings, ScalingMode, SubtitleMode, ToneMappingMode, VideoFilterOptions, YadifOptions,
};
use crate::overlay_filter::{
    FramePoint, OverlayFilter, OverlayKind, OverlaySource, SoftwareOverlay,
};
use crate::probe::{CodecType, ProbeResultAudioStream, ProbeResultStream, ProbeResultVideoStream};
use crate::video_codec::{MetadataBsf, VideoCodec, VideoEncoder};
use crate::video_decoder::VideoDecoder;
use crate::video_filter::{
    ClipFilter, ColorChannelMixerFilter, CropFilter, DeinterlaceFilter, Dv5WorkaroundFilter,
    EnsureAlphaFilter, FadeFilter, FormatFilter, FpsFilter, LoopFilter, PadFilter, ScaleFilter,
    SetSarFilter, ShaderFilter, SoftwareDeinterlaceFilter, SoftwareDeinterlaceOptions,
    SubtitleImageScaleFilter, SubtitlesFilter, ToneMapFilter, TransposeDir, TransposeFilter,
    VideoFilter,
};

pub const KEYFRAME_INTERVAL_SECONDS: u32 = 2;
pub const SEGMENT_SECONDS: u32 = 4;

#[derive(Debug, Clone, Copy, Display, EnumString)]
#[strum(serialize_all = "lowercase")]
pub enum AudioFormat {
    Aac,
    Ac3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kbps(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hz(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, Serialize)]
#[strum(serialize_all = "lowercase")]
pub enum VideoFormat {
    Av1,
    H264,
    Hevc,
    Mpeg2Video,
    Vc1,
    Vp8,
    Vp9,
}

impl VideoFormat {
    /// other codecs carry interlaced content as progressive pictures
    pub fn has_interlaced_coding(&self) -> bool {
        matches!(
            self,
            VideoFormat::H264 | VideoFormat::Mpeg2Video | VideoFormat::Vc1
        )
    }
}

/// The formats next can encode. `VideoFormat` identifies a codec (decode, capability probes);
/// this is the subset a channel can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, Serialize)]
#[strum(serialize_all = "lowercase")]
pub enum EncodeFormat {
    H264,
    Hevc,
    Mpeg2Video,
}

impl From<EncodeFormat> for VideoFormat {
    fn from(value: EncodeFormat) -> Self {
        match value {
            EncodeFormat::H264 => VideoFormat::H264,
            EncodeFormat::Hevc => VideoFormat::Hevc,
            EncodeFormat::Mpeg2Video => VideoFormat::Mpeg2Video,
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct PtsOffset {
    pub duration: Duration,
    pub audio_end: Option<Duration>,
}

impl Default for PtsOffset {
    fn default() -> Self {
        PtsOffset {
            duration: Duration::ZERO,
            audio_end: None,
        }
    }
}

impl PtsOffset {
    pub(crate) fn audio_packet_floor(self) -> Option<f64> {
        // Bitstream filters see packet time before the muxer adds the offset.
        // Match TsOffset's millisecond precision, including copy-seek correction.
        self.audio_end
            .map(|end| end.as_secs_f64() - self.duration.as_millis() as f64 / 1000.0)
    }
}

enum SubtitleBurn<'a> {
    Image {
        stream: &'a ProbeResultVideoStream,
        input: &'a ProbedInput,
        size: FrameSize,
    },
    Text {
        stream: &'a ProbeResultVideoStream,
        input: &'a ProbedInput,
    },
}

pub(crate) struct OutputContext {
    pub(crate) frame_rate: FrameRate,
    pub(crate) audio_codec: AudioCodec,
    pub(crate) audio_channels: Option<u32>,
    pub(crate) video_encoder: VideoEncoder,
    pub(crate) pts_offset: Option<PtsOffset>,
    pub(crate) encoder_target: EncoderTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum FrameSurface {
    System,
    Amf,
    Cuda,
    Qsv,
    Rkmpp,
    Vaapi,
    VideoToolbox,
    Vulkan,
    OpenCL,
}

impl FrameSurface {
    pub(crate) fn device_name(&self) -> Option<&'static str> {
        match self {
            FrameSurface::Amf => Some("amf"),
            FrameSurface::Cuda => Some("cuda"),
            FrameSurface::OpenCL => Some("opencl"),
            FrameSurface::Qsv => Some("qsv"),
            FrameSurface::Rkmpp => Some("rkmpp"),
            FrameSurface::Vaapi => Some("vaapi"),
            FrameSurface::Vulkan => Some("vulkan"),
            FrameSurface::VideoToolbox => Some("videotoolbox"),
            FrameSurface::System => None,
        }
    }
}

pub type SurfaceSet = std::collections::HashSet<FrameSurface>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Rgba,
    Yuv420p,
    Yuv420p10le,
    Yuva420p,
    Yuva420p10le,
    Nv12,
    Nv15,
    P010le,
    P016,
    /// never a conversion target
    Unknown,
}

gen_subset!(HwPixelFormat, PixelFormat, Nv12, Nv15, P010le);

impl PixelFormat {
    pub fn parse(pix_fmt: &str) -> PixelFormat {
        match pix_fmt.to_lowercase().as_str() {
            "bgra" => PixelFormat::Bgra,
            "rgba" => PixelFormat::Rgba,
            "yuv420p" => PixelFormat::Yuv420p,
            "yuv420p10le" => PixelFormat::Yuv420p10le,
            "yuva420p" => PixelFormat::Yuva420p,
            "yuva420p10le" => PixelFormat::Yuva420p10le,
            "nv12" => PixelFormat::Nv12,
            "nv15" => PixelFormat::Nv15,
            "p010le" => PixelFormat::P010le,
            _ => {
                log::debug!("unknown pixel format {}", pix_fmt);
                PixelFormat::Unknown
            }
        }
    }

    pub(crate) fn bit_depth(&self) -> u8 {
        match self {
            PixelFormat::Bgra
            | PixelFormat::Rgba
            | PixelFormat::Yuv420p
            | PixelFormat::Yuva420p
            | PixelFormat::Nv12
            | PixelFormat::Unknown => 8,
            PixelFormat::Yuv420p10le
            | PixelFormat::Yuva420p10le
            | PixelFormat::P010le
            | PixelFormat::Nv15 => 10,
            PixelFormat::P016 => 16,
        }
    }

    pub(crate) fn has_alpha(&self) -> bool {
        matches!(
            self,
            PixelFormat::Bgra
                | PixelFormat::Rgba
                | PixelFormat::Yuva420p
                | PixelFormat::Yuva420p10le
        )
    }

    pub(crate) fn as_arg(&self) -> &str {
        match self {
            PixelFormat::Bgra => "bgra",
            PixelFormat::Rgba => "rgba",
            PixelFormat::Yuv420p => "yuv420p",
            PixelFormat::Yuv420p10le => "yuv420p10le",
            PixelFormat::Yuva420p => "yuva420p",
            PixelFormat::Yuva420p10le => "yuva420p10le",
            PixelFormat::Nv12 => "nv12",
            PixelFormat::Nv15 => "nv15",
            PixelFormat::P010le => "p010le",
            PixelFormat::P016 => "p016",
            PixelFormat::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HdrFormat {
    None,
    /// PQ without metadata
    Pq,
    /// PQ with metadata
    Hdr10,
    Hlg,
    Dv5,
}

#[derive(Clone, Debug, derive_more::Display)]
#[display(
    "FrameState(size={},is_anamorphic={},surface={})",
    size,
    is_anamorphic,
    surface
)]
pub struct FrameState {
    pub(crate) size: FrameSize,
    pub(crate) is_anamorphic: bool,
    pub(crate) is_interlaced: bool,
    pub(crate) sample_aspect_ratio: Option<String>,
    pub(crate) display_aspect_ratio: Option<String>,
    pub(crate) surface: FrameSurface,
    pub(crate) pixel_format: PixelFormat,
    pub(crate) color: FrameColor,
    pub(crate) hdr_format: HdrFormat,
    pub(crate) rotation: Option<i32>,
    pub(crate) decoder_frames: bool,
    /// a filter has tagged frames 1:1; until then they carry the container's SAR, which can
    /// disagree with `sample_aspect_ratio` when that came from a hint
    pub(crate) sar_written: bool,
}

impl FrameState {
    pub(crate) fn set_square_pixels(&mut self) {
        self.is_anamorphic = false;
        self.sample_aspect_ratio = Some(String::from("1:1"));
        self.display_aspect_ratio = None;
        self.sar_written = true;
    }

    pub(crate) fn apply_rotation(&mut self) {
        if let Some(dir) = self.rotation.and_then(TransposeDir::from_rotation)
            && dir.is_quarter_turn()
        {
            std::mem::swap(&mut self.size.width, &mut self.size.height);
            // rotation does not make pixels square. hints may omit SAR, so keep DAR.
            for ratio in [
                &mut self.sample_aspect_ratio,
                &mut self.display_aspect_ratio,
            ] {
                *ratio = ratio
                    .as_deref()
                    .and_then(parse_aspect_ratio)
                    .map(|value| (1.0 / value).to_string());
            }
        }

        self.rotation = None;
    }

    /// All tonemap filters output SDR BT.709.
    pub(crate) fn apply_tonemap(&mut self) {
        self.hdr_format = HdrFormat::None;
        self.color = FrameColor::bt709();
    }
}

pub enum PipelineInput {
    Audio {
        input_source: InputSource,
        index: u32,
        path: String,
        seek: Duration,
        channels: u32,
        decoder: AudioDecoder,
        /// Open the file again for audio, even when it is also the video input.
        own_input: bool,
        /// A second demuxer of this file keeps the video's start time.
        own_input_keeps_sync: bool,
    },
    Video {
        input_source: InputSource,
        index: u32,
        path: String,
        seek: Duration,
        realtime: bool,
        decoder: VideoDecoder,
    },
    Subtitle {
        input_source: InputSource,
        index: u32,
        path: String,
        seek: Duration,
        canvas_size: Option<FrameSize>,
    },
    Graphics {
        input: GraphicsInput,
        layer_index: usize,
        index: u32,
        path: String,
        extra_input_args: ArgVec,
    },
}

impl PipelineInput {
    fn sort_order(&self) -> usize {
        match self {
            PipelineInput::Video { .. } => 0,
            PipelineInput::Audio { .. } => 1,
            PipelineInput::Subtitle { .. } => 2,
            PipelineInput::Graphics { layer_index, .. } => 3 + *layer_index,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnvironmentVariable {
    pub key: String,
    pub value: String,
}

pub struct Pipeline {
    ffmpeg_info: FfmpegInfo,
    copy_decisions: CopyDecisions,
    accel: Option<HardwareAccel>,
    filter_options: VideoFilterOptions,
    initial_state: FrameState,

    global_options: Vec<GlobalOption>,
    inputs: Vec<PipelineInput>,
    filter_chain: FilterChain,
    output_options: Vec<OutputOption>,
    env_vars: Vec<EnvironmentVariable>,
    degradations: Degradations,

    input_request_context: FfmpegInputRequestContext,
    output_context: OutputContext,
    output_sample_aspect_ratio: Option<(u64, u64)>,
}

impl Pipeline {
    fn full(
        ffmpeg_info: &FfmpegInfo,
        mut input_settings: InputSettings,
        output_settings: OutputSettings,
    ) -> Result<Pipeline, FFPipelineError> {
        let mut final_output_settings = output_settings;
        let mut degradations = Degradations::default();

        if let Some(silence) = input_settings.audio_input.silence_if_missing() {
            log::info!(
                "source {} has no audio stream; using silence",
                input_settings.audio_input.probe_result.path
            );
            input_settings.audio_input = silence;
        }

        if let Some(accel) = &final_output_settings.accel
            && accel
                .known_accel()
                .map(|a| !ffmpeg_info.has_hw_accel(a))
                .unwrap_or(false)
        {
            log::warn!("ffmpeg does not support requested accel {:?}", accel);
            final_output_settings.unavailable_accel = Some(accel_name(accel));
            final_output_settings.accel = None;
        }
        // the accel this channel asked for, whether or not it could be used
        let requested_accel = final_output_settings
            .accel
            .as_ref()
            .map(accel_name)
            .or_else(|| final_output_settings.unavailable_accel.clone());
        let accel_usable = final_output_settings.accel.is_some();
        let unusable = move |what: &str, name: &str| {
            if accel_usable {
                format!("{name} hardware {what} cannot be used")
            } else {
                format!("{name} is unavailable to this channel, so hardware {what} cannot be used")
            }
        };

        let video_transcode = &mut final_output_settings.video.transcode;
        if video_transcode.format == EncodeFormat::Mpeg2Video && video_transcode.bit_depth == 10 {
            log::debug!("mpeg2video does not support 10-bit output, using 8-bit");
            video_transcode.bit_depth = 8;
        }

        let mut duration = std::cmp::min(
            input_settings.audio_input.out_point - input_settings.audio_input.in_point,
            input_settings.video_input.out_point - input_settings.video_input.in_point,
        );

        let video_stream = input_settings.select_video_stream()?;
        let audio_stream = input_settings.select_audio_stream()?;
        let graphics_streams: Vec<_> = input_settings
            .graphics_inputs
            .iter()
            .map(|input| input_settings.select_graphics_stream(input))
            .collect();

        let video_transcode = &final_output_settings.video.transcode;
        let is_still_image = input_settings.video_input.probe_result.is_still_image();

        let original_subtitle_burn =
            subtitle_burn(&input_settings, &final_output_settings, video_stream);
        let copy_decisions = copy_decisions(
            &input_settings,
            &final_output_settings,
            video_stream,
            &input_settings.audio_input,
            audio_stream,
            original_subtitle_burn.as_ref(),
        );

        let audio_codec = match (
            &copy_decisions.audio,
            final_output_settings.audio.transcode.format,
        ) {
            (Some(CopyDecision::Copy), _) => AudioCodec::Copy,
            (_, AudioFormat::Aac) => AudioCodec::Aac,
            (_, AudioFormat::Ac3) => AudioCodec::Ac3,
        };

        // TODO: add target profile to config
        let video_encoder = match &copy_decisions.video {
            Some(CopyDecision::Copy) => VideoEncoder::Copy,
            _ => {
                let format = video_transcode.format;
                let bit_depth = video_transcode.bit_depth;
                let hardware_codec = final_output_settings
                    .accel
                    .as_ref()
                    .filter(|a| a.can_encode(&format, bit_depth))
                    .and_then(|a| a.codec_for_format(&format, bit_depth, video_transcode.size));
                match (&final_output_settings.accel, &hardware_codec) {
                    (_, None) => {
                        if let Some(name) = &requested_accel {
                            degradations.add(
                                DegradationCode::HardwareEncodeUnavailable,
                                format!(
                                    "{} for {}-bit {} output. Using CPU encoding.",
                                    unusable("encoding", name),
                                    bit_depth,
                                    format_name(&format)
                                ),
                            )
                        }
                    }
                    (Some(HardwareAccel::Vaapi(vaapi)), Some(_))
                        if vaapi.low_power_unavailable(&format, bit_depth) =>
                    {
                        degradations.add(
                            DegradationCode::VaapiLowPowerUnavailable,
                            format!(
                                "The VAAPI device has no low-power {} encoder for {}-bit output. Using regular VAAPI encoding.",
                                format_name(&format),
                                bit_depth
                            ),
                        )
                    }
                    _ => {}
                }
                VideoEncoder::Encode(hardware_codec.unwrap_or_else(|| match format {
                    EncodeFormat::H264 => VideoCodec::libx264(),
                    EncodeFormat::Hevc => VideoCodec::libx265(),
                    EncodeFormat::Mpeg2Video => VideoCodec::mpeg2video(),
                }))
            }
        };

        let mut pts_offset = final_output_settings.pts_offset;
        let mut video_seek = input_settings.video_input.in_point;
        let mut copy_output_options = Vec::new();
        if video_encoder == VideoEncoder::Copy {
            if let Some(copy_seek) = &input_settings.video_copy_seek {
                let frame_duration =
                    Duration::from_secs_f64(1.0 / video_stream.frame_rate.parsed_frame_rate);
                let args = copy_seek.args(duration, frame_duration);
                video_seek = args.input_seek;
                duration = args.duration;
                if let Some(output_seek) = args.output_seek {
                    copy_output_options.push(OutputOption::Seek(output_seek));
                }
                if let Some(offset) = pts_offset.as_mut()
                    && offset.duration > args.ts_offset_correction
                {
                    offset.duration -= args.ts_offset_correction;
                }
            }

            // ffmpeg adds SPS/PPS only before IDR slices, so an open-GOP seek or segment that
            // starts on another I-frame can't decode. HEVC adds them on every IRAP.
            if video_stream.codec == "h264" {
                copy_output_options.push(OutputOption::H264CopyParameterSets);
            }
        }

        let video_decoder = VideoDecoder::new(
            ffmpeg_info,
            video_stream,
            is_still_image,
            &video_encoder,
            final_output_settings.accel.as_ref(),
        );
        if let (VideoDecoder::Software, Some(name)) = (&video_decoder, &requested_accel) {
            degradations.add(
                DegradationCode::HardwareDecodeUnavailable,
                format!(
                    "{} for this item ({} {}, {}). Using CPU decoding.",
                    unusable("decoding", name),
                    video_stream.codec,
                    video_stream.profile,
                    video_stream.pix_fmt
                ),
            );
        }

        let hdr = match (
            video_stream.dv_profile,
            video_stream.color_params.color_transfer.as_deref(),
        ) {
            (Some(5), _) => HdrFormat::Dv5,
            (_, Some("smpte2084")) if video_stream.color_params.has_hdr10_metadata => {
                HdrFormat::Hdr10
            }
            (_, Some("smpte2084")) => HdrFormat::Pq,
            (_, Some("arib-std-b67")) => HdrFormat::Hlg,
            _ => HdrFormat::None,
        };

        let hdr_preservation_issue = if video_encoder == VideoEncoder::Copy {
            None
        } else if hdr == HdrFormat::Dv5 {
            Some("Dolby Vision profile 5 cannot be preserved by this transcode path")
        } else if hdr != HdrFormat::None && video_transcode.bit_depth != 10 {
            Some("HDR preservation needs 10-bit output, but this profile produces 8-bit video")
        } else {
            None
        };
        let tone_mapping = match (video_transcode.tone_mapping, hdr_preservation_issue) {
            (ToneMappingMode::On, _) => true,
            (ToneMappingMode::SdrFallback, Some(reason)) => {
                degradations.add(
                    DegradationCode::HdrToneMapped,
                    format!("Converted to SDR: {reason}."),
                );
                true
            }
            (ToneMappingMode::Off, Some(reason)) => {
                return Err(FFPipelineError::HdrPreservationUnsupported(format!(
                    "{reason}. Tone mapping is Off; select On or SDR fallback to play this item."
                )));
            }
            _ => false,
        };

        let credit_squeeze = input_settings.credit_squeeze.as_ref().and_then(|request| {
            let reason = if hdr != HdrFormat::None {
                Some(String::from("Credit squeeze has not been validated for HDR; playing full-screen."))
            } else if video_encoder.preferred_surface() != FrameSurface::Vaapi
                || video_transcode.bit_depth != 8
                || !matches!(&final_output_settings.accel, Some(HardwareAccel::Vaapi(vaapi)) if vaapi.can_libplacebo_shader(ffmpeg_info) && vaapi.capabilities.can_overlay)
                || !ffmpeg_info.has_video_filter(&KnownVideoFilter::OverlayVaapi)
                || !ffmpeg_info.has_video_filter(&KnownVideoFilter::ScaleVaapi)
                || !ffmpeg_info.has_video_filter_option(&KnownVideoFilter::Concat, "preserve_ts")
            {
                Some(String::from("Credit squeeze requires 8-bit VAAPI, Vulkan sharing, and timestamp-preserving concat; playing full-screen."))
            } else if let Ok(background) = &request.background {
                match input_settings.select_graphics_stream(background) {
                    None => Some(String::from("Credit squeeze background has no video stream; playing full-screen.")),
                    Some(stream) if request.advertisement.is_some() && (
                        background.probe_result.is_still_image()
                        || stream.width.is_none_or(|w| w == 0)
                        || stream.height.is_none_or(|h| h == 0)
                        || stream.dv_profile.is_some()
                        || matches!(stream.color_params.color_transfer.as_deref(), Some("smpte2084" | "arib-std-b67"))
                        || request.advertisement.as_ref().is_some_and(|ad| ad.out_point <= background.in_point)
                    ) => Some(String::from("Credit squeeze advertisement must be an SDR video with playable time remaining; playing original picture and audio.")),
                    Some(_) => None,
                }
            } else { request.background.as_ref().err().cloned() };
            if let Some(reason) = reason {
                degradations.add(DegradationCode::CreditSqueezeUnavailable, reason);
                return None;
            }
            request.background.as_ref().ok().map(|background| (request.start.saturating_sub(input_settings.playout_offset), background, request))
        });

        let initial_state = FrameState {
            size: FrameSize {
                width: video_stream
                    .width
                    .ok_or(FFPipelineError::VideoInputIsRequired)?,
                height: video_stream
                    .height
                    .ok_or(FFPipelineError::VideoInputIsRequired)?,
            },
            is_anamorphic: video_stream.is_anamorphic(),
            // if user does not want to deinterlace, pretend content is not interlaced
            is_interlaced: video_transcode.deinterlace && video_stream.is_interlaced(),
            sample_aspect_ratio: video_stream.sample_aspect_ratio.to_owned(),
            display_aspect_ratio: video_stream.display_aspect_ratio.to_owned(),
            surface: video_decoder.output_surface(),
            pixel_format: video_decoder
                .output_format(&PixelFormat::parse(video_stream.pix_fmt.as_str())),
            color: FrameColor::from(&video_stream.color_params),
            hdr_format: hdr,
            rotation: video_stream.rotation,
            decoder_frames: video_decoder.output_surface() != FrameSurface::System,
            sar_written: false,
        };

        // Compose in square pixels at the program's display shape, then fit the completed
        // image into the fixed encoded raster. Logos, subtitles and squeeze share this canvas.
        let source_aspect_geometry = (video_transcode.preserve_source_aspect
            && video_encoder != VideoEncoder::Copy)
            .then_some(video_transcode.size)
            .flatten()
            .map(|encoded| {
                let mut display_state = initial_state.clone();
                display_state.apply_rotation();
                (encoded, encoded.square_pixel_size_contain(&display_state))
            });
        if let Some((_, display)) = source_aspect_geometry {
            final_output_settings.video.transcode.size = Some(display);
            final_output_settings.video.transcode.scaling_mode = ScalingMode::ScaleAndPad;
        }
        let video_transcode = &final_output_settings.video.transcode;
        let subtitle_burn = subtitle_burn(&input_settings, &final_output_settings, video_stream);
        let output_sample_aspect_ratio = source_aspect_geometry.map(|(encoded, display)| {
            (
                u64::from(display.width) * u64::from(encoded.height),
                u64::from(display.height) * u64::from(encoded.width),
            )
        });

        // copy can't change the rate; only items already at the target are copied
        let output_frame_rate = final_output_settings
            .frame_rate
            .clone()
            .filter(|_| video_encoder != VideoEncoder::Copy);

        let output_context = OutputContext {
            audio_codec,
            audio_channels: final_output_settings.audio.transcode.channels,
            video_encoder: video_encoder.clone(),
            pts_offset,
            frame_rate: output_frame_rate
                .clone()
                .unwrap_or_else(|| video_stream.frame_rate.to_owned()),
            encoder_target: EncoderTarget {
                surface: video_encoder.preferred_surface(),
                pixel_format: video_encoder.preferred_pixel_format(video_transcode.bit_depth),
                also_accepts: video_encoder.also_accepts(video_transcode.bit_depth),
            },
        };

        let mut filters = vec![
            PipelineFilter::Audio(AudioFilter::LoudNorm {
                settings: final_output_settings.audio.transcode.loudness.clone(),
                sample_rate: final_output_settings.audio.transcode.sample_rate,
            }),
            PipelineFilter::Audio(AudioFilter::Resample),
            PipelineFilter::Audio(AudioFilter::Pad),
        ];

        filters.extend([
            PipelineFilter::Video(
                LoopFilter {
                    is_still_image,
                    loops: None,
                }
                .into(),
            ),
            PipelineFilter::Video(Dv5WorkaroundFilter.into()),
        ]);

        // tonemap first when decoded with vulkan (for libplacebo), or when not downscaling
        let source = initial_state.size;
        let tonemap_first = video_decoder.output_surface() == FrameSurface::Vulkan
            || video_transcode
                .size
                .is_none_or(|target| target.pixel_count() >= source.pixel_count());

        let tonemap = PipelineFilter::Video(
            ToneMapFilter {
                prefer_vaapi_lut: video_transcode.hdr_vaapi_lut
                    && crate::accel::vaapi::hdr10_lut_eligible(video_stream),
                algorithm: video_transcode.filter_options.tonemap.tonemap.clone(),
                output_format: match video_transcode.bit_depth {
                    10 => PixelFormat::Yuv420p10le,
                    _ => PixelFormat::Yuv420p,
                },
            }
            .into(),
        );

        let scale = ScaleFilter {
            size: video_transcode.size,
            scaling_mode: video_transcode.scaling_mode,
            input_is_anamorphic: initial_state.is_anamorphic,
            flags: video_transcode.filter_options.scale.flags.clone(),
        };

        // the shader replaces the scale where libplacebo can draw it. HDR sources keep the plain
        // scale and tonemap, except HDR10 when the stage tone-maps it itself.
        let shader_tonemaps = video_transcode
            .filter_options
            .custom_shader
            .as_ref()
            .is_some_and(|shader| {
                tone_mapping
                    && shader.options().hdr_vaapi_lut
                    && shader.input_height().is_some()
                    && matches!(hdr, HdrFormat::Hdr10 | HdrFormat::Pq)
                    && initial_state.pixel_format.bit_depth() == 10
                    && ffmpeg_info.has_video_filter(&KnownVideoFilter::TonemapVaapiLut)
            });
        let scale = match &video_transcode.filter_options.custom_shader {
            Some(_) if hdr != HdrFormat::None && !shader_tonemaps => {
                log::debug!("{hdr:?} source; not drawing the custom shader");
                degradations.add(
                    DegradationCode::CustomShaderHdrSkipped,
                    format!("The custom shader is not drawn on this {hdr:?} source."),
                );
                PipelineFilter::Video(scale.into())
            }
            Some(shader) => match &final_output_settings.accel {
                Some(HardwareAccel::Vaapi(vaapi)) if vaapi.can_libplacebo_shader(ffmpeg_info) => {
                    PipelineFilter::Video(
                        ShaderFilter {
                            scale,
                            shader: shader.clone(),
                            hdr: shader_tonemaps,
                        }
                        .into(),
                    )
                }
                _ => {
                    log::warn!(
                        "the custom shader needs vaapi with libplacebo and a vulkan device that imports its frames; not drawing it"
                    );
                    degradations.add(
                        DegradationCode::CustomShaderUnavailable,
                        String::from(
                            "The custom shader needs VAAPI with libplacebo and a Vulkan device that imports its frames; it is not drawn.",
                        ),
                    );
                    PipelineFilter::Video(scale.into())
                }
            },
            None => PipelineFilter::Video(scale.into()),
        };

        let shader_tonemaps =
            shader_tonemaps && matches!(&scale, PipelineFilter::Video(VideoFilter::Shader(_)));
        let squeeze_shader = if credit_squeeze.is_some() {
            match &scale {
                PipelineFilter::Video(VideoFilter::Shader(stage)) => Some(stage.shader.clone()),
                _ => None,
            }
        } else {
            None
        };

        let geometry_filters = [
            PipelineFilter::Video(
                DeinterlaceFilter {
                    filter: SoftwareDeinterlaceFilter::Yadif(YadifOptions::default()),
                    options: SoftwareDeinterlaceOptions {
                        bwdif: video_transcode.filter_options.bwdif.clone(),
                        w3fdif: video_transcode.filter_options.w3fdif.clone(),
                        yadif: video_transcode.filter_options.yadif.clone(),
                    },
                    input_is_interlaced: initial_state.is_interlaced,
                }
                .into(),
            ),
            // after deinterlace, which can double the rate; before the rest, so it runs at the
            // target rate
            PipelineFilter::Video(
                FpsFilter {
                    frame_rate: output_frame_rate.clone(),
                }
                .into(),
            ),
            PipelineFilter::Video(TransposeFilter::default().into()),
            scale,
            PipelineFilter::Video(
                CropFilter {
                    size: video_transcode.size,
                    scaling_mode: video_transcode.scaling_mode,
                }
                .into(),
            ),
        ];

        // always pad after tonemap; qsv on windows ignores pad color with some hdr content
        let pad = PipelineFilter::Video(
            PadFilter {
                size: video_transcode.size,
                scaling_mode: video_transcode.scaling_mode,
            }
            .into(),
        );

        if shader_tonemaps || !tone_mapping {
            filters.extend(geometry_filters);
        } else if tonemap_first {
            filters.push(tonemap);
            filters.extend(geometry_filters);
        } else {
            filters.extend(geometry_filters);
            filters.push(tonemap);
        }
        filters.push(pad);
        filters.push(PipelineFilter::Video(SetSarFilter.into()));

        let mut inputs = vec![
            PipelineInput::Audio {
                input_source: input_settings.audio_input.input_source.to_owned(),
                index: audio_stream.stream_index,
                path: input_settings.audio_input.probe_result.path.to_owned(),
                seek: input_settings.audio_input.in_point,
                channels: audio_stream.channels,
                decoder: AudioDecoder::new(audio_stream, &final_output_settings),
                // decided in optimize, once overlay surfaces are resolved
                own_input: false,
                own_input_keeps_sync: own_input_keeps_sync(
                    input_settings
                        .audio_input
                        .probe_result
                        .format_name
                        .as_deref(),
                ),
            },
            PipelineInput::Video {
                input_source: input_settings.video_input.input_source.to_owned(),
                index: video_stream.stream_index,
                path: input_settings.video_input.probe_result.path.to_owned(),
                seek: if is_still_image {
                    Duration::ZERO
                } else {
                    video_seek
                },
                realtime: final_output_settings.realtime && !final_output_settings.is_live,
                decoder: video_decoder,
            },
        ];

        match subtitle_burn {
            Some(SubtitleBurn::Image {
                stream: subtitle_stream,
                input: subtitle_input,
                size,
            }) => {
                inputs.push(PipelineInput::Subtitle {
                    input_source: subtitle_input.input_source.to_owned(),
                    index: subtitle_stream.stream_index,
                    path: subtitle_input.probe_result.path.to_owned(),
                    seek: subtitle_input.in_point,
                    canvas_size: unknown_pgs_canvas_size(subtitle_stream),
                });

                let secondary_initial_state = FrameState {
                    size,
                    is_anamorphic: subtitle_stream.is_anamorphic(),
                    is_interlaced: false,
                    sample_aspect_ratio: subtitle_stream.sample_aspect_ratio.to_owned(),
                    display_aspect_ratio: subtitle_stream.display_aspect_ratio.to_owned(),
                    surface: FrameSurface::System,
                    pixel_format: if subtitle_stream.pix_fmt.is_empty() {
                        PixelFormat::Bgra
                    } else {
                        PixelFormat::parse(&subtitle_stream.pix_fmt)
                    },
                    color: FrameColor::from(&subtitle_stream.color_params),
                    hdr_format: HdrFormat::None,
                    rotation: None,
                    decoder_frames: false,
                    sar_written: false,
                };

                filters.push(PipelineFilter::Overlay(OverlayFilter {
                    kind: SoftwareOverlay::default().into(),
                    secondary: vec![
                        // Sub2video heartbeats can outpace the video; discard redundant
                        // PGS frames before scaling and uploading the subtitle canvas.
                        FpsFilter {
                            frame_rate: subtitle_stream
                                .is_pgs_subtitle()
                                .then(|| {
                                    FrameRate::parse_target(&output_context.frame_rate.r_frame_rate)
                                })
                                .flatten(),
                        }
                        .into(),
                        SubtitleImageScaleFilter {
                            size,
                            flags: video_transcode.filter_options.scale.flags.clone(),
                        }
                        .into(),
                    ],
                    secondary_initial_state,
                    secondary_source: OverlaySource::Subtitle,
                    location: None,
                }));
            }
            Some(SubtitleBurn::Text {
                stream: subtitle_stream,
                input: subtitle_input,
            }) => {
                // only use force_style with SRT, which doesn't have any styling of its own
                let mut final_force_style = None;
                if subtitle_stream.codec == "srt" || subtitle_stream.codec == "subrip" {
                    final_force_style = final_output_settings.subtitle_force_style;
                }

                let subtitle_index = subtitle_input
                    .probe_result
                    .streams
                    .iter()
                    .filter_map(|s| match s {
                        ProbeResultStream::Video(v) if v.codec_type == CodecType::Subtitle => {
                            Some(v.stream_index)
                        }
                        _ => None,
                    })
                    .position(|index| index == subtitle_stream.stream_index)
                    .unwrap_or(0);

                filters.push(PipelineFilter::Video(
                    SubtitlesFilter {
                        path: subtitle_input.probe_result.path.to_owned(),
                        subtitle_index,
                        seek: subtitle_input.in_point,
                        preserve_timestamps: false,
                        fonts_folder: final_output_settings.fonts_folder.to_owned(),
                        force_style: final_force_style,
                    }
                    .into(),
                ))
            }
            None => {}
        }

        if let Some((start, background, request)) = credit_squeeze
            && let Some(stream) = input_settings.select_graphics_stream(background)
        {
            let mut extra_input_args = Vec::new();
            if let Some(ad) = &request.advertisement {
                if !background.in_point.is_zero()
                    && !matches!(background.input_source, InputSource::Lavfi(_))
                {
                    extra_input_args.extend(args![
                        "-ss",
                        format!("{}ms", background.in_point.as_millis())
                    ]);
                }
                extra_input_args.extend(args![
                    "-t",
                    format!(
                        "{}ms",
                        ad.out_point.saturating_sub(background.in_point).as_millis()
                    )
                ]);
                let audio = ProbedInput {
                    input_source: background.input_source.clone(),
                    probe_result: background.probe_result.clone(),
                    in_point: background.in_point,
                    out_point: ad.out_point,
                    stream_index: None,
                };
                filters.insert(
                    0,
                    PipelineFilter::Audio(AudioFilter::CreditSqueeze {
                        layer_index: background.layer_index,
                        stream_index: audio.select_audio_stream().ok().map(|a| a.stream_index),
                        start,
                        channels: output_context
                            .audio_channels
                            .unwrap_or(audio_stream.channels),
                    }),
                );
            }
            inputs.push(PipelineInput::Graphics {
                input: background.clone(),
                layer_index: background.layer_index,
                index: stream.stream_index,
                path: background.probe_result.path.clone(),
                extra_input_args,
            });
            let mut normal: Vec<_> = filters
                .iter()
                .filter(|f| !matches!(f, PipelineFilter::Audio(_)))
                .cloned()
                .collect();
            let mut credits = normal.clone();
            normal.push(PipelineFilter::Video(CreditSqueezeColor.into()));
            if request.advertisement.is_some() {
                credits.retain(|filter| {
                    !matches!(
                        filter,
                        PipelineFilter::Video(VideoFilter::Pad(_) | VideoFilter::Crop(_))
                    )
                });
            }
            for filter in &mut credits {
                match filter {
                    PipelineFilter::Video(VideoFilter::Shader(shader)) => {
                        let mut scale = shader.scale.clone();
                        if request.advertisement.is_some() {
                            scale.scaling_mode = ScalingMode::ScaleAndPad;
                        }
                        *filter = PipelineFilter::Video(scale.into());
                    }
                    PipelineFilter::Video(VideoFilter::Scale(scale))
                        if request.advertisement.is_some() =>
                    {
                        scale.scaling_mode = ScalingMode::ScaleAndPad;
                    }
                    PipelineFilter::Video(VideoFilter::Subtitles(subtitles)) => {
                        // The trimmed tail must retain the shared source clock at the join.
                        subtitles.preserve_timestamps = true;
                    }
                    _ => {}
                }
            }
            let native_nv12 = squeeze_shader
                .as_ref()
                .is_some_and(|shader| shader.options().native_nv12_output)
                && ffmpeg_info
                    .has_video_filter_option(&KnownVideoFilter::LibPlacebo, "nv12_output");
            let size = video_transcode.size.unwrap_or(initial_state.size);
            credits.push(PipelineFilter::CreditSqueeze(CreditSqueeze {
                background_index: background.layer_index,
                start,
                advertisement_elapsed: request.advertisement.as_ref().map(|ad| {
                    ad.elapsed + input_settings.playout_offset.saturating_sub(request.start)
                }),
                program_size: source_aspect_geometry
                    .map_or(initial_state.size, |(_, display)| display),
                size,
                shader: squeeze_shader,
                native_nv12,
                software_output: false,
            }));
            filters.retain(|f| matches!(f, PipelineFilter::Audio(_)));
            filters.push(PipelineFilter::CreditSqueezeBranches(
                CreditSqueezeBranches {
                    start,
                    size,
                    normal: FilterChain::new(normal),
                    credits: FilterChain::new(credits),
                },
            ));
        }

        for (graphics_input, graphics_stream) in
            input_settings.graphics_inputs.iter().zip(graphics_streams)
        {
            let Some(graphics_stream) = graphics_stream else {
                return Err(FFPipelineError::GraphicsStreamNotFound(
                    graphics_input.layer_index,
                ));
            };
            let (Some(height), Some(width)) = (graphics_stream.height, graphics_stream.width)
            else {
                return Err(FFPipelineError::GraphicsStreamNotFound(
                    graphics_input.layer_index,
                ));
            };
            let video_size = video_transcode.size.as_ref().unwrap_or(&initial_state.size);

            // hw overlays don't clip reliably (vaapi rects are int16), so crop to the visible part
            let mut canvas_clip = None;
            if let GraphicsKind::Canvas(CanvasLayout::Region { origin }) = &graphics_input.kind {
                let visible = FrameSize {
                    width: video_size.width.saturating_sub(origin.x).min(width),
                    height: video_size.height.saturating_sub(origin.y).min(height),
                };
                if visible.width == 0 || visible.height == 0 {
                    log::warn!(
                        "canvas graphics layer {} ({}x{} at {},{}) is outside the {}x{} output; skipping it",
                        graphics_input.layer_index,
                        width,
                        height,
                        origin.x,
                        origin.y,
                        video_size.width,
                        video_size.height
                    );
                    continue;
                }
                if visible != (FrameSize { width, height }) {
                    log::warn!(
                        "canvas graphics layer {} ({}x{} at {},{}) extends past the {}x{} output; clipping it to {}x{}",
                        graphics_input.layer_index,
                        width,
                        height,
                        origin.x,
                        origin.y,
                        video_size.width,
                        video_size.height,
                        visible.width,
                        visible.height
                    );
                    canvas_clip = Some(ClipFilter { size: visible });
                }
            }

            let extra_input_args = if graphics_input.kind.is_canvas() {
                // local canvas shouldn't restart from beginning after bursting
                let seek = input_settings.playout_offset + graphics_input.in_point;
                if seek > Duration::ZERO
                    && matches!(graphics_input.input_source, InputSource::Local(_))
                {
                    args![
                        "-ss",
                        format!("{}ms", seek.as_millis()),
                        "-t",
                        format!("{}ms", duration.as_millis())
                    ]
                } else {
                    args!["-t", format!("{}ms", duration.as_millis())]
                }
            } else if graphics_stream.is_still_image() {
                // decode a single frame; the loop filter below repeats it *after* scaling, so
                // decode and scale happen once instead of once per output frame
                args!["-framerate", output_context.frame_rate.r_frame_rate.clone()]
            } else if graphics_stream.codec == "gif" || graphics_stream.codec == "apng" {
                args![
                    "-ignore_loop",
                    "0",
                    "-t",
                    format!("{}ms", duration.as_millis())
                ]
            } else {
                args![
                    "-stream_loop",
                    "-1",
                    "-t",
                    format!("{}ms", duration.as_millis())
                ]
            };

            inputs.push(PipelineInput::Graphics {
                input: graphics_input.clone(),
                layer_index: graphics_input.layer_index,
                index: graphics_stream.stream_index,
                path: graphics_input.probe_result.path.to_owned(),
                extra_input_args,
            });

            let secondary_initial_state = FrameState {
                size: FrameSize { width, height },
                is_anamorphic: false,
                is_interlaced: false,
                sample_aspect_ratio: Some(String::from("1:1")),
                display_aspect_ratio: None,
                surface: FrameSurface::System,
                pixel_format: if graphics_stream.pix_fmt.is_empty() {
                    PixelFormat::Bgra
                } else {
                    PixelFormat::parse(&graphics_stream.pix_fmt)
                },
                color: FrameColor::from(&graphics_stream.color_params),
                hdr_format: HdrFormat::None,
                rotation: None,
                decoder_frames: false,
                sar_written: false,
            };

            // a full-frame canvas size mismatch means stale playout metadata, so scale instead of
            // failing the item. a region canvas is never scaled
            let canvas_needs_scale = matches!(
                graphics_input.kind,
                GraphicsKind::Canvas(CanvasLayout::FullFrame)
            ) && video_size != &secondary_initial_state.size;
            if canvas_needs_scale {
                log::warn!(
                    "canvas graphics layer {} is {}x{} but output is {}x{}; scaling in software",
                    graphics_input.layer_index,
                    secondary_initial_state.size.width,
                    secondary_initial_state.size.height,
                    video_size.width,
                    video_size.height
                );
            }

            let source_content_size = match video_transcode.scaling_mode {
                ScalingMode::ScaleAndPad => {
                    let mut rotated_state = initial_state.clone();
                    rotated_state.apply_rotation();
                    video_size.square_pixel_size_contain(&rotated_state)
                }
                ScalingMode::Crop | ScalingMode::Stretch => *video_size,
            };

            let (scaled_size, location, fade_filters, opacity) = match &graphics_input.kind {
                GraphicsKind::Canvas(layout) => {
                    let origin = match layout {
                        CanvasLayout::FullFrame => FramePoint { x: 0, y: 0 },
                        CanvasLayout::Region { origin } => origin.clone(),
                    };
                    (FrameSize { width, height }, Some(origin), vec![], 1.0)
                }
                GraphicsKind::Media(options) => {
                    let size =
                        options.scaled_size(FrameSize { width, height }, video_transcode.size);
                    (
                        size,
                        Some(options.frame_location(&source_content_size, &size, video_size)),
                        FadeFilter::for_graphics(
                            options.timing.as_ref(),
                            input_settings.start,
                            input_settings.playout_offset,
                            duration,
                        ),
                        options.opacity_percent.unwrap_or(100.0) / 100.0,
                    )
                }
            };

            let ensure_alpha_filter: VideoFilter = EnsureAlphaFilter {
                format: match secondary_initial_state.pixel_format.bit_depth() {
                    10 => PixelFormat::Yuva420p10le,
                    _ => PixelFormat::Bgra,
                },
            }
            .into();

            let mut secondary_filters: Vec<VideoFilter> = if graphics_input.kind.is_canvas() {
                // convert to bgra, not yuva420p: most hw overlays upload bgra, and yuva420p
                // removes chroma detail. evaluate removes this filter for a bgra canvas. the
                // filter also keeps alpha for formats that `PixelFormat::parse` does not know
                let mut filters: Vec<VideoFilter> = vec![
                    FormatFilter {
                        format: PixelFormat::Bgra,
                    }
                    .into(),
                ];
                if let Some(clip) = canvas_clip {
                    filters.push(clip.into());
                }
                if canvas_needs_scale {
                    // stretch, not contain: the canvas has to stay exactly the output size or
                    // the (0,0) overlay would pin a smaller canvas to the top left corner
                    filters.push(
                        ScaleFilter {
                            size: Some(*video_size),
                            scaling_mode: ScalingMode::Stretch,
                            input_is_anamorphic: false,
                            flags: video_transcode.filter_options.scale.flags.clone(),
                        }
                        .into(),
                    );
                }
                filters
            } else {
                vec![
                    ensure_alpha_filter,
                    ColorChannelMixerFilter { alpha: opacity }.into(),
                    ScaleFilter {
                        size: Some(scaled_size),
                        scaling_mode: ScalingMode::ScaleAndPad,
                        input_is_anamorphic: false,
                        flags: video_transcode.filter_options.scale.flags.clone(),
                    }
                    .into(),
                ]
            };

            // a still image is decoded as a single frame; only fades need it repeated (they act on
            // frame timestamps). otherwise the overlay's repeatlast holds it, which keeps the
            // per-frame format conversion and hwupload out of the chain entirely
            if !fade_filters.is_empty() {
                secondary_filters.push(
                    LoopFilter::bounded(
                        graphics_stream.is_still_image(),
                        duration,
                        &output_context.frame_rate,
                    )
                    .into(),
                );
            }

            secondary_filters.extend(fade_filters.iter().map(|f| f.clone().into()));

            filters.push(PipelineFilter::Overlay(OverlayFilter {
                kind: SoftwareOverlay::default().into(),
                secondary: secondary_filters,
                secondary_initial_state,
                secondary_source: OverlaySource::Graphics(graphics_input.layer_index),
                location,
            }));
        }

        if let Some((encoded, _)) = source_aspect_geometry {
            filters.push(PipelineFilter::Video(
                ScaleFilter {
                    size: Some(encoded),
                    scaling_mode: ScalingMode::Stretch,
                    input_is_anamorphic: false,
                    flags: video_transcode.filter_options.scale.flags.clone(),
                }
                .into(),
            ));
        }

        let mut env_vars = Vec::new();

        if let Some(reports_folder) = final_output_settings
            .reports_folder
            .as_deref()
            .filter(|s| !s.is_empty())
            && let Some(report_id) = final_output_settings
                .report_id
                .as_deref()
                .filter(|s| !s.is_empty())
        {
            let folder = PathBuf::from(reports_folder);
            if let Err(err) = std::fs::create_dir_all(&folder) {
                log::warn!("failed to create ffmpeg reports folder: {err}; will not save report");
            } else {
                let file = folder
                    .join(format!(".in-flight-{}.log", report_id))
                    .to_string_lossy()
                    .to_string()
                    .replace(r"%", r"%%");

                #[cfg(target_os = "windows")]
                let mut file = file;

                #[cfg(target_os = "windows")]
                {
                    file = file.replace(r"\", r"/").replace(r":/", r"\:/");
                }

                env_vars = vec![EnvironmentVariable {
                    key: String::from("FFREPORT"),
                    value: format!("file={file}:level=32"),
                }]
            }
        }

        if let VideoEncoder::Encode(codec) = &video_encoder
            && let Some(accel) = &final_output_settings.accel
        {
            filters.extend(
                accel
                    .encoder_filters(codec)
                    .into_iter()
                    .map(PipelineFilter::Video),
            );
        }

        let input_request_context = FfmpegInputRequestContext {
            channel_number: input_settings.channel_number.clone(),
            playout_offset: input_settings.playout_offset,
            duration,
            frame_rate: output_context.frame_rate.r_frame_rate.clone(),
        };

        Ok(Pipeline {
            ffmpeg_info: ffmpeg_info.clone(),
            copy_decisions,
            accel: final_output_settings.accel.clone(),
            filter_options: final_output_settings.video.transcode.filter_options,
            initial_state: initial_state.clone(),
            global_options: vec![
                // hardware accel should use a single thread
                GlobalOption::Threads(match &final_output_settings.accel {
                    Some(_) => 1,
                    _ => 0,
                }),
                GlobalOption::NoStdIn,
                GlobalOption::HideBanner,
                GlobalOption::LogLevel(LogLevel::Error),
                GlobalOption::StandardFormatFlags,
            ],
            inputs,
            filter_chain: FilterChain::new(filters),
            output_options: [
                OutputOption::NoDemuxDecodeDelay,
                OutputOption::MovFlagsFastStart,
                OutputOption::CudaNoAutoScale,
                OutputOption::AudioCodec(audio_codec),
                OutputOption::AudioBitrate(final_output_settings.audio.transcode.bitrate),
                OutputOption::AudioBuffer(final_output_settings.audio.transcode.buffer),
                OutputOption::AudioChannels(final_output_settings.audio.transcode.channels),
                OutputOption::AudioSampleRate(final_output_settings.audio.transcode.sample_rate),
                OutputOption::VideoCodec(video_encoder),
                OutputOption::VideoAspect(source_aspect_geometry.map(|(_, display)| display)),
            ]
            .into_iter()
            .chain(copy_output_options)
            .chain([
                OutputOption::VideoBitrate(final_output_settings.video.transcode.bitrate),
                OutputOption::VideoBuffer(final_output_settings.video.transcode.buffer),
                OutputOption::DoNotMapMetadata,
                OutputOption::Duration(duration),
                // apad does not end; stop at the end of video
                OutputOption::Shortest(None),
                OutputOption::TsOffset(pts_offset),
                OutputOption::AudioPacketFloor,
                OutputOption::VideoTrackTimeScale(90_000),
                OutputOption::FrameRate(output_frame_rate),
                OutputOption::Format(final_output_settings.format),
            ])
            .collect(),
            input_request_context,
            output_context,
            output_sample_aspect_ratio,
            env_vars,
            degradations,
        })
    }

    pub fn copy_decisions(&self) -> &CopyDecisions {
        &self.copy_decisions
    }

    /// Where this item runs other than as configured. Complete once the pipeline is optimized.
    pub fn degradations(&self) -> &[PipelineDegradation] {
        self.degradations.as_slice()
    }

    pub fn optimize(&mut self) {
        // audio copy shouldn't have bitrate etc
        if self.output_context.audio_codec == AudioCodec::Copy {
            self.output_options.retain(|o| {
                !matches!(
                    o,
                    OutputOption::AudioBitrate(_)
                        | OutputOption::AudioBuffer(_)
                        | OutputOption::AudioChannels(_)
                        | OutputOption::AudioSampleRate(_)
                        | OutputOption::Shortest(_)
                )
            });

            self.filter_chain.disable_audio();
        };

        // remove audio channels output option if input channel count matches;
        // aac with more than 2 channels still needs it to normalize layout
        let aac_surround = self.output_context.audio_codec == AudioCodec::Aac
            && self.output_context.audio_channels.is_some_and(|c| c > 2);
        if let Some(audio_channels) = self.inputs.iter().find_map(|s| match s {
            PipelineInput::Audio { channels, .. } => Some(channels),
            _ => None,
        }) && Some(audio_channels) == self.output_context.audio_channels.as_ref()
            && !aac_surround
        {
            self.output_options
                .retain(|o| !matches!(o, OutputOption::AudioChannels(_)));
        }

        // video copy shouldn't have bitrate, etc
        if self.output_context.video_encoder == VideoEncoder::Copy {
            self.output_options.retain(|o| {
                !matches!(
                    o,
                    OutputOption::VideoBitrate(_) | OutputOption::VideoBuffer(_)
                )
            });

            self.filter_chain.disable_video();
        }

        let final_state = self
            .filter_chain
            .evaluate(&self.initial_state, &self.ffmpeg_info);

        // ffmpeg keeps only the last -bsf:v, so all header fixups share one filter
        let tonemapped = self.initial_state.hdr_format != HdrFormat::None
            && final_state.hdr_format == HdrFormat::None;
        if let VideoEncoder::Encode(codec) = &self.output_context.video_encoder
            && let Some(bsf) = self
                .accel
                .as_ref()
                .and_then(|a| a.metadata_bsf(codec))
                .map(|bsf| MetadataBsf {
                    sample_aspect_ratio: self
                        .output_sample_aspect_ratio
                        .or(bsf.sample_aspect_ratio),
                    bt709: bsf.bt709 && tonemapped,
                    ..bsf
                })
                .filter(|bsf| !bsf.is_empty())
            && let Some(index) = self
                .output_options
                .iter()
                .position(|o| matches!(o, OutputOption::VideoCodec(_)))
        {
            self.output_options
                .insert(index + 1, OutputOption::VideoMetadata(bsf));
        }
        self.filter_chain.resolve(
            &self.ffmpeg_info,
            &self.accel,
            &self.filter_options,
            &self.initial_state,
            &self.output_context.encoder_target,
        );

        // an accel resolves the tonemap to its own filter where it can; one left as the software
        // tonemap runs on the cpu
        if self.initial_state.hdr_format != HdrFormat::None
            && let Some(accel) = &self.accel
            && self
                .filter_chain
                .filters
                .iter()
                .any(|f| matches!(f, PipelineFilter::Video(VideoFilter::ToneMap(_))))
        {
            self.degradations.add(
                DegradationCode::HardwareTonemapUnavailable,
                format!(
                    "{} hardware tone mapping is unavailable for this HDR item. Using CPU tone mapping.",
                    accel_name(accel)
                ),
            );
        }

        // prepend decoder filters;
        // this is a special case that's only really needed for CUDA's hwupload workaround
        if let Some(video_decoder) = self.inputs.iter().find_map(|s| match s {
            PipelineInput::Video { decoder, .. } => Some(decoder),
            _ => None,
        }) {
            self.filter_chain.prepend(video_decoder.filters());
        }

        self.filter_chain.optimize();

        if self.audio_needs_own_input() {
            for input in &mut self.inputs {
                if let PipelineInput::Audio { own_input, .. } = input {
                    *own_input = true;
                }
            }
        }

        if self.shortest_needs_small_buffer() {
            for option in &mut self.output_options {
                if let OutputOption::Shortest(buffer) = option {
                    *buffer = Some(Duration::from_millis(500));
                }
            }
        }

        if let Some(accel) = &self.accel {
            let mut surfaces = self.filter_chain.surfaces().clone();
            surfaces.insert(self.initial_state.surface);
            surfaces.insert(self.output_context.encoder_target.surface);
            if surfaces.iter().any(|s| *s != FrameSurface::System) {
                let args = accel.init_hw_device(&surfaces);
                self.global_options.push(GlobalOption::InitHwDevice(args));
            }
        }
    }

    /// With a shared demuxer, video read ahead of late audio waits in overlay_qsv or -shortest
    /// and uses up a fixed qsv pool (runtime < 2.9). Audio is late when loudnorm holds back ~3 s
    /// behind a slow canvas, or when the file stores it seconds after the video (some ASF).
    fn audio_needs_own_input(&self) -> bool {
        // rkmpp tests time out with loudnorm
        if let Some(HardwareAccel::Rkmpp(_)) = &self.accel
            && self.has_loudnorm()
        {
            return true;
        }

        let Some(HardwareAccel::Qsv(qsv)) = &self.accel else {
            return false;
        };

        let canvas_layers: Vec<usize> = self
            .inputs
            .iter()
            .filter_map(|i| match i {
                PipelineInput::Graphics {
                    input, layer_index, ..
                } if input.kind.is_canvas() => Some(*layer_index),
                _ => None,
            })
            .collect();

        let qsv_overlays_canvas = self.filter_chain.filters.iter().any(|f| {
            matches!(
                f,
                PipelineFilter::Overlay(OverlayFilter {
                    kind: OverlayKind::Qsv(_),
                    secondary_source: OverlaySource::Graphics(layer),
                    ..
                }) if canvas_layers.contains(layer)
            )
        });

        let loudnorm_with_canvas = self.has_loudnorm() && qsv_overlays_canvas;

        let keeps_sync = self.inputs.iter().any(|i| {
            matches!(
                i,
                PipelineInput::Audio {
                    own_input_keeps_sync: true,
                    ..
                }
            )
        });

        qsv.capabilities.requires_fixed_pool() && (loudnorm_with_canvas || keeps_sync)
    }

    /// loudnorm delays audio by ~3 s, so -shortest queues video until audio arrives.
    /// The default 10 s queue uses all surfaces of a fixed qsv pool (64), with or without a canvas.
    fn shortest_needs_small_buffer(&self) -> bool {
        let Some(HardwareAccel::Qsv(qsv)) = &self.accel else {
            return false;
        };

        qsv.capabilities.requires_fixed_pool() && self.has_loudnorm()
    }

    fn has_loudnorm(&self) -> bool {
        self.filter_chain.filters.iter().any(|f| {
            matches!(
                f,
                PipelineFilter::Audio(AudioFilter::LoudNorm {
                    settings: Some(_),
                    ..
                })
            )
        })
    }

    pub fn args(&self) -> ArgVec {
        let mut result: ArgVec = Vec::new();

        let mut audio_label = String::from("0:a");
        let mut video_label = String::from("0:v");
        let mut subtitle_label = None;
        let mut graphics_labels = vec![
            None;
            self.inputs
                .iter()
                .filter_map(|i| match i {
                    PipelineInput::Graphics { layer_index, .. } => Some(*layer_index),
                    _ => None,
                })
                .max()
                .map_or(0, |i| i + 1)
        ];

        let mut input_paths: Vec<&str> = Vec::new();

        // audio decoder options must come before their input's `-i`.
        // the video input writes that `-i` when both share a demuxer.
        let audio_decoder_args: Option<(&str, ArgVec)> = self.inputs.iter().find_map(|i| match i {
            PipelineInput::Audio {
                path,
                decoder,
                own_input: false,
                ..
            } => Some((path.as_str(), decoder.as_arg())),
            _ => None,
        });

        // the subtitle can share the video's `-i`
        let mut subtitle_canvas_args: Option<(&str, ArgVec)> =
            self.inputs.iter().find_map(|i| match i {
                PipelineInput::Subtitle {
                    index,
                    path,
                    canvas_size: Some(size),
                    ..
                } => Some((
                    path.as_str(),
                    args![
                        format!("-canvas_size:{index}"),
                        format!("{}x{}", size.width, size.height)
                    ],
                )),
                _ => None,
            });

        let mut sorted_inputs: Vec<&PipelineInput> = self.inputs.iter().collect();
        sorted_inputs.sort_by_key(|i| i.sort_order());

        result.extend(self.global_options.iter().flat_map(|o| o.as_arg()));

        for input in sorted_inputs.iter() {
            match input {
                PipelineInput::Video {
                    input_source,
                    index,
                    path,
                    seek,
                    realtime,
                    decoder,
                    ..
                } => {
                    input_paths.push(path.as_str());

                    result.extend(decoder.as_arg());

                    if let Some((audio_path, audio_args)) = &audio_decoder_args
                        && *audio_path == path.as_str()
                    {
                        result.extend(audio_args.to_owned());
                    }

                    let video_input_index = input_paths.iter().position(|p| p == path).unwrap_or(0);
                    video_label = format!("{}:{}", video_input_index, index);

                    if !seek.is_zero() {
                        result.extend(args!["-ss", format!("{}ms", seek.as_millis())]);
                    }

                    if *realtime {
                        result.extend(args!["-readrate", "1.0"]);
                    }

                    // ffmpeg's autorotate only applies to system-memory frames and would run
                    // ahead of deinterlacing, so TransposeFilter rotates explicitly instead
                    result.extend(args!["-noautorotate"]);

                    result.extend(input_source.args_for_input());

                    if let Some((_, canvas_args)) =
                        subtitle_canvas_args.take_if(|(p, _)| *p == path.as_str())
                    {
                        result.extend(canvas_args);
                    }

                    result.extend(args!["-i", path.to_owned()]);
                }
                PipelineInput::Audio {
                    input_source,
                    index,
                    path,
                    seek,
                    decoder,
                    own_input,
                    ..
                } => {
                    let shared_index = input_paths
                        .iter()
                        .position(|p| p == path)
                        .filter(|_| !own_input);

                    let audio_input_index = match shared_index {
                        Some(shared_index) => shared_index,
                        None => {
                            input_paths.push(path.as_str());

                            result.extend(decoder.as_arg());

                            // lavfi can't seek
                            if !seek.is_zero() && !matches!(input_source, InputSource::Lavfi(_)) {
                                result.extend(args!["-ss", format!("{}ms", seek.as_millis())]);
                            }

                            result.extend(input_source.args_for_input());

                            if let Some((_, canvas_args)) =
                                subtitle_canvas_args.take_if(|(p, _)| *p == path.as_str())
                            {
                                result.extend(canvas_args);
                            }

                            result.extend(args!["-i", path.to_owned()]);

                            input_paths.len() - 1
                        }
                    };
                    audio_label = format!("{}:{}", audio_input_index, index);
                }
                PipelineInput::Subtitle {
                    input_source,
                    index,
                    path,
                    seek,
                    ..
                } => {
                    if !input_paths.contains(&path.as_str()) {
                        input_paths.push(path.as_str());

                        if !seek.is_zero() {
                            result.extend(args!["-ss", format!("{}ms", seek.as_millis())]);
                        }

                        result.extend(input_source.args_for_input());

                        if let Some((_, canvas_args)) = subtitle_canvas_args.take() {
                            result.extend(canvas_args);
                        }

                        result.extend(args!["-i", path.to_owned()]);
                    }

                    let subtitle_input_index =
                        input_paths.iter().position(|p| p == path).unwrap_or(0);
                    subtitle_label = Some(format!("{}:{}", subtitle_input_index, index));
                }
                PipelineInput::Graphics {
                    input,
                    layer_index,
                    index,
                    path,
                    extra_input_args,
                } => {
                    input_paths.push(path.as_str());

                    if input.kind.is_canvas() {
                        result.extend(
                            input
                                .input_source
                                .args_for_input_with_context(&self.input_request_context),
                        );
                    } else {
                        result.extend(input.input_source.args_for_input());
                    }
                    result.extend(extra_input_args.clone());
                    result.extend(args!["-i", path.to_owned()]);
                    let graphics_input_index = input_paths.len() - 1;
                    graphics_labels[*layer_index] =
                        Some(format!("{}:{}", graphics_input_index, index));
                }
            }
        }

        let mut filter_chain = self.filter_chain.to_owned();
        filter_chain.build(
            &audio_label,
            &video_label,
            subtitle_label.as_ref(),
            &graphics_labels,
        );

        result.extend(filter_chain.as_arg());

        result.extend(args!["-map", filter_chain.video_label().to_owned()]);
        result.extend(args!["-map", filter_chain.audio_label().to_owned()]);

        result.extend(
            self.output_options
                .iter()
                .flat_map(|o| o.as_arg(&self.output_context)),
        );

        result
    }

    pub fn envs(&self) -> Vec<EnvironmentVariable> {
        let mut result = self.env_vars.clone();

        if let Some(a) = &self.accel {
            result.extend(a.envs())
        }

        result.extend(self.filter_chain.envs());

        result
    }
}

impl std::fmt::Display for Pipeline {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "args: {}", self.args().join(" "))
    }
}

// PGS is 1080p. With no size, ffmpeg uses the video size (e.g. 4K) and scales that canvas on
// every frame until the first subtitle.
fn unknown_pgs_canvas_size(stream: &ProbeResultVideoStream) -> Option<FrameSize> {
    (stream.is_pgs_subtitle() && (stream.width.is_none() || stream.height.is_none())).then_some(
        FrameSize {
            width: 1920,
            height: 1080,
        },
    )
}

fn subtitle_burn<'a>(
    input_settings: &'a InputSettings,
    output_settings: &OutputSettings,
    video_stream: &ProbeResultVideoStream,
) -> Option<SubtitleBurn<'a>> {
    match (
        input_settings.select_subtitle_stream(),
        input_settings.subtitle_input.as_ref(),
    ) {
        (Some(stream), Some(input)) if stream.is_subtitle_image() => output_settings
            .video
            .transcode
            .size
            .or_else(|| {
                // with no target size, only transpose changes the frame size
                let (width, height) = (video_stream.width?, video_stream.height?);
                Some(if video_stream.is_quarter_turn() {
                    FrameSize {
                        width: height,
                        height: width,
                    }
                } else {
                    FrameSize { width, height }
                })
            })
            .map(|size| SubtitleBurn::Image {
                stream,
                input,
                size,
            }),
        (Some(stream), Some(input)) if output_settings.subtitle_mode == SubtitleMode::Burn => {
            Some(SubtitleBurn::Text { stream, input })
        }
        _ => None,
    }
}

fn copy_decisions(
    input_settings: &InputSettings,
    output_settings: &OutputSettings,
    video_stream: &ProbeResultVideoStream,
    audio_input: &ProbedInput,
    audio_stream: &ProbeResultAudioStream,
    subtitle_burn: Option<&SubtitleBurn>,
) -> CopyDecisions {
    CopyDecisions {
        video: output_settings.video.copy.as_ref().map(|policy| {
            video_copy_decision(
                policy,
                &input_settings.video_input,
                video_stream,
                &VideoCopyContext {
                    caller_blockers: &input_settings.video_copy_blockers,
                    is_still_image: input_settings.video_input.probe_result.is_still_image(),
                    has_graphics: !input_settings.graphics_inputs.is_empty()
                        || input_settings.credit_squeeze.is_some(),
                    image_subtitle: matches!(subtitle_burn, Some(SubtitleBurn::Image { .. })),
                    burned_subtitle: matches!(subtitle_burn, Some(SubtitleBurn::Text { .. })),
                    target_frame_rate: output_settings.frame_rate.as_ref(),
                },
            )
        }),
        audio: output_settings.audio.copy.as_ref().map(|policy| {
            let decision = audio_copy_decision(policy, audio_input, audio_stream);
            if input_settings
                .credit_squeeze
                .as_ref()
                .is_some_and(|s| s.advertisement.is_some())
            {
                let mut blockers = match decision {
                    CopyDecision::Copy => Vec::new(),
                    CopyDecision::Transcode(blockers) => blockers,
                };
                blockers.push(CopyBlocker::CreditSqueezeAudio);
                CopyDecision::Transcode(blockers)
            } else {
                decision
            }
        }),
    }
}

/// Lets the caller prepare a copy (keyframe seek) before building the pipeline.
pub fn predict_copy_decisions(
    input_settings: &InputSettings,
    output_settings: &OutputSettings,
) -> Result<CopyDecisions, FFPipelineError> {
    let video_stream = input_settings.select_video_stream()?;
    // must match `Pipeline::full`
    let silence = input_settings.audio_input.silence_if_missing();
    let audio_input = silence.as_ref().unwrap_or(&input_settings.audio_input);
    let audio_stream = audio_input.select_audio_stream()?;
    let subtitle_burn = subtitle_burn(input_settings, output_settings, video_stream);
    Ok(copy_decisions(
        input_settings,
        output_settings,
        video_stream,
        audio_input,
        audio_stream,
        subtitle_burn.as_ref(),
    ))
}

pub fn generate_pipeline(
    ffmpeg_info: &FfmpegInfo,
    input_settings: InputSettings,
    output_settings: OutputSettings,
) -> Result<Pipeline, FFPipelineError> {
    Pipeline::full(ffmpeg_info, input_settings, output_settings)
}

/// For AVFMT_TS_DISCONT formats, ffmpeg starts the input at its earliest used stream, so an
/// audio-only second input would shift audio against video. Unknown formats count as discontinuous.
fn own_input_keeps_sync(format_name: Option<&str>) -> bool {
    const TS_DISCONT: [&str; 11] = [
        "dhav",
        "dvdvideo",
        "flv",
        "hls",
        "live_flv",
        "mpeg",
        "mpegts",
        "mpegtsraw",
        "ogg",
        "rcwt",
        "ty",
    ];
    format_name.is_some_and(|name| !name.split(',').any(|n| TS_DISCONT.contains(&n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_format_parses_ffmpeg_codec_names() {
        for (codec, format) in [
            ("av1", VideoFormat::Av1),
            ("h264", VideoFormat::H264),
            ("hevc", VideoFormat::Hevc),
            ("mpeg2video", VideoFormat::Mpeg2Video),
            ("vc1", VideoFormat::Vc1),
            ("vp8", VideoFormat::Vp8),
            ("vp9", VideoFormat::Vp9),
        ] {
            assert_eq!(codec.parse::<VideoFormat>().ok(), Some(format));
        }
        assert!("prores".parse::<VideoFormat>().is_err());
    }

    #[test]
    fn device_name_returns_correct_ffmpeg_device_strings() {
        assert_eq!(FrameSurface::Cuda.device_name(), Some("cuda"));
        assert_eq!(FrameSurface::OpenCL.device_name(), Some("opencl"));
        assert_eq!(FrameSurface::Qsv.device_name(), Some("qsv"));
        assert_eq!(FrameSurface::Vaapi.device_name(), Some("vaapi"));
        assert_eq!(FrameSurface::Vulkan.device_name(), Some("vulkan"));
        assert_eq!(
            FrameSurface::VideoToolbox.device_name(),
            Some("videotoolbox")
        );
        assert_eq!(FrameSurface::System.device_name(), None);
    }

    fn multichannel_ac3_input(path: &str) -> InputSettings {
        let probe_result = crate::probe::ProbeResult {
            path: path.to_owned(),
            streams: vec![
                crate::probe::ProbeResultStream::Video(Box::new(
                    crate::probe::ProbeResultVideoStream {
                        stream_index: 0,
                        codec: "h264".to_owned(),
                        codec_type: crate::probe::CodecType::Video,
                        dv_profile: None,
                        profile: "main".to_owned(),
                        height: Some(480),
                        width: Some(720),
                        frame_rate: FrameRate::parse("30000/1001"),
                        sample_aspect_ratio: None,
                        display_aspect_ratio: None,
                        pix_fmt: "yuv420p".to_owned(),
                        color_params: Default::default(),
                        field_order: None,
                        rotation: None,
                    },
                )),
                crate::probe::ProbeResultStream::Audio(crate::probe::ProbeResultAudioStream {
                    stream_index: 1,
                    codec: "ac3".to_owned(),
                    channels: 6,
                }),
            ],
            duration: Some(Duration::from_secs(60)),
            format_name: Some("matroska".to_owned()),
        };

        let probed_input = |probe_result: crate::probe::ProbeResult| crate::input::ProbedInput {
            input_source: InputSource::Local(crate::input::LocalInputSource {
                path: path.to_owned(),
            }),
            probe_result,
            in_point: Duration::ZERO,
            out_point: Duration::from_secs(30),
            stream_index: None,
        };

        InputSettings {
            start: time::OffsetDateTime::now_utc(),
            playout_offset: Duration::ZERO,
            audio_input: probed_input(probe_result.clone()),
            video_input: probed_input(probe_result),
            subtitle_input: None,
            graphics_inputs: Vec::new(),
            credit_squeeze: None,
            channel_number: None,
            video_copy_seek: None,
            video_copy_blockers: Vec::new(),
        }
    }

    fn stereo_output() -> OutputSettings {
        OutputSettings {
            audio: crate::output_settings::AudioOutputSettings {
                copy: None,
                transcode: crate::output_settings::AudioTranscodeSettings {
                    format: AudioFormat::Aac,
                    bitrate: Some(Kbps(320)),
                    buffer: Some(Kbps(640)),
                    channels: Some(2),
                    sample_rate: Some(Hz(48000)),
                    loudness: None,
                },
            },
            video: crate::output_settings::VideoOutputSettings {
                copy: None,
                transcode: crate::output_settings::VideoTranscodeSettings {
                    format: EncodeFormat::H264,
                    bit_depth: 8,
                    tone_mapping: ToneMappingMode::On,
                    hdr_vaapi_lut: false,
                    bitrate: Some(Kbps(2000)),
                    buffer: Some(Kbps(4000)),
                    size: Some(FrameSize {
                        width: 1280,
                        height: 720,
                    }),
                    scaling_mode: ScalingMode::ScaleAndPad,
                    preserve_source_aspect: false,
                    deinterlace: true,
                    filter_options: VideoFilterOptions::default(),
                },
            },
            accel: None,
            unavailable_accel: None,
            format: crate::output_format::OutputFormat::Hls {
                playlist: "out.m3u8".to_owned(),
                segment_template: "live%06d.ts".to_owned(),
                troubleshoot: false,
            },
            pts_offset: None,
            realtime: false,
            is_live: false,
            frame_rate: None,
            subtitle_mode: SubtitleMode::Burn,
            fonts_folder: None,
            subtitle_force_style: None,
            reports_folder: None,
            report_id: None,
        }
    }

    #[test]
    fn continuing_hls_job_marks_counter_reset_and_bounds_audio_packets() {
        let mut output = stereo_output();
        output.pts_offset = Some(PtsOffset {
            duration: Duration::from_micros(8_041_367),
            audio_end: Some(Duration::from_micros(8_066_733)),
        });
        let mut pipeline = generate_pipeline(
            &FfmpegInfo::default(),
            multichannel_ac3_input("main.mkv"),
            output,
        )
        .unwrap();
        pipeline.optimize();
        let args = pipeline.args();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-output_ts_offset", "8041ms"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-bsf:a", "noise=amount=0:drop='lt(pts*tb,0.025733000)'"])
        );
        assert!(args.windows(2).any(|pair| pair
            == [
                "-hls_segment_options",
                "mpegts_flags=+initial_discontinuity"
            ]));
    }

    #[test]
    fn source_aspect_removes_baked_bars_but_keeps_encoded_size() {
        for preserve in [false, true] {
            let mut input = multichannel_ac3_input("main.mkv");
            if let crate::probe::ProbeResultStream::Video(video) =
                &mut input.video_input.probe_result.streams[0]
            {
                video.width = Some(640);
            }
            let mut output = stereo_output();
            output.video.transcode.preserve_source_aspect = preserve;
            let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, output).unwrap();
            pipeline.optimize();
            let args = pipeline.args();
            let graph = filter_complex(&args);
            assert_eq!(graph.contains("pad="), !preserve, "{graph}");
            assert_eq!(args.iter().any(|arg| arg == "-aspect:v"), preserve);
            if preserve {
                let index = args.iter().position(|arg| arg == "-aspect:v").unwrap();
                assert_eq!(args[index + 1], "960:720");
                assert!(graph.contains("1280:720"), "{graph}");
            }
        }
    }

    fn hdr10_input() -> InputSettings {
        let mut input = multichannel_ac3_input("main.mkv");
        for probe in [
            &mut input.video_input.probe_result,
            &mut input.audio_input.probe_result,
        ] {
            if let crate::probe::ProbeResultStream::Video(video) = &mut probe.streams[0] {
                video.codec = "hevc".to_owned();
                video.width = Some(1920);
                video.height = Some(1080);
                video.pix_fmt = "yuv420p10le".to_owned();
                video.color_params = crate::probe::ProbeResultColorParams {
                    color_range: Some("tv".to_owned()),
                    color_space: Some("bt2020nc".to_owned()),
                    color_transfer: Some("smpte2084".to_owned()),
                    color_primaries: Some("bt2020".to_owned()),
                    has_hdr10_metadata: true,
                };
            }
        }
        input
    }

    #[test]
    fn hdr10_lut_matches_legacy_input_selection() {
        let input = hdr10_input();
        let crate::probe::ProbeResultStream::Video(video) =
            &input.video_input.probe_result.streams[0]
        else {
            panic!("HDR fixture must contain video");
        };
        assert!(crate::accel::vaapi::hdr10_lut_eligible(video));
        for profile in [5, 7, 8] {
            let mut other = video.clone();
            other.dv_profile = Some(profile);
            assert!(!crate::accel::vaapi::hdr10_lut_eligible(&other));
        }
        for (codec, profile) in [
            ("dvhe", "Main 10"),
            ("dvh1", "Main 10"),
            ("hevc", "Dolby Vision Profile 7"),
        ] {
            let mut other = video.clone();
            other.codec = codec.to_owned();
            other.profile = profile.to_owned();
            assert!(!crate::accel::vaapi::hdr10_lut_eligible(&other));
        }
        for (field, value) in [
            ("range", None),
            ("range", Some("pc")),
            ("transfer", Some("arib-std-b67")),
            ("primaries", Some("bt709")),
            ("matrix", Some("bt2020c")),
            ("pixel", Some("yuv420p")),
        ] {
            let mut other = video.clone();
            let value = value.map(str::to_owned);
            match field {
                "range" => other.color_params.color_range = value,
                "transfer" => other.color_params.color_transfer = value,
                "primaries" => other.color_params.color_primaries = value,
                "matrix" => other.color_params.color_space = value,
                "pixel" => other.pix_fmt = value.unwrap(),
                _ => unreachable!(),
            }
            assert!(!crate::accel::vaapi::hdr10_lut_eligible(&other), "{field}");
        }
    }

    fn degradation_codes(pipeline: &Pipeline) -> Vec<DegradationCode> {
        pipeline.degradations().iter().map(|d| d.code).collect()
    }

    fn hdr10_shader_args(with_lut: bool) -> String {
        hdr10_shader_pipeline(with_lut).args().join(" ")
    }

    fn hdr10_shader_pipeline(with_lut: bool) -> Pipeline {
        use crate::filter_chain::tests::{
            ffmpeg_info_with_libplacebo, vaapi_accel_with_libplacebo,
        };
        let input = hdr10_input();
        let mut output = OutputSettings {
            accel: Some(HardwareAccel::Vaapi(vaapi_accel_with_libplacebo(true))),
            ..stereo_output()
        };
        output.video.transcode.size = Some(FrameSize {
            width: 1920,
            height: 1080,
        });
        output.video.transcode.filter_options.custom_shader = Some(
            crate::output_settings::CustomShader::new("//!HOOK MAIN")
                .with_input_height(Some(480))
                .with_options(crate::output_settings::CustomShaderOptions {
                    hdr_vaapi_lut: true,
                    ..Default::default()
                }),
        );
        let mut ffmpeg_info = ffmpeg_info_with_libplacebo();
        ffmpeg_info
            .hwaccels
            .insert(crate::ffmpeg_info::KnownHardwareAccel::Vaapi.to_string());
        if with_lut {
            ffmpeg_info
                .video_filters
                .insert(KnownVideoFilter::TonemapVaapiLut.to_string());
        }
        let mut pipeline = Pipeline::full(&ffmpeg_info, input, output).unwrap();
        pipeline.optimize();
        pipeline
    }

    fn squeeze_pipeline(squeeze: bool, elapsed: u64, shader: bool) -> Pipeline {
        squeeze_pipeline_with(squeeze, elapsed, shader, |_, _| {})
    }

    fn squeeze_pipeline_with(
        squeeze: bool,
        elapsed: u64,
        shader: bool,
        configure: impl FnOnce(&mut InputSettings, &mut OutputSettings),
    ) -> Pipeline {
        use crate::filter_chain::tests::{
            ffmpeg_info_with_libplacebo, vaapi_accel_with_libplacebo,
        };
        let mut info = ffmpeg_info_with_libplacebo();
        info.hwaccels
            .insert(crate::ffmpeg_info::KnownHardwareAccel::Vaapi.to_string());
        info.video_filters.extend(
            [
                KnownVideoFilter::ScaleVaapi,
                KnownVideoFilter::OverlayVaapi,
                KnownVideoFilter::Concat,
            ]
            .map(|f| f.to_string()),
        );
        info.video_filter_options
            .insert("concat".to_owned(), ["preserve_ts".to_owned()].into());
        let mut input = multichannel_ac3_input("main.mkv");
        input.playout_offset = Duration::from_secs(elapsed);
        if squeeze {
            let mut background = canvas_input(InputSource::Local(crate::input::LocalInputSource {
                path: "background.png".to_owned(),
            }));
            background.kind = GraphicsKind::Media(crate::input::MediaGraphicsOptions {
                location: crate::input::GraphicsLocation::Center,
                width_percent: None,
                within_source_content: None,
                horizontal_margin_percent: None,
                vertical_margin_percent: None,
                opacity_percent: None,
                timing: None,
            });
            input.credit_squeeze = Some(crate::input::CreditSqueezeInput {
                start: Duration::from_secs(5),
                background: Ok(background),
                advertisement: None,
            });
        }
        let mut output = stereo_output();
        let mut vaapi = vaapi_accel_with_libplacebo(true);
        vaapi.capabilities.supported.insert((
            libva_sys::VA_PROFILE_H264_MAIN,
            libva_sys::VA_ENTRYPOINT_ENC_SLICE,
        ));
        vaapi
            .capabilities
            .vpp_pixel_formats
            .extend([libva_sys::VA_FOURCC_NV12, libva_sys::VA_FOURCC_BGRA]);
        vaapi.capabilities.can_overlay = true;
        output.accel = Some(HardwareAccel::Vaapi(vaapi));
        if shader {
            output.video.transcode.filter_options.custom_shader = Some(
                crate::output_settings::CustomShader::new("//!HOOK MAIN")
                    .with_input_height(Some(480)),
            );
        }
        configure(&mut input, &mut output);
        let mut pipeline = Pipeline::full(&info, input, output).unwrap();
        pipeline.optimize();
        pipeline
    }

    fn squeeze_ad_input() -> crate::input::CreditSqueezeInput {
        let mut background = canvas_input(InputSource::Local(crate::input::LocalInputSource {
            path: "advertisement.mkv".to_owned(),
        }));
        background.probe_result = multichannel_ac3_input("advertisement.mkv")
            .video_input
            .probe_result;
        background.kind = GraphicsKind::Media(crate::input::MediaGraphicsOptions {
            location: crate::input::GraphicsLocation::Center,
            width_percent: None,
            within_source_content: None,
            horizontal_margin_percent: None,
            vertical_margin_percent: None,
            opacity_percent: None,
            timing: None,
        });
        background.in_point = Duration::from_secs(2);
        crate::input::CreditSqueezeInput {
            start: Duration::from_secs(5),
            background: Ok(background),
            advertisement: Some(crate::input::CreditSqueezeAdvertisementInput {
                elapsed: Duration::ZERO,
                out_point: Duration::from_secs(20),
            }),
        }
    }

    #[test]
    fn credit_squeeze_ad_splices_program_audio_and_uses_silence_for_a_silent_ad() {
        for silent in [false, true] {
            let pipeline = squeeze_pipeline_with(true, 0, false, |input, _| {
                let mut squeeze = squeeze_ad_input();
                if silent {
                    squeeze
                        .background
                        .as_mut()
                        .unwrap()
                        .probe_result
                        .streams
                        .truncate(1);
                }
                input.credit_squeeze = Some(squeeze);
            });
            let (layer_index, stream_index, start, channels) = pipeline
                .filter_chain
                .filters
                .iter()
                .find_map(|filter| match filter {
                    PipelineFilter::Audio(AudioFilter::CreditSqueeze {
                        layer_index,
                        stream_index,
                        start,
                        channels,
                    }) => Some((*layer_index, *stream_index, *start, *channels)),
                    _ => None,
                })
                .expect("ad audio splice");
            assert_eq!(layer_index, 0);
            assert_eq!(stream_index, if silent { None } else { Some(1) });
            assert_eq!(start, Duration::from_secs(5));
            assert_eq!(channels, 2);
            let args = pipeline.args();
            let graph = filter_complex(&args);
            assert!(graph.contains("atrim=end=5,apad=whole_dur=5,atrim=end=5"));
            assert!(graph.contains("concat=n=2:v=0:a=1[cs_audio]"));
            assert_eq!(graph.contains("anullsrc=r=48000:cl=2c"), silent);
            assert_eq!(
                graph.contains(
                    "[1:1]aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=2c"
                ),
                !silent
            );
        }
    }

    #[test]
    fn credit_squeeze_ad_late_join_uses_ad_audio_and_remaining_source_duration() {
        let pipeline = squeeze_pipeline_with(true, 8, false, |input, _| {
            let mut squeeze = squeeze_ad_input();
            // ChannelSession has advanced the source in-point by the three seconds past the marker.
            squeeze.background.as_mut().unwrap().in_point = Duration::from_secs(5);
            input.credit_squeeze = Some(squeeze);
        });
        let args = pipeline.args();
        let audio = filter_complex(&args);
        assert!(audio.contains("[0:1]anullsink;[1:1]aformat="));
        assert!(!audio.contains("concat=n=2:v=0:a=1"));
        let inputs = args_by_input(&args);
        let (_, ad_args) = inputs
            .iter()
            .find(|(path, _)| path == "advertisement.mkv")
            .expect("ad input");
        assert!(ad_args.windows(2).any(|pair| pair == ["-ss", "5000ms"]));
        assert!(ad_args.windows(2).any(|pair| pair == ["-t", "15000ms"]));
        assert!(args.iter().any(|arg| arg.contains("clip(t-0+3,0,1)")));
    }

    #[test]
    fn credit_squeeze_ad_blocks_copyable_program_audio() {
        let pipeline = squeeze_pipeline_with(true, 0, false, |input, output| {
            input.credit_squeeze = Some(squeeze_ad_input());
            output.audio.copy = Some(crate::output_settings::CopyPolicy::default());
        });
        assert_eq!(
            pipeline.copy_decisions().audio,
            Some(CopyDecision::Transcode(vec![
                CopyBlocker::CreditSqueezeAudio
            ]))
        );
        assert!(pipeline.output_context.audio_codec == AudioCodec::Aac);
    }

    #[test]
    fn credit_squeeze_hdr_ad_refusal_retains_original_picture_and_audio() {
        let pipeline = squeeze_pipeline_with(true, 0, false, |input, _| {
            let mut squeeze = squeeze_ad_input();
            let ProbeResultStream::Video(video) =
                &mut squeeze.background.as_mut().unwrap().probe_result.streams[0]
            else {
                panic!("ad video");
            };
            video.color_params.color_transfer = Some("smpte2084".to_owned());
            input.credit_squeeze = Some(squeeze);
        });
        assert!(degradation_codes(&pipeline).contains(&DegradationCode::CreditSqueezeUnavailable));
        assert!(!pipeline.filter_chain.filters.iter().any(|filter| matches!(
            filter,
            PipelineFilter::Audio(AudioFilter::CreditSqueeze { .. })
                | PipelineFilter::CreditSqueezeBranches(_)
        )));
        let args = pipeline.args();
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("concat=n=2") || arg.contains("anullsink"))
        );
        assert!(
            !args_by_input(&args)
                .iter()
                .any(|(path, _)| path == "advertisement.mkv")
        );
    }

    #[test]
    fn credit_squeeze_ad_keeps_elapsed_entrance_and_normalizes_only_moving_aspect() {
        for elapsed in [Duration::from_millis(500), Duration::from_secs(4)] {
            let pipeline = squeeze_pipeline_with(true, 0, true, |input, _| {
                let mut squeeze = squeeze_ad_input();
                squeeze.start = Duration::ZERO;
                squeeze.advertisement.as_mut().unwrap().elapsed = elapsed;
                input.credit_squeeze = Some(squeeze);
            });
            let branches = pipeline
                .filter_chain
                .filters
                .iter()
                .find_map(|filter| match filter {
                    PipelineFilter::CreditSqueezeBranches(branches) => Some(branches),
                    _ => None,
                })
                .expect("squeeze branches");
            assert_eq!(branches.start, Duration::ZERO);
            let stage = branches
                .credits
                .filters
                .iter()
                .find_map(|filter| match filter {
                    PipelineFilter::CreditSqueeze(stage) => Some(stage),
                    _ => None,
                })
                .expect("squeeze compositor");
            assert_eq!(stage.advertisement_elapsed, Some(elapsed));
            let args = pipeline.args();
            let graph = args
                .iter()
                .find(|arg| arg.contains("[cs_background][cs_program]libplacebo="))
                .expect("video graph");
            assert!(graph.contains("format=rgba:normalize_sar=1:colorspace=bt709"));
            assert!(graph.contains(&format!("clip(t-0+{},0,1)", elapsed.as_secs_f64())));
            assert!(!graph.contains("loop=loop=-1:size=1:start=0"));
        }
        let still = squeeze_pipeline(true, 0, true).args();
        assert!(!still.iter().any(|arg| arg.contains("normalize_sar=1")));
        assert!(
            still
                .iter()
                .any(|arg| arg.contains("loop=loop=-1:size=1:start=0"))
        );
    }

    #[test]
    fn credit_squeeze_preserves_lead_geometry_audio_and_converts_join_colour() {
        for shader in [false, true] {
            let ordinary = squeeze_pipeline(false, 0, shader);
            let squeezed = squeeze_pipeline(true, 0, shader);
            let normal = squeezed
                .filter_chain
                .filters
                .iter()
                .find_map(|f| {
                    if let PipelineFilter::CreditSqueezeBranches(branches) = f {
                        Some(&branches.normal)
                    } else {
                        None
                    }
                })
                .expect("squeeze branches");
            let original_video: Vec<_> = ordinary
                .filter_chain
                .filters
                .iter()
                .filter(|f| !matches!(f, PipelineFilter::Audio(_)))
                .collect();
            assert_eq!(
                format!(
                    "{:?}",
                    normal
                        .filters
                        .iter()
                        .filter(|f| !matches!(
                            f,
                            PipelineFilter::Video(VideoFilter::CreditSqueezeColor(_))
                        ))
                        .collect::<Vec<_>>()
                ),
                format!("{original_video:?}")
            );
            assert!(matches!(
                normal.filters.last(),
                Some(PipelineFilter::Video(VideoFilter::CreditSqueezeColor(_)))
            ));
            assert!(ordinary.output_context.audio_codec == squeezed.output_context.audio_codec);
            assert_eq!(
                ordinary.output_context.pts_offset.map(|o| o.duration),
                squeezed.output_context.pts_offset.map(|o| o.duration)
            );
            assert!(
                !degradation_codes(&squeezed).contains(&DegradationCode::CreditSqueezeUnavailable)
            );
            let args = squeezed.args();
            assert_eq!(arg_value(&ordinary.args(), "-t"), arg_value(&args, "-t"));
            assert!(args.iter().any(|arg| arg.contains("trim=end=5")));
            let graph = arg_value(&args, "-filter_complex").expect("squeeze filter graph");
            let join = graph
                .split(';')
                .find(|chain| chain.ends_with("[cs_joined]"))
                .expect("normal-to-credits join");
            assert_eq!(
                join,
                "[csn_v][cs_output]concat=n=2:v=1:a=0:preserve_ts=1[cs_joined]"
            );
            assert!(args.iter().any(|arg| arg.contains("out_color_matrix=bt709:out_color_primaries=bt709:out_color_transfer=bt709:out_range=tv,setparams=")));
        }
    }

    #[test]
    fn credit_squeeze_join_after_marker_has_no_fullscreen_lead_in() {
        let pipeline = squeeze_pipeline(true, 8, true);
        let args = pipeline.args();
        assert!(
            args.iter()
                .any(|arg| arg.contains("pos_w='if(eq(idx,1),ow*0.55,ow)'"))
        );
        assert!(!args.iter().any(|arg| arg.contains("concat=n=2")));
        assert!(!args.iter().any(|arg| arg.contains("trim=end=")));
    }

    #[test]
    fn incompatible_hdr_output_obeys_fallback_or_strict_mode() {
        for dv5 in [false, true] {
            for mode in [
                ToneMappingMode::On,
                ToneMappingMode::SdrFallback,
                ToneMappingMode::Off,
            ] {
                let mut input = hdr10_input();
                if dv5
                    && let crate::probe::ProbeResultStream::Video(video) =
                        &mut input.video_input.probe_result.streams[0]
                {
                    video.dv_profile = Some(5);
                }
                let mut output = stereo_output();
                output.video.transcode.tone_mapping = mode;
                output.video.transcode.bit_depth = if dv5 { 10 } else { 8 };
                let result = Pipeline::full(&FfmpegInfo::default(), input, output);
                if mode == ToneMappingMode::Off {
                    assert!(matches!(
                        result,
                        Err(FFPipelineError::HdrPreservationUnsupported(_))
                    ));
                } else {
                    let pipeline = result.unwrap();
                    assert_eq!(
                        degradation_codes(&pipeline).contains(&DegradationCode::HdrToneMapped),
                        mode == ToneMappingMode::SdrFallback
                    );
                }
            }
        }
    }

    #[test]
    fn hdr10_drawn_in_the_stage_reports_no_shader_or_tonemap_degradation() {
        let codes = degradation_codes(&hdr10_shader_pipeline(true));
        assert!(
            !codes.contains(&DegradationCode::CustomShaderHdrSkipped),
            "{codes:?}"
        );
        assert!(
            !codes.contains(&DegradationCode::HardwareTonemapUnavailable),
            "{codes:?}"
        );
    }

    #[test]
    fn hdr10_without_the_lut_filter_reports_the_skipped_shader() {
        let codes = degradation_codes(&hdr10_shader_pipeline(false));
        assert!(
            codes.contains(&DegradationCode::CustomShaderHdrSkipped),
            "{codes:?}"
        );
        // libplacebo still tone-maps on the gpu
        assert!(
            !codes.contains(&DegradationCode::HardwareTonemapUnavailable),
            "{codes:?}"
        );
    }

    #[test]
    fn hdr_tone_mapped_in_software_under_an_accel_is_reported() {
        use crate::filter_chain::tests::{ffmpeg_info_with_filters, vaapi_accel_with_libplacebo};
        let mut ffmpeg_info = ffmpeg_info_with_filters(&[]);
        ffmpeg_info
            .hwaccels
            .insert(crate::ffmpeg_info::KnownHardwareAccel::Vaapi.to_string());
        let output = OutputSettings {
            accel: Some(HardwareAccel::Vaapi(vaapi_accel_with_libplacebo(true))),
            ..stereo_output()
        };
        let mut pipeline = Pipeline::full(&ffmpeg_info, hdr10_input(), output).unwrap();
        pipeline.optimize();

        assert!(
            pipeline.args().join(" ").contains("zscale=t=linear"),
            "{:?}",
            pipeline.args()
        );
        let tonemap = pipeline
            .degradations()
            .iter()
            .find(|d| d.code == DegradationCode::HardwareTonemapUnavailable)
            .expect("tonemap degradation");
        assert_eq!(
            tonemap.message,
            "VAAPI hardware tone mapping is unavailable for this HDR item. Using CPU tone mapping."
        );
    }

    #[test]
    fn decoding_and_encoding_in_software_under_an_accel_are_reported() {
        use crate::filter_chain::tests::{ffmpeg_info_with_filters, vaapi_accel_with_libplacebo};
        let mut ffmpeg_info = ffmpeg_info_with_filters(&[]);
        ffmpeg_info
            .hwaccels
            .insert(crate::ffmpeg_info::KnownHardwareAccel::Vaapi.to_string());
        // the test device lists no decode or encode profiles
        let output = OutputSettings {
            accel: Some(HardwareAccel::Vaapi(vaapi_accel_with_libplacebo(true))),
            ..stereo_output()
        };
        let mut pipeline =
            Pipeline::full(&ffmpeg_info, multichannel_ac3_input("main.mkv"), output).unwrap();
        pipeline.optimize();

        assert_eq!(
            degradation_codes(&pipeline),
            [
                DegradationCode::HardwareEncodeUnavailable,
                DegradationCode::HardwareDecodeUnavailable
            ]
        );
        assert!(
            pipeline.degradations()[0]
                .message
                .starts_with("VAAPI hardware encoding cannot be used for 8-bit H.264 output."),
            "{:?}",
            pipeline.degradations()
        );
    }

    #[rstest::rstest]
    #[case::the_device_could_not_be_opened(None, Some("VAAPI"))]
    #[case::ffmpeg_lacks_the_accel(
        Some(HardwareAccel::Vaapi(crate::filter_chain::tests::vaapi_accel_with_libplacebo(true))),
        None
    )]
    fn an_accel_the_channel_cannot_use_reports_cpu_decode_and_encode(
        #[case] accel: Option<HardwareAccel>,
        #[case] unavailable_accel: Option<&str>,
    ) {
        let output = OutputSettings {
            accel,
            unavailable_accel: unavailable_accel.map(String::from),
            ..stereo_output()
        };
        let mut pipeline = Pipeline::full(
            &FfmpegInfo::default(),
            multichannel_ac3_input("main.mkv"),
            output,
        )
        .unwrap();
        pipeline.optimize();

        assert_eq!(
            degradation_codes(&pipeline),
            [
                DegradationCode::HardwareEncodeUnavailable,
                DegradationCode::HardwareDecodeUnavailable
            ]
        );
        assert!(
            pipeline.degradations()[0].message.starts_with(
                "VAAPI is unavailable to this channel, so hardware encoding cannot be used for 8-bit H.264 output."
            ),
            "{:?}",
            pipeline.degradations()
        );
    }

    #[test]
    fn software_without_an_accel_reports_nothing() {
        let mut pipeline = Pipeline::full(
            &FfmpegInfo::default(),
            multichannel_ac3_input("main.mkv"),
            stereo_output(),
        )
        .unwrap();
        pipeline.optimize();

        assert!(
            pipeline.degradations().is_empty(),
            "{:?}",
            pipeline.degradations()
        );
    }

    #[test]
    fn hdr10_shader_tone_maps_in_the_stage_and_skips_the_separate_tonemap() {
        let args = hdr10_shader_args(true);
        assert!(
            args.contains(
                "scale_vaapi=w=854:h=480:format=p010,tonemap_vaapi_lut,hwmap=derive_device=drm"
            ),
            "{args}"
        );
        assert_eq!(args.matches("tonemap").count(), 1, "{args}");
    }

    #[test]
    fn hdr10_without_the_lut_filter_skips_the_shader_and_tone_maps() {
        let args = hdr10_shader_args(false);
        assert!(!args.contains("custom_shader_bin"), "{args}");
        assert!(args.contains("tonemap"), "{args}");
    }

    #[test]
    fn custom_shader_without_vaapi_keeps_the_plain_scale() {
        let mut output = stereo_output();
        output.video.transcode.filter_options.custom_shader =
            Some(crate::output_settings::CustomShader::new("//!HOOK MAIN"));
        let mut pipeline = Pipeline::full(
            &FfmpegInfo::default(),
            multichannel_ac3_input("main.mkv"),
            output,
        )
        .unwrap();
        pipeline.optimize();
        let args = pipeline.args().join(" ");

        assert!(!args.contains("libplacebo"), "{args}");
        assert!(args.contains("[0:0]scale=1080:720:"), "{args}");
        assert_eq!(
            degradation_codes(&pipeline),
            [DegradationCode::CustomShaderUnavailable]
        );
    }

    #[test]
    fn copy_omits_encoder_options() {
        let mut output = stereo_output();
        output.video.copy = Some(crate::output_settings::CopyPolicy::default());
        output.audio.copy = Some(crate::output_settings::CopyPolicy::default());
        let mut pipeline = Pipeline::full(
            &FfmpegInfo::default(),
            multichannel_ac3_input("main.mkv"),
            output,
        )
        .unwrap();
        pipeline.optimize();
        let args = pipeline.args();

        for (option, value) in [("-vcodec", "copy"), ("-acodec", "copy")] {
            let index = args.iter().rposition(|a| a == option).expect(option);
            assert_eq!(args[index + 1], value, "{args:?}");
        }
        for option in [
            "-g",
            "-force_key_frames",
            "-b:v",
            "-maxrate:v",
            "-b:a",
            "-ac",
            "-ar",
        ] {
            assert!(!args.iter().any(|a| a == option), "{option}: {args:?}");
        }
    }

    #[test]
    fn copy_transcodes_blocked_stream_only() {
        let mut input = multichannel_ac3_input("main.mkv");
        if let crate::probe::ProbeResultStream::Video(video) =
            &mut input.video_input.probe_result.streams[0]
        {
            video.codec = "vc1".to_owned();
        }
        let mut output = stereo_output();
        output.video.copy = Some(crate::output_settings::CopyPolicy::default());
        output.audio.copy = Some(crate::output_settings::CopyPolicy::default());
        let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, output).unwrap();
        pipeline.optimize();
        let args = pipeline.args();

        for (option, value) in [("-vcodec", "libx264"), ("-acodec", "copy")] {
            let index = args.iter().rposition(|a| a == option).expect(option);
            assert_eq!(args[index + 1], value, "{args:?}");
        }
        assert_eq!(
            pipeline.copy_decisions().transcode_summary().as_deref(),
            Some("video (vc1 is not in copy_formats)")
        );
    }

    #[test]
    fn image_subtitle_burns_at_rotated_source_size_without_target_size() {
        let mut input = multichannel_ac3_input("main.mkv");
        if let crate::probe::ProbeResultStream::Video(video) =
            &mut input.video_input.probe_result.streams[0]
        {
            video.rotation = Some(90);
        }
        let mut subtitle = multichannel_ac3_input("main.sup").video_input;
        subtitle.probe_result.streams.truncate(1);
        if let crate::probe::ProbeResultStream::Video(video) = &mut subtitle.probe_result.streams[0]
        {
            video.codec_type = crate::probe::CodecType::Subtitle;
            video.codec = "hdmv_pgs_subtitle".to_owned();
        }
        input.subtitle_input = Some(subtitle);
        let mut output = stereo_output();
        output.video.transcode.size = None;
        output.video.transcode.filter_options.scale.flags = Some("lanczos".to_owned());
        output.video.copy = Some(crate::output_settings::CopyPolicy::default());
        let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, output).unwrap();
        pipeline.optimize();
        let args = pipeline.args();

        let filter = args
            .windows(2)
            .filter(|a| a[0] == "-filter_complex")
            .map(|a| a[1].as_ref())
            .collect::<Vec<_>>()
            .join(";");
        assert!(
            filter.contains("scale=480:720:flags=lanczos:force_original_aspect_ratio"),
            "{filter}"
        );
        assert_eq!(
            pipeline.copy_decisions().transcode_summary().as_deref(),
            Some("video (image subtitles)")
        );
    }

    fn image_subtitle_input(path: &str, codec: &str, size: Option<(u32, u32)>) -> ProbedInput {
        let mut subtitle = multichannel_ac3_input(path).video_input;
        subtitle.probe_result.streams.truncate(1);
        if let crate::probe::ProbeResultStream::Video(video) = &mut subtitle.probe_result.streams[0]
        {
            video.codec_type = crate::probe::CodecType::Subtitle;
            video.codec = codec.to_owned();
            video.stream_index = 7;
            video.width = size.map(|(w, _)| w);
            video.height = size.map(|(_, h)| h);
        }
        subtitle
    }

    fn args_by_input(args: &ArgVec) -> Vec<(String, Vec<String>)> {
        let mut start = 0;
        let mut result = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if arg == "-i" {
                result.push((
                    args[i + 1].to_string(),
                    args[start..i].iter().map(|a| a.to_string()).collect(),
                ));
                start = i + 2;
            }
        }
        result
    }

    fn canvas_size_args(input: InputSettings) -> Vec<(String, Vec<String>)> {
        let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).unwrap();
        pipeline.optimize();
        args_by_input(&pipeline.args())
            .into_iter()
            .map(|(path, args)| {
                let canvas = args
                    .windows(2)
                    .filter(|a| a[0].starts_with("-canvas_size"))
                    .flat_map(|a| a.to_vec())
                    .collect();
                (path, canvas)
            })
            .collect()
    }

    #[test]
    fn pgs_cadence_is_limited_before_scaling_to_the_video_rate() {
        for target in [None, Some("25")] {
            let mut input = multichannel_ac3_input("main.mkv");
            input.subtitle_input =
                Some(image_subtitle_input("main.mkv", "hdmv_pgs_subtitle", None));
            let mut output = stereo_output();
            output.frame_rate = target.and_then(FrameRate::parse_target);
            let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, output).unwrap();
            pipeline.optimize();
            let filter = filter_complex(&pipeline.args());
            let rate = target.unwrap_or("30000/1001");
            assert!(
                filter.contains(&format!("[0:7]fps={rate},scale=")),
                "{filter}"
            );
            assert!(filter.contains(":shortest=1[v_o0]"), "{filter}");
        }
    }

    #[test]
    fn only_a_rate_limited_subtitle_overlay_ends_with_the_video() {
        let mut input = multichannel_ac3_input("main.mkv");
        input.subtitle_input = Some(image_subtitle_input("main.mkv", "dvd_subtitle", None));
        let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).unwrap();
        pipeline.optimize();
        let filter = filter_complex(&pipeline.args());

        assert!(!filter.contains("fps="), "{filter}");
        assert!(!filter.contains("shortest"), "{filter}");
    }

    #[test]
    fn embedded_pgs_without_size_sets_canvas_on_shared_input() {
        let mut input = multichannel_ac3_input("main.mkv");
        input.subtitle_input = Some(image_subtitle_input("main.mkv", "hdmv_pgs_subtitle", None));
        assert_eq!(
            canvas_size_args(input),
            [(
                "main.mkv".to_owned(),
                vec!["-canvas_size:7".to_owned(), "1920x1080".to_owned()]
            )]
        );
    }

    #[test]
    fn external_pgs_without_size_sets_canvas_on_its_own_input() {
        let mut input = multichannel_ac3_input("main.mkv");
        input.subtitle_input = Some(image_subtitle_input("main.sup", "pgssub", None));
        assert_eq!(
            canvas_size_args(input),
            [
                ("main.mkv".to_owned(), vec![]),
                (
                    "main.sup".to_owned(),
                    vec!["-canvas_size:7".to_owned(), "1920x1080".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn image_subtitle_canvas_is_left_to_ffmpeg_when_size_known_or_not_pgs() {
        for subtitle in [
            image_subtitle_input("main.mkv", "hdmv_pgs_subtitle", Some((1920, 1080))),
            image_subtitle_input("main.mkv", "dvd_subtitle", None),
        ] {
            let mut input = multichannel_ac3_input("main.mkv");
            input.subtitle_input = Some(subtitle);
            assert_eq!(canvas_size_args(input), [("main.mkv".to_owned(), vec![])]);
        }
    }

    fn separate_audio_input(input_source: InputSource, seek: Duration) -> InputSettings {
        let mut input = multichannel_ac3_input("main.mkv");
        input.video_input.in_point = seek;
        input.audio_input = multichannel_ac3_input("song.flac").audio_input;
        input.audio_input.input_source = input_source;
        input.audio_input.in_point = seek;
        input
    }

    fn input_seeks(args: &ArgVec) -> Vec<(String, Option<String>)> {
        args.iter()
            .enumerate()
            .filter(|(_, a)| *a == "-i")
            .map(|(i, _)| {
                let input_args = &args[..i];
                let start = input_args
                    .iter()
                    .rposition(|a| a == "-i")
                    .map_or(0, |p| p + 2);
                let seek = input_args[start..]
                    .windows(2)
                    .find(|w| w[0] == "-ss")
                    .map(|w| w[1].to_string());
                (args[i + 1].to_string(), seek)
            })
            .collect()
    }

    #[test]
    fn separate_audio_input_seeks_with_video() {
        let input = separate_audio_input(
            InputSource::Local(crate::input::LocalInputSource {
                path: "song.flac".to_owned(),
            }),
            Duration::from_millis(12_345),
        );
        let pipeline = Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).unwrap();

        assert_eq!(
            input_seeks(&pipeline.args()),
            vec![
                ("main.mkv".to_owned(), Some("12345ms".to_owned())),
                ("song.flac".to_owned(), Some("12345ms".to_owned())),
            ]
        );
    }

    #[test]
    fn separate_lavfi_audio_input_never_seeks() {
        let input = separate_audio_input(
            InputSource::Lavfi(crate::input::LavfiInputSource {
                params: "anullsrc".to_owned(),
            }),
            Duration::from_millis(12_345),
        );
        let pipeline = Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).unwrap();

        assert_eq!(
            input_seeks(&pipeline.args()),
            vec![
                ("main.mkv".to_owned(), Some("12345ms".to_owned())),
                ("song.flac".to_owned(), None),
            ]
        );
    }

    fn video_only_input(seek: Duration) -> InputSettings {
        let mut input = multichannel_ac3_input("video_only.mkv");
        for probed in [&mut input.audio_input, &mut input.video_input] {
            probed
                .probe_result
                .streams
                .retain(|s| !matches!(s, crate::probe::ProbeResultStream::Audio(_)));
            probed.in_point = seek;
        }
        input
    }

    #[test]
    fn video_only_source_uses_silence() {
        let pipeline = Pipeline::full(
            &FfmpegInfo::default(),
            video_only_input(Duration::from_millis(12_345)),
            stereo_output(),
        )
        .unwrap();

        assert_eq!(
            input_seeks(&pipeline.args()),
            vec![
                ("video_only.mkv".to_owned(), Some("12345ms".to_owned())),
                (
                    "anullsrc=channel_layout=stereo:sample_rate=48000".to_owned(),
                    None
                ),
            ]
        );
    }

    #[test]
    fn video_only_source_ignores_missing_audio_stream_index() {
        let mut input = video_only_input(Duration::ZERO);
        input.audio_input.stream_index = Some(1);

        assert!(Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).is_ok());
    }

    #[test]
    fn video_only_source_predicts_audio_transcode() {
        let mut output = stereo_output();
        output.audio.copy = Some(crate::output_settings::CopyPolicy::default());

        let decisions = predict_copy_decisions(&video_only_input(Duration::ZERO), &output).unwrap();

        assert!(
            matches!(
                &decisions.audio,
                Some(CopyDecision::Transcode(blockers))
                    if blockers.contains(&crate::copy_decision::CopyBlocker::GeneratedSource)
            ),
            "{:?}",
            decisions.audio
        );
    }

    #[test]
    fn ac3_downmix_is_emitted_when_audio_shares_the_video_input() {
        let path = "/tmp/shared.mkv";
        let pipeline = Pipeline::full(
            &FfmpegInfo::default(),
            multichannel_ac3_input(path),
            stereo_output(),
        )
        .unwrap();

        let args = pipeline.args();
        let downmix = args.iter().position(|a| a == "-downmix").expect("-downmix");
        let input = args.iter().position(|a| a == "-i").expect("-i");

        assert_eq!(args[downmix + 1], "stereo");
        assert!(downmix < input, "-downmix must precede -i: {args:?}");
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 1);
    }

    fn amf_encoder_only() -> HardwareAccel {
        use crate::capabilities::amf::{AmfCapabilities, AmfEncoderCapability};

        let encoder = AmfEncoderCapability {
            bit_depths: vec![8],
            b_frames: false,
            max_profile: None,
            max_level: None,
        };
        HardwareAccel::Amf(crate::accel::amf::Amf {
            capabilities: AmfCapabilities {
                supported_decoders: Default::default(),
                supported_encoders: [
                    (VideoFormat::H264, encoder.clone()),
                    (VideoFormat::Hevc, encoder),
                ]
                .into(),
                vpp_input_formats: Default::default(),
                vpp_output_formats: Default::default(),
                runtime_version: None,
                device: None,
                adapter: None,
            },
        })
    }

    fn amf_args(hdr: bool, format: EncodeFormat) -> ArgVec {
        let mut input = multichannel_ac3_input("main.mkv");
        if hdr {
            for probe in [
                &mut input.video_input.probe_result,
                &mut input.audio_input.probe_result,
            ] {
                if let crate::probe::ProbeResultStream::Video(video) = &mut probe.streams[0] {
                    video.codec = "hevc".to_owned();
                    video.pix_fmt = "yuv420p10le".to_owned();
                    video.color_params = crate::probe::ProbeResultColorParams {
                        color_range: Some("tv".to_owned()),
                        color_space: Some("bt2020nc".to_owned()),
                        color_transfer: Some("smpte2084".to_owned()),
                        color_primaries: Some("bt2020".to_owned()),
                        has_hdr10_metadata: false,
                    };
                }
            }
        }
        let mut output = OutputSettings {
            accel: Some(amf_encoder_only()),
            ..stereo_output()
        };
        output.video.transcode.format = format;
        let ffmpeg_info = FfmpegInfo {
            hwaccels: [crate::ffmpeg_info::KnownHardwareAccel::Amf.to_string()].into(),
            ..FfmpegInfo::default()
        };
        let mut pipeline = Pipeline::full(&ffmpeg_info, input, output).unwrap();
        pipeline.optimize();
        let args = pipeline.args();
        assert!(
            args.iter().any(|a| a.ends_with("_amf")),
            "amf encoder not used: {args:?}"
        );
        args
    }

    #[test]
    fn amf_tonemap_writes_bt709_tags_with_bsf() {
        for (format, bsf) in [
            (EncodeFormat::H264, "h264_metadata"),
            (EncodeFormat::Hevc, "hevc_metadata"),
        ] {
            let args = amf_args(true, format);
            let index = args
                .iter()
                .position(|a| a == "-bsf:v")
                .unwrap_or_else(|| panic!("-bsf:v missing: {args:?}"));
            assert_eq!(
                args[index + 1],
                format!(
                    "{bsf}=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1"
                )
            );
            // ffmpeg ignores options after the output path
            let output = args.iter().position(|a| a == "-f").expect("-f");
            assert!(index < output, "-bsf:v must precede output: {args:?}");
        }
    }

    #[test]
    fn amf_without_tonemap_has_no_bsf() {
        let args = amf_args(false, EncodeFormat::Hevc);
        assert!(!args.iter().any(|a| a == "-bsf:v"), "{args:?}");
    }

    fn video_toolbox_args(format: EncodeFormat, preserve_source_aspect: bool) -> ArgVec {
        use crate::capabilities::videotoolbox::VideoToolboxCapabilities;

        let accel = HardwareAccel::VideoToolbox(crate::accel::video_toolbox::VideoToolbox {
            capabilities: VideoToolboxCapabilities {
                supported_decoders: Default::default(),
                supported_interlaced_decoders: Default::default(),
                supported_encoders: [(VideoFormat::H264, 8), (VideoFormat::Hevc, 8)].into(),
            },
        });
        let mut output = OutputSettings {
            accel: Some(accel),
            ..stereo_output()
        };
        output.video.transcode.format = format;
        output.video.transcode.preserve_source_aspect = preserve_source_aspect;
        let ffmpeg_info = FfmpegInfo {
            hwaccels: [crate::ffmpeg_info::KnownHardwareAccel::VideoToolbox.to_string()].into(),
            ..FfmpegInfo::default()
        };
        let mut pipeline =
            Pipeline::full(&ffmpeg_info, multichannel_ac3_input("main.mkv"), output).unwrap();
        pipeline.optimize();
        let args = pipeline.args();
        assert!(
            args.iter().any(|a| a.ends_with("_videotoolbox")),
            "videotoolbox encoder not used: {args:?}"
        );
        args
    }

    #[test]
    fn video_toolbox_h264_writes_sar_with_bsf() {
        let args = video_toolbox_args(EncodeFormat::H264, false);
        let bsfs: Vec<_> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-bsf:v")
            .map(|(i, _)| &args[i + 1])
            .collect();
        assert_eq!(bsfs, ["h264_metadata=sample_aspect_ratio=1/1"], "{args:?}");
    }

    #[test]
    fn source_aspect_survives_video_toolbox_header_fix() {
        let args = video_toolbox_args(EncodeFormat::H264, true);
        let index = args.iter().position(|arg| arg == "-bsf:v").unwrap();
        assert_eq!(
            args[index + 1],
            "h264_metadata=sample_aspect_ratio=777600/921600"
        );
    }

    #[test]
    fn video_toolbox_hevc_has_no_bsf() {
        let args = video_toolbox_args(EncodeFormat::Hevc, false);
        assert!(!args.iter().any(|a| a == "-bsf:v"), "{args:?}");
    }

    #[test]
    fn metadata_bsf_combines_fixups_into_one_filter() {
        let bsf = MetadataBsf {
            filter: "h264_metadata",
            sample_aspect_ratio: Some((1, 1)),
            bt709: true,
        };
        assert_eq!(
            bsf.as_arg(),
            [
                "-bsf:v",
                "h264_metadata=sample_aspect_ratio=1/1:colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1",
            ]
        );
    }

    fn canvas_input(source: InputSource) -> GraphicsInput {
        let mut probe = multichannel_ac3_input("canvas.nut")
            .video_input
            .probe_result;
        probe.path = source.input_path().unwrap();
        probe.format_name = Some("nut".to_owned());
        probe
            .streams
            .retain(|s| matches!(s, crate::probe::ProbeResultStream::Video(_)));
        if let crate::probe::ProbeResultStream::Video(video) = &mut probe.streams[0] {
            video.codec = "ffv1".to_owned();
            video.pix_fmt = "bgra".to_owned();
            video.width = Some(1280);
            video.height = Some(720);
        }
        GraphicsInput {
            layer_index: 0,
            input_source: source,
            probe_result: probe,
            stream_index: None,
            kind: GraphicsKind::Canvas(CanvasLayout::FullFrame),
            in_point: Duration::from_secs(3),
        }
    }

    #[rstest::rstest]
    #[case(940, 540, Some(None))]
    #[case(0, 0, Some(None))]
    #[case(1100, 600, Some(Some("crop=180:120:0:0")))]
    #[case(1280, 0, None)]
    #[case(u32::MAX, u32::MAX, None)]
    fn region_canvas_overlays_at_origin_clipped_to_output(
        #[case] x: u32,
        #[case] y: u32,
        #[case] expected: Option<Option<&str>>,
    ) {
        let mut input = multichannel_ac3_input("main.mkv");
        let mut graphics = canvas_input(InputSource::Local(crate::input::LocalInputSource {
            path: "canvas.nut".to_owned(),
        }));
        if let crate::probe::ProbeResultStream::Video(video) = &mut graphics.probe_result.streams[0]
        {
            video.width = Some(300);
            video.height = Some(150);
        }
        graphics.kind = GraphicsKind::Canvas(CanvasLayout::Region {
            origin: FramePoint { x, y },
        });
        input.graphics_inputs.push(graphics);
        let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).unwrap();
        pipeline.optimize();
        let args = pipeline.args();
        let filter = args
            .windows(2)
            .filter(|a| a[0] == "-filter_complex")
            .map(|a| a[1].as_ref())
            .collect::<Vec<_>>()
            .join(";");

        let Some(crop) = expected else {
            assert!(!args.iter().any(|a| a == "canvas.nut"), "{args:?}");
            assert!(!filter.contains("overlay"), "{filter}");
            return;
        };
        let canvas_chain = filter.split(';').find(|c| c.starts_with("[1:0]")).unwrap();
        assert!(!canvas_chain.contains("scale="), "{filter}");
        match crop {
            Some(crop) => assert!(canvas_chain.contains(crop), "{filter}"),
            None => assert!(!canvas_chain.contains("crop="), "{filter}"),
        }
        assert!(filter.contains(&format!("overlay=x={x}:y={y}")), "{filter}");
    }

    #[test]
    fn canvas_sized_for_another_resolution_is_scaled_instead_of_failing() {
        let mut input = multichannel_ac3_input("main.mkv");
        let mut graphics = canvas_input(InputSource::Local(crate::input::LocalInputSource {
            path: "canvas.nut".to_owned(),
        }));
        if let crate::probe::ProbeResultStream::Video(video) = &mut graphics.probe_result.streams[0]
        {
            video.width = Some(1920);
            video.height = Some(1080);
        }
        input.graphics_inputs.push(graphics);
        let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).unwrap();
        pipeline.optimize();
        let filter = pipeline
            .args()
            .windows(2)
            .filter(|a| a[0] == "-filter_complex")
            .map(|a| a[1].as_ref())
            .collect::<Vec<_>>()
            .join(";");
        assert!(filter.contains("scale=1280:720"), "{filter}");
        assert!(filter.contains("overlay=x=0:y=0"), "{filter}");
    }

    #[test]
    fn canvas_local_seeks_to_schedule_offset_plus_source_in_point() {
        for offset in [0, 44, 88] {
            let mut input = multichannel_ac3_input("main.mkv");
            input.playout_offset = Duration::from_secs(offset);
            input.graphics_inputs.push(canvas_input(InputSource::Local(
                crate::input::LocalInputSource {
                    path: "canvas.nut".to_owned(),
                },
            )));
            let mut pipeline =
                Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).unwrap();
            pipeline.optimize();
            let args = pipeline.args();
            let canvas = args.iter().position(|a| a == "canvas.nut").unwrap();
            assert_eq!(
                args[canvas - 5..canvas]
                    .iter()
                    .map(|a| a.as_ref())
                    .collect::<Vec<_>>(),
                [
                    "-ss",
                    &format!("{}ms", (offset + 3) * 1000),
                    "-t",
                    "30000ms",
                    "-i"
                ]
            );
            assert!(
                !args
                    .iter()
                    .any(|a| matches!(a.as_ref(), "-stream_loop" | "-ignore_loop" | "-framerate"))
            );
            let filter = args
                .windows(2)
                .filter(|a| a[0] == "-filter_complex")
                .map(|a| a[1].as_ref())
                .collect::<Vec<_>>()
                .join(";");
            assert!(filter.contains("overlay=x=0:y=0"), "{filter}");
            assert!(
                !filter.contains("fade=")
                    && !filter.contains("colorchannelmixer")
                    && !filter.contains("loop="),
                "{filter}"
            );
        }
    }

    fn qsv_with_runtime(runtime_api: (u16, u16)) -> HardwareAccel {
        HardwareAccel::Qsv(crate::accel::qsv::Qsv {
            capabilities: crate::capabilities::qsv::QsvCapabilities {
                supported_decoders: Default::default(),
                supported_encoders: Default::default(),
                upload_formats: Default::default(),
                convert_pairs: Default::default(),
                vpp_filters: Default::default(),
                rotation_formats: Default::default(),
                composite_pairs: Default::default(),
                runtime_api: Some(runtime_api),
            },
        })
    }

    fn loudnorm_args(seek: Duration, canvas: bool, loudness: bool) -> ArgVec {
        loudnorm_args_with(seek, canvas, loudness, (1, 35), true)
    }

    fn loudnorm_args_with(
        seek: Duration,
        canvas: bool,
        loudness: bool,
        runtime_api: (u16, u16),
        overlay_qsv: bool,
    ) -> ArgVec {
        loudnorm_args_in(
            Some("matroska,webm"),
            seek,
            canvas,
            loudness,
            runtime_api,
            overlay_qsv,
        )
    }

    fn loudnorm_args_in(
        format_name: Option<&str>,
        seek: Duration,
        canvas: bool,
        loudness: bool,
        runtime_api: (u16, u16),
        overlay_qsv: bool,
    ) -> ArgVec {
        let mut input = multichannel_ac3_input("main.mkv");
        input.audio_input.probe_result.format_name = format_name.map(str::to_owned);
        input.video_input.in_point = seek;
        input.audio_input.in_point = seek;
        if canvas {
            input.graphics_inputs.push(canvas_input(InputSource::Local(
                crate::input::LocalInputSource {
                    path: "canvas.nut".to_owned(),
                },
            )));
        }
        let mut output = stereo_output();
        if loudness {
            output.audio.transcode.loudness =
                Some(crate::output_settings::AudioLoudnessSettings::default());
        }
        output.accel = Some(qsv_with_runtime(runtime_api));
        let ffmpeg_info = FfmpegInfo {
            hwaccels: [crate::ffmpeg_info::KnownHardwareAccel::Qsv.to_string()].into(),
            video_filters: overlay_qsv
                .then(|| crate::ffmpeg_info::KnownVideoFilter::OverlayQsv.to_string())
                .into_iter()
                .collect(),
            ..FfmpegInfo::default()
        };
        let mut pipeline = Pipeline::full(&ffmpeg_info, input, output).unwrap();
        pipeline.optimize();
        pipeline.args()
    }

    #[test]
    fn own_input_keeps_sync_for_containers_without_timestamp_discontinuities() {
        assert!(super::own_input_keeps_sync(Some("matroska,webm")));
        assert!(super::own_input_keeps_sync(Some("avi")));
        assert!(!super::own_input_keeps_sync(Some("mpegts")));
        assert!(!super::own_input_keeps_sync(Some("hls,applehttp")));
        assert!(!super::own_input_keeps_sync(None));
    }

    #[test]
    fn loudnorm_with_canvas_reads_audio_through_its_own_input() {
        let args = loudnorm_args(Duration::from_millis(12_345), true, true);

        assert_eq!(
            input_seeks(&args),
            vec![
                ("main.mkv".to_owned(), Some("12345ms".to_owned())),
                ("main.mkv".to_owned(), Some("12345ms".to_owned())),
                ("canvas.nut".to_owned(), Some("3000ms".to_owned())),
            ]
        );

        let filter = args
            .windows(2)
            .filter(|a| a[0] == "-filter_complex")
            .map(|a| a[1].as_ref())
            .collect::<Vec<_>>()
            .join(";");
        assert!(filter.contains("[1:1]loudnorm="), "{filter}");
        assert!(filter.contains("[0:0]"), "{filter}");
        assert!(filter.contains("overlay_qsv"), "{filter}");

        // the downmix belongs to the audio input, not the video input that shares its file
        let inputs: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-i")
            .map(|(i, _)| i)
            .collect();
        let downmix = args.iter().position(|a| a == "-downmix").expect("-downmix");
        assert!(inputs[0] < downmix && downmix < inputs[1], "{args:?}");

        assert_eq!(shortest_buf_duration(&args), Some("0.500"), "{args:?}");
    }

    fn shortest_buf_duration(args: &ArgVec) -> Option<&str> {
        assert!(args.iter().any(|a| a == "-shortest"), "{args:?}");
        args.windows(2)
            .find(|a| a[0] == "-shortest_buf_duration")
            .map(|a| a[1].as_ref())
    }

    #[test]
    fn audio_shares_the_video_input_with_dynamic_qsv_pools() {
        for runtime_api in [(2, 9), (2, 10), (2, 15)] {
            let args = loudnorm_args_with(Duration::ZERO, true, true, runtime_api, true);
            assert_eq!(
                args.iter().filter(|a| *a == "main.mkv").count(),
                1,
                "runtime {runtime_api:?}: {args:?}"
            );
            assert_eq!(shortest_buf_duration(&args), None, "{args:?}");
        }
    }

    #[test]
    fn audio_shares_the_video_input_with_a_software_canvas_overlay() {
        let args = loudnorm_args_in(Some("mpegts"), Duration::ZERO, true, true, (1, 35), false);
        assert_eq!(
            args.iter().filter(|a| *a == "main.mkv").count(),
            1,
            "{args:?}"
        );
        // encoder input is still qsv frames from a fixed pool
        assert_eq!(shortest_buf_duration(&args), Some("0.500"), "{args:?}");
    }

    #[test]
    fn audio_shares_the_video_input_without_loudnorm_or_canvas() {
        for (canvas, loudness, buffer) in [
            (false, true, Some("0.500")),
            (true, false, None),
            (false, false, None),
        ] {
            let args = loudnorm_args_in(
                Some("mpegts"),
                Duration::ZERO,
                canvas,
                loudness,
                (1, 35),
                true,
            );
            assert_eq!(
                args.iter().filter(|a| *a == "main.mkv").count(),
                1,
                "canvas={canvas} loudness={loudness}: {args:?}"
            );
            assert_eq!(shortest_buf_duration(&args), buffer, "{args:?}");
        }
    }

    #[test]
    fn fixed_qsv_pool_reads_audio_through_its_own_input_when_sync_allows() {
        for (format_name, own) in [
            (Some("matroska,webm"), true),
            (Some("asf"), true),
            (Some("mov,mp4,m4a,3gp,3g2,mj2"), true),
            (Some("mpegts"), false),
            (Some("mpeg"), false),
            (None, false),
        ] {
            let args = loudnorm_args_in(format_name, Duration::ZERO, false, false, (1, 35), true);
            let inputs = args.iter().filter(|a| *a == "main.mkv").count();
            assert_eq!(inputs, if own { 2 } else { 1 }, "{format_name:?}: {args:?}");
        }

        let args = loudnorm_args_in(Some("asf"), Duration::ZERO, false, false, (2, 9), true);
        assert_eq!(
            args.iter().filter(|a| *a == "main.mkv").count(),
            1,
            "{args:?}"
        );
    }

    #[test]
    fn loudnorm_without_canvas_keeps_default_shortest_buffer_with_dynamic_qsv_pools() {
        let args = loudnorm_args_with(Duration::ZERO, false, true, (2, 9), true);
        assert_eq!(shortest_buf_duration(&args), None, "{args:?}");
    }

    #[test]
    fn canvas_http_headers_use_process_context_without_seeking() {
        use crate::input::{HttpInputOptions, HttpInputSource};
        let source = InputSource::Http(HttpInputSource {
            uri: "http://localhost/canvas".to_owned(),
            options: HttpInputOptions {
                headers: vec!["Authorization: Bearer test".to_owned()],
                ..Default::default()
            },
        });
        // Probing/extraction retain only configured headers.
        let probe_args = source.args_for_input().join(" ");
        assert!(probe_args.contains("Authorization: Bearer test"));
        assert!(!probe_args.contains("x-etv-"));
        for offset in [0, 44, 88] {
            for override_rate in [None, Some(FrameRate::parse("25/1"))] {
                let mut input = multichannel_ac3_input("main.mkv");
                input.channel_number = Some("12.1".to_owned());
                input.playout_offset = Duration::from_secs(offset);
                input.graphics_inputs.push(canvas_input(source.clone()));
                let mut output = stereo_output();
                output.frame_rate = override_rate.clone();
                let pipeline = Pipeline::full(&FfmpegInfo::default(), input, output).unwrap();
                let args = pipeline.args();
                let headers = args.windows(2).find(|a| a[0] == "-headers").unwrap()[1].as_ref();
                let expected_rate = override_rate
                    .as_ref()
                    .map_or("30000/1001", |r| r.r_frame_rate.as_str());
                assert_eq!(
                    headers,
                    format!(
                        "Authorization: Bearer test\r\nx-etv-channel:12.1\r\nx-etv-offset-ms:{}\r\nx-etv-duration-ms:30000\r\nx-etv-frame-rate:{expected_rate}\r\n",
                        offset * 1000
                    )
                );
                assert_eq!(args.iter().filter(|a| *a == "-headers").count(), 1);
                assert!(!args.iter().any(|a| matches!(
                    a.as_ref(),
                    "-ss" | "-stream_loop" | "-framerate" | "-ignore_loop"
                )));
                let canvas = args
                    .iter()
                    .position(|a| a == "http://localhost/canvas")
                    .unwrap();
                assert_eq!(
                    args[canvas - 3..canvas]
                        .iter()
                        .map(|a| a.as_ref())
                        .collect::<Vec<_>>(),
                    ["-t", "30000ms", "-i"]
                );
            }
        }
    }
    #[test]
    fn contextual_headers_reach_only_canvas_http_inputs() {
        use crate::input::{HttpInputOptions, HttpInputSource};
        let http = |uri: &str| {
            InputSource::Http(HttpInputSource {
                uri: uri.to_owned(),
                options: HttpInputOptions::default(),
            })
        };
        let mut input = multichannel_ac3_input("http://localhost/video");
        input.video_input.input_source = http("http://localhost/video");
        input.audio_input.input_source = http("http://localhost/audio");
        input.audio_input.probe_result.path = "http://localhost/audio".to_owned();
        // Media seeks include a source in-point; headers must report schedule time only.
        input.video_input.in_point = Duration::from_secs(54);
        input.video_input.out_point = Duration::from_secs(98);
        input.audio_input.in_point = Duration::from_secs(54);
        input.audio_input.out_point = Duration::from_secs(98);
        input.playout_offset = Duration::from_secs(44);
        input.channel_number = Some("7".to_owned());
        let mut subtitle = multichannel_ac3_input("http://localhost/subtitle").video_input;
        subtitle.input_source = http("http://localhost/subtitle");
        subtitle.probe_result.streams.truncate(1);
        if let crate::probe::ProbeResultStream::Video(video) = &mut subtitle.probe_result.streams[0]
        {
            video.codec_type = crate::probe::CodecType::Subtitle;
            video.codec = "hdmv_pgs_subtitle".to_owned();
        }
        input.subtitle_input = Some(subtitle);
        input
            .graphics_inputs
            .push(canvas_input(http("http://localhost/canvas")));
        let pipeline = Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).unwrap();
        let args = pipeline.args();
        let mut inputs = 0;
        for group in args.split_inclusive(|a| a.starts_with("http://localhost/")) {
            if !group
                .last()
                .is_some_and(|a| a.starts_with("http://localhost/"))
            {
                continue;
            }
            inputs += 1;
            let headers: Vec<_> = group.windows(2).filter(|a| a[0] == "-headers").collect();
            if group.last().is_some_and(|a| a.ends_with("/canvas")) {
                assert_eq!(headers.len(), 1, "{group:?}");
                assert_eq!(
                    headers[0][1],
                    "x-etv-channel:7\r\nx-etv-offset-ms:44000\r\nx-etv-duration-ms:44000\r\nx-etv-frame-rate:30000/1001\r\n"
                );
            } else {
                assert!(headers.is_empty(), "{group:?}");
            }
        }
        assert_eq!(inputs, 4);
    }

    fn filter_complex(args: &ArgVec) -> String {
        args.windows(2)
            .filter(|a| a[0] == "-filter_complex")
            .map(|a| a[1].as_ref())
            .collect::<Vec<_>>()
            .join(";")
    }

    fn arg_value<'a>(args: &'a ArgVec, option: &str) -> Option<&'a str> {
        args.iter()
            .rposition(|a| a == option)
            .map(|i| args[i + 1].as_ref())
    }

    #[test]
    fn target_frame_rate_converts_after_deinterlace_and_before_scale() {
        let mut input = multichannel_ac3_input("main.mkv");
        if let crate::probe::ProbeResultStream::Video(video) =
            &mut input.video_input.probe_result.streams[0]
        {
            video.field_order = Some("tt".to_owned());
        }
        let mut output = stereo_output();
        output.frame_rate = FrameRate::parse_target("25");
        let ffmpeg_info = FfmpegInfo {
            video_filters: [crate::ffmpeg_info::KnownVideoFilter::Yadif.to_string()].into(),
            ..Default::default()
        };
        let mut pipeline = Pipeline::full(&ffmpeg_info, input, output).unwrap();
        pipeline.optimize();
        let args = pipeline.args();

        let filter = filter_complex(&args);
        let position = |needle: &str| filter.find(needle).expect(&filter);
        assert!(
            position("yadif") < position("fps=25") && position("fps=25") < position("scale="),
            "{filter}"
        );
        assert_eq!(arg_value(&args, "-g"), Some("50"), "{args:?}");
        assert_eq!(arg_value(&args, "-r"), Some("25"), "{args:?}");
        assert_eq!(arg_value(&args, "-fps_mode"), Some("cfr"), "{args:?}");
    }

    #[test]
    fn no_target_frame_rate_keeps_source_rate() {
        let mut pipeline = Pipeline::full(
            &FfmpegInfo::default(),
            multichannel_ac3_input("main.mkv"),
            stereo_output(),
        )
        .unwrap();
        pipeline.optimize();
        let args = pipeline.args();

        assert!(!filter_complex(&args).contains("fps="), "{args:?}");
        assert_eq!(arg_value(&args, "-g"), Some("60"), "{args:?}");
        assert!(!args.iter().any(|a| a == "-r"), "{args:?}");
    }

    #[test]
    fn copy_with_target_frame_rate_copies_only_matching_rates() {
        for (target, copies) in [("30000/1001", true), ("25", false)] {
            let mut output = stereo_output();
            output.video.copy = Some(crate::output_settings::CopyPolicy::default());
            output.frame_rate = FrameRate::parse_target(target);
            let mut pipeline = Pipeline::full(
                &FfmpegInfo::default(),
                multichannel_ac3_input("main.mkv"),
                output,
            )
            .unwrap();
            pipeline.optimize();
            let args = pipeline.args();

            if copies {
                assert_eq!(arg_value(&args, "-vcodec"), Some("copy"), "{args:?}");
                assert!(!args.iter().any(|a| a == "-r"), "{args:?}");
            } else {
                assert_eq!(arg_value(&args, "-vcodec"), Some("libx264"), "{args:?}");
                assert_eq!(arg_value(&args, "-r"), Some(target), "{args:?}");
                assert!(filter_complex(&args).contains("fps=25"), "{args:?}");
                assert_eq!(
                    pipeline.copy_decisions().transcode_summary().as_deref(),
                    Some("video (frame rate 30000/1001 is not 25)")
                );
            }
        }
    }

    #[test]
    fn still_image_graphics_decode_at_target_frame_rate() {
        for (target, expected) in [(None, "30000/1001"), (Some("25"), "25")] {
            let mut input = multichannel_ac3_input("main.mkv");
            let mut watermark = canvas_input(InputSource::Local(crate::input::LocalInputSource {
                path: "watermark.png".to_owned(),
            }));
            watermark.kind = GraphicsKind::Media(crate::input::MediaGraphicsOptions {
                location: crate::input::GraphicsLocation::BottomRight,
                width_percent: Some(10.0),
                within_source_content: Some(true),
                horizontal_margin_percent: Some(5.0),
                vertical_margin_percent: Some(5.0),
                opacity_percent: Some(100.0),
                timing: None,
            });
            watermark.probe_result.format_name = Some("image2".to_owned());
            if let crate::probe::ProbeResultStream::Video(video) =
                &mut watermark.probe_result.streams[0]
            {
                video.codec = "png".to_owned();
                video.pix_fmt = "rgba".to_owned();
            }
            input.graphics_inputs.push(watermark);
            let mut output = stereo_output();
            output.frame_rate = target.and_then(FrameRate::parse_target);
            let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, output).unwrap();
            pipeline.optimize();
            let args = pipeline.args();

            assert_eq!(arg_value(&args, "-framerate"), Some(expected), "{args:?}");
        }
    }

    #[test]
    fn still_image_graphics_loop_ends_with_item() {
        let mut input = multichannel_ac3_input("main.mkv");
        let mut watermark = canvas_input(InputSource::Local(crate::input::LocalInputSource {
            path: "watermark.png".to_owned(),
        }));
        watermark.kind = GraphicsKind::Media(crate::input::MediaGraphicsOptions {
            location: crate::input::GraphicsLocation::BottomRight,
            width_percent: Some(10.0),
            within_source_content: Some(true),
            horizontal_margin_percent: Some(5.0),
            vertical_margin_percent: Some(5.0),
            opacity_percent: Some(100.0),
            timing: Some(crate::input::GraphicsTiming::Periodic(
                crate::input::PeriodicTiming {
                    clock: crate::input::PeriodicClock::Content,
                    frequency_ms: 10000,
                    phase_offset_ms: None,
                    disable_after_ms: None,
                    fade_ms: Some(1000),
                    hold_ms: 2000,
                },
            )),
        });
        watermark.probe_result.format_name = Some("image2".to_owned());
        if let crate::probe::ProbeResultStream::Video(video) =
            &mut watermark.probe_result.streams[0]
        {
            video.codec = "png".to_owned();
            video.pix_fmt = "rgba".to_owned();
        }
        input.graphics_inputs.push(watermark);
        let mut pipeline = Pipeline::full(&FfmpegInfo::default(), input, stereo_output()).unwrap();
        pipeline.optimize();
        let args = pipeline.args();

        // 30 s at 30000/1001
        assert_eq!(arg_value(&args, "-t"), Some("30000ms"), "{args:?}");
        assert!(
            filter_complex(&args).contains("loop=900:1,fade="),
            "{args:?}"
        );
    }

    fn qsv_h264_args(runtime_api: (u16, u16), size: FrameSize) -> ArgVec {
        qsv_h264_args_with_geometry(runtime_api, size, ScalingMode::ScaleAndPad, None)
    }

    fn qsv_h264_args_with_geometry(
        runtime_api: (u16, u16),
        size: FrameSize,
        scaling_mode: ScalingMode,
        sample_aspect_ratio: Option<&str>,
    ) -> ArgVec {
        let mut accel = qsv_with_runtime(runtime_api);
        if let HardwareAccel::Qsv(qsv) = &mut accel {
            qsv.capabilities.supported_decoders = [(VideoFormat::H264, vec![8])].into();
            qsv.capabilities.supported_encoders = [(VideoFormat::H264, vec![8])].into();
        }
        let mut output = stereo_output();
        output.video.transcode.size = Some(size);
        output.video.transcode.scaling_mode = scaling_mode;
        output.accel = Some(accel);
        let ffmpeg_info = FfmpegInfo {
            hwaccels: [crate::ffmpeg_info::KnownHardwareAccel::Qsv.to_string()].into(),
            video_filters: [crate::ffmpeg_info::KnownVideoFilter::VppQsv.to_string()].into(),
            ..FfmpegInfo::default()
        };
        let mut input = multichannel_ac3_input("main.mkv");
        if let crate::probe::ProbeResultStream::Video(video) =
            &mut input.video_input.probe_result.streams[0]
        {
            video.sample_aspect_ratio = sample_aspect_ratio.map(str::to_owned);
        }
        let mut pipeline = Pipeline::full(&ffmpeg_info, input, output).unwrap();
        pipeline.optimize();
        pipeline.args()
    }

    #[test]
    fn legacy_qsv_copies_decoder_surfaces_before_encode() {
        let source_size = FrameSize {
            width: 720,
            height: 480,
        };

        let filter = filter_complex(&qsv_h264_args((1, 20), source_size));
        assert!(filter.contains("scale_qsv"), "{filter}");

        let filter = filter_complex(&qsv_h264_args((2, 9), source_size));
        assert!(!filter.contains("scale_qsv"), "{filter}");
    }

    #[test]
    fn legacy_qsv_copies_decoder_surfaces_after_anamorphic_stretch_without_resize() {
        let filter = filter_complex(&qsv_h264_args_with_geometry(
            (1, 20),
            FrameSize {
                width: 720,
                height: 480,
            },
            ScalingMode::Stretch,
            Some("32:27"),
        ));
        // A same-size vpp_qsv plus setsar only changes metadata, not the surfaces.
        assert!(filter.contains("scale_qsv"), "{filter}");
    }

    #[test]
    fn legacy_qsv_skips_copy_after_vpp() {
        let filter = filter_complex(&qsv_h264_args(
            (1, 20),
            FrameSize {
                width: 1280,
                height: 720,
            },
        ));
        assert!(filter.contains("vpp_qsv="), "{filter}");
        assert!(!filter.contains("scale_qsv"), "{filter}");
    }
}
