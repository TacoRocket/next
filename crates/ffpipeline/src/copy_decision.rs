use std::fmt;

use crate::frame_rate::FrameRate;
use crate::input::{InputSource, ProbedInput};
use crate::output_settings::CopyPolicy;
use crate::pipeline::VideoFormat;
use crate::probe::{ProbeResultAudioStream, ProbeResultVideoStream};

#[derive(Debug, Clone, PartialEq)]
pub enum CopyDecision {
    Copy,
    Transcode(Vec<CopyBlocker>),
}

impl CopyDecision {
    fn from_blockers(blockers: Vec<CopyBlocker>) -> Self {
        if blockers.is_empty() {
            CopyDecision::Copy
        } else {
            CopyDecision::Transcode(blockers)
        }
    }

    pub fn is_copy(&self) -> bool {
        matches!(self, CopyDecision::Copy)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CopyBlocker {
    CodecNotAllowed(String),
    /// Most hardware players decode H.264 only as 8-bit 4:2:0, and HEVC only as 4:2:0.
    PixelFormat(String),
    StillImage,
    GraphicsLayers,
    ImageSubtitle,
    BurnedSubtitle,
    DolbyVision5,
    GeneratedSource,
    /// AVI packets have no pts, so copied timing is wrong; AAC copy from AVI to mpegts fails
    ContainerWithoutPts,
    /// Copy drops video until a keyframe.
    NoKeyframes,
    /// Copy would drop video until the first keyframe and shorten the timeline. A transcode
    /// holds the first picture.
    StartsBetweenKeyframes,
    /// Copy can't change the frame rate.
    FrameRateMismatch {
        source: String,
        target: String,
    },
}

impl fmt::Display for CopyBlocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CopyBlocker::CodecNotAllowed(codec) => write!(f, "{codec} is not in copy_formats"),
            CopyBlocker::PixelFormat(pix_fmt) => write!(f, "pixel format {pix_fmt}"),
            CopyBlocker::StillImage => f.write_str("still image"),
            CopyBlocker::GraphicsLayers => f.write_str("graphics layers"),
            CopyBlocker::ImageSubtitle => f.write_str("image subtitles"),
            CopyBlocker::BurnedSubtitle => f.write_str("burned-in text subtitles"),
            CopyBlocker::DolbyVision5 => f.write_str("dolby vision profile 5"),
            CopyBlocker::GeneratedSource => f.write_str("generated (lavfi) source"),
            CopyBlocker::ContainerWithoutPts => f.write_str("avi container"),
            CopyBlocker::NoKeyframes => f.write_str("no keyframes where playback starts"),
            CopyBlocker::StartsBetweenKeyframes => f.write_str("starts between keyframes"),
            CopyBlocker::FrameRateMismatch { source, target } => {
                write!(f, "frame rate {source} is not {target}")
            }
        }
    }
}

/// `None`: the stream has no copy policy and always transcodes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CopyDecisions {
    pub video: Option<CopyDecision>,
    pub audio: Option<CopyDecision>,
}

impl CopyDecisions {
    pub fn transcode_summary(&self) -> Option<String> {
        let streams: Vec<String> = [("video", &self.video), ("audio", &self.audio)]
            .into_iter()
            .filter_map(|(stream, decision)| match decision {
                Some(CopyDecision::Transcode(blockers)) => Some(format!(
                    "{stream} ({})",
                    blockers
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                _ => None,
            })
            .collect();

        (!streams.is_empty()).then(|| streams.join("; "))
    }
}

#[derive(Debug, Default)]
pub(crate) struct VideoCopyContext<'a> {
    pub(crate) caller_blockers: &'a [CopyBlocker],
    pub(crate) is_still_image: bool,
    pub(crate) has_graphics: bool,
    pub(crate) image_subtitle: bool,
    pub(crate) burned_subtitle: bool,
    pub(crate) target_frame_rate: Option<&'a FrameRate>,
}

pub(crate) fn video_copy_decision(
    policy: &CopyPolicy<VideoFormat>,
    input: &ProbedInput,
    stream: &ProbeResultVideoStream,
    context: &VideoCopyContext,
) -> CopyDecision {
    let mut blockers = source_blockers(input);
    blockers.extend_from_slice(context.caller_blockers);

    let allowed = stream
        .codec
        .parse::<VideoFormat>()
        .is_ok_and(|format| policy.formats.contains(&format));
    if !allowed {
        blockers.push(CopyBlocker::CodecNotAllowed(stream.codec.clone()));
    } else if !copyable_pixel_format(&stream.codec, &stream.pix_fmt) {
        blockers.push(CopyBlocker::PixelFormat(stream.pix_fmt.clone()));
    }

    for (blocked, blocker) in [
        (context.is_still_image, CopyBlocker::StillImage),
        (stream.dv_profile == Some(5), CopyBlocker::DolbyVision5),
        (context.has_graphics, CopyBlocker::GraphicsLayers),
        (context.image_subtitle, CopyBlocker::ImageSubtitle),
        (context.burned_subtitle, CopyBlocker::BurnedSubtitle),
    ] {
        if blocked {
            blockers.push(blocker);
        }
    }

    if let Some(target) = context.target_frame_rate
        && !stream.frame_rate.same_rate(target)
    {
        blockers.push(CopyBlocker::FrameRateMismatch {
            source: stream.frame_rate.r_frame_rate.clone(),
            target: target.r_frame_rate.clone(),
        });
    }

    CopyDecision::from_blockers(blockers)
}

/// The codec name alone does not say a player can decode it: 10-bit H.264 (High 10) and
/// 4:2:2 or 4:4:4 video are still `h264`/`hevc`. Matches ffprobe's pixel format names, since
/// `PixelFormat::parse` reads formats it does not know as 8-bit. An unprobed format copies.
fn copyable_pixel_format(codec: &str, pix_fmt: &str) -> bool {
    match codec {
        "h264" => matches!(pix_fmt, "" | "yuv420p" | "yuvj420p"),
        "hevc" => matches!(pix_fmt, "" | "yuv420p" | "yuvj420p" | "yuv420p10le"),
        _ => true,
    }
}

pub(crate) fn audio_copy_decision(
    policy: &CopyPolicy<String>,
    input: &ProbedInput,
    stream: &ProbeResultAudioStream,
) -> CopyDecision {
    let mut blockers = source_blockers(input);

    if !policy.formats.contains(&stream.codec) {
        blockers.push(CopyBlocker::CodecNotAllowed(stream.codec.clone()));
    }

    CopyDecision::from_blockers(blockers)
}

fn source_blockers(input: &ProbedInput) -> Vec<CopyBlocker> {
    let mut blockers = Vec::new();
    if matches!(input.input_source, InputSource::Lavfi(_)) {
        blockers.push(CopyBlocker::GeneratedSource);
    }
    if input.probe_result.format_name.as_deref() == Some("avi") {
        blockers.push(CopyBlocker::ContainerWithoutPts);
    }
    blockers
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::input::{LavfiInputSource, LocalInputSource};
    use crate::probe::{CodecType, ProbeResult};

    fn input(source: InputSource, format_name: &str) -> ProbedInput {
        ProbedInput {
            input_source: source,
            probe_result: ProbeResult {
                path: String::from("source"),
                streams: Vec::new(),
                duration: Some(Duration::from_secs(60)),
                format_name: Some(format_name.to_owned()),
            },
            in_point: Duration::ZERO,
            out_point: Duration::from_secs(60),
            stream_index: None,
        }
    }

    fn local(format_name: &str) -> ProbedInput {
        input(
            InputSource::Local(LocalInputSource {
                path: String::from("source"),
            }),
            format_name,
        )
    }

    fn lavfi() -> ProbedInput {
        input(
            InputSource::Lavfi(LavfiInputSource {
                params: String::from("anullsrc"),
            }),
            "mpegts",
        )
    }

    fn video(codec: &str) -> ProbeResultVideoStream {
        ProbeResultVideoStream {
            stream_index: 0,
            codec: codec.to_owned(),
            codec_type: CodecType::Video,
            dv_profile: None,
            profile: String::new(),
            height: Some(1080),
            width: Some(1920),
            frame_rate: FrameRate::parse("30000/1001"),
            sample_aspect_ratio: None,
            display_aspect_ratio: None,
            pix_fmt: String::from("yuv420p"),
            color_params: Default::default(),
            field_order: None,
            rotation: None,
        }
    }

    fn audio(codec: &str) -> ProbeResultAudioStream {
        ProbeResultAudioStream {
            stream_index: 1,
            codec: codec.to_owned(),
            channels: 2,
        }
    }

    fn decide_video(input: &ProbedInput, stream: &ProbeResultVideoStream) -> CopyDecision {
        video_copy_decision(
            &CopyPolicy::default(),
            input,
            stream,
            &VideoCopyContext::default(),
        )
    }

    #[test]
    fn allowed_codecs_copy() {
        for codec in ["h264", "hevc"] {
            assert!(decide_video(&local("matroska"), &video(codec)).is_copy());
        }
        for codec in ["aac", "ac3", "eac3", "mp3"] {
            let decision =
                audio_copy_decision(&CopyPolicy::default(), &local("mpegts"), &audio(codec));
            assert!(decision.is_copy(), "{codec}");
        }
    }

    #[test]
    fn codec_outside_policy_transcodes() {
        for codec in ["mpeg2video", "vc1", "prores"] {
            assert_eq!(
                decide_video(&local("matroska"), &video(codec)),
                CopyDecision::Transcode(vec![CopyBlocker::CodecNotAllowed(codec.to_owned())])
            );
        }
        assert_eq!(
            audio_copy_decision(&CopyPolicy::default(), &local("matroska"), &audio("flac")),
            CopyDecision::Transcode(vec![CopyBlocker::CodecNotAllowed(String::from("flac"))])
        );
    }

    #[test]
    fn pixel_formats_players_cannot_decode_transcode() {
        let decide = |codec: &str, pix_fmt: &str| {
            decide_video(
                &local("matroska"),
                &ProbeResultVideoStream {
                    pix_fmt: pix_fmt.to_owned(),
                    ..video(codec)
                },
            )
        };

        for (codec, pix_fmt) in [
            ("h264", "yuv420p"),
            ("h264", "yuvj420p"),
            ("h264", ""),
            ("hevc", "yuv420p"),
            ("hevc", "yuv420p10le"),
            ("hevc", ""),
        ] {
            assert!(decide(codec, pix_fmt).is_copy(), "{codec} {pix_fmt}");
        }
        for (codec, pix_fmt) in [
            ("h264", "yuv420p10le"),
            ("h264", "yuv422p"),
            ("h264", "yuv422p10le"),
            ("h264", "yuv444p"),
            ("hevc", "yuv422p10le"),
            ("hevc", "yuv444p"),
        ] {
            assert_eq!(
                decide(codec, pix_fmt),
                CopyDecision::Transcode(vec![CopyBlocker::PixelFormat(pix_fmt.to_owned())]),
                "{codec} {pix_fmt}"
            );
        }
        assert_eq!(
            CopyDecisions {
                video: Some(decide("h264", "yuv420p10le")),
                audio: None,
            }
            .transcode_summary()
            .as_deref(),
            Some("video (pixel format yuv420p10le)")
        );
    }

    #[test]
    fn codec_outside_policy_does_not_report_pixel_format() {
        assert_eq!(
            decide_video(
                &local("matroska"),
                &ProbeResultVideoStream {
                    pix_fmt: String::from("yuv422p10le"),
                    ..video("prores")
                },
            ),
            CopyDecision::Transcode(vec![CopyBlocker::CodecNotAllowed(String::from("prores"))])
        );
    }

    #[test]
    fn mpeg2video_copies_when_listed() {
        let policy = CopyPolicy {
            formats: vec![VideoFormat::Mpeg2Video],
        };
        let decision = video_copy_decision(
            &policy,
            &local("mpegts"),
            &video("mpeg2video"),
            &VideoCopyContext::default(),
        );
        assert!(decision.is_copy());
    }

    #[test]
    fn generated_sources_transcode() {
        assert_eq!(
            decide_video(&lavfi(), &video("rawvideo")),
            CopyDecision::Transcode(vec![
                CopyBlocker::GeneratedSource,
                CopyBlocker::CodecNotAllowed(String::from("rawvideo")),
            ])
        );
        assert_eq!(
            audio_copy_decision(&CopyPolicy::default(), &lavfi(), &audio("pcm_s16le")),
            CopyDecision::Transcode(vec![
                CopyBlocker::GeneratedSource,
                CopyBlocker::CodecNotAllowed(String::from("pcm_s16le")),
            ])
        );
    }

    #[test]
    fn avi_transcodes_both_streams() {
        assert_eq!(
            decide_video(&local("avi"), &video("h264")),
            CopyDecision::Transcode(vec![CopyBlocker::ContainerWithoutPts])
        );
        assert_eq!(
            audio_copy_decision(&CopyPolicy::default(), &local("avi"), &audio("aac")),
            CopyDecision::Transcode(vec![CopyBlocker::ContainerWithoutPts])
        );
    }

    #[test]
    fn video_filters_and_dv5_transcode() {
        let stream = ProbeResultVideoStream {
            dv_profile: Some(5),
            ..video("hevc")
        };
        let context = VideoCopyContext {
            caller_blockers: &[CopyBlocker::NoKeyframes],
            is_still_image: true,
            has_graphics: true,
            image_subtitle: true,
            burned_subtitle: true,
            target_frame_rate: None,
        };
        assert_eq!(
            video_copy_decision(
                &CopyPolicy::default(),
                &local("matroska"),
                &stream,
                &context
            ),
            CopyDecision::Transcode(vec![
                CopyBlocker::NoKeyframes,
                CopyBlocker::StillImage,
                CopyBlocker::DolbyVision5,
                CopyBlocker::GraphicsLayers,
                CopyBlocker::ImageSubtitle,
                CopyBlocker::BurnedSubtitle,
            ])
        );
    }

    #[test]
    fn target_frame_rate_copies_only_matching_rates() {
        let decide = |source: &str, target: &str| {
            let target = FrameRate::parse_target(target).unwrap();
            video_copy_decision(
                &CopyPolicy::default(),
                &local("matroska"),
                &ProbeResultVideoStream {
                    frame_rate: FrameRate::parse(source),
                    ..video("h264")
                },
                &VideoCopyContext {
                    target_frame_rate: Some(&target),
                    ..Default::default()
                },
            )
        };

        assert!(decide("30000/1001", "30000/1001").is_copy());
        assert!(decide("25/1", "25").is_copy());
        let decision = decide("30/1", "30000/1001");
        assert_eq!(
            decision,
            CopyDecision::Transcode(vec![CopyBlocker::FrameRateMismatch {
                source: String::from("30/1"),
                target: String::from("30000/1001"),
            }])
        );
        assert_eq!(
            CopyDecisions {
                video: Some(decision),
                audio: None,
            }
            .transcode_summary()
            .as_deref(),
            Some("video (frame rate 30/1 is not 30000/1001)")
        );
    }

    #[test]
    fn summary_names_only_transcoded_streams() {
        assert_eq!(CopyDecisions::default().transcode_summary(), None);
        assert_eq!(
            CopyDecisions {
                video: Some(CopyDecision::Copy),
                audio: None,
            }
            .transcode_summary(),
            None
        );
        assert_eq!(
            CopyDecisions {
                video: Some(CopyDecision::Transcode(vec![
                    CopyBlocker::GraphicsLayers,
                    CopyBlocker::CodecNotAllowed(String::from("vc1")),
                ])),
                audio: Some(CopyDecision::Transcode(vec![
                    CopyBlocker::ContainerWithoutPts
                ])),
            }
            .transcode_summary()
            .as_deref(),
            Some("video (graphics layers, vc1 is not in copy_formats); audio (avi container)")
        );
    }
}
