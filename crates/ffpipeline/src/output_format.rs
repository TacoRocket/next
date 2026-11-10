use crate::ArgVec;
use crate::frame_rate::FrameRate;
use crate::pipeline::{KEYFRAME_INTERVAL_SECONDS, OutputContext, SEGMENT_SECONDS};
use crate::video_codec::VideoEncoder;

pub(crate) fn gop_frames(frame_rate: &FrameRate) -> u32 {
    frame_rate.parsed_frame_rate.round_ties_even() as u32 * KEYFRAME_INTERVAL_SECONDS
}

#[derive(Debug, Clone)]
pub enum OutputFormat {
    Hls {
        playlist: String,
        segment_template: String,
        troubleshoot: bool,
    },
    /// HLS with fragmented MP4 segments. `init_filename` is a bare file name: ffmpeg writes it
    /// next to the playlist, and each job needs its own so earlier segments keep theirs.
    HlsFmp4 {
        playlist: String,
        segment_template: String,
        init_filename: String,
        troubleshoot: bool,
    },
}

impl OutputFormat {
    pub fn is_fmp4(&self) -> bool {
        matches!(self, OutputFormat::HlsFmp4 { .. })
    }

    pub(crate) fn as_arg(&self, output_context: &OutputContext) -> ArgVec {
        let force_key_frames_expr = format!("expr:gte(t,n_forced*{KEYFRAME_INTERVAL_SECONDS})");
        let segment_seconds = format!("{SEGMENT_SECONDS}");
        // TODO: 1-second GOP for qsv
        let gop = format!("{}", gop_frames(&output_context.frame_rate));
        let keyint_min = gop.clone();

        let mut args: ArgVec = Vec::new();

        let (playlist, segment_template, troubleshoot, init_filename) = match self {
            OutputFormat::Hls {
                playlist,
                segment_template,
                troubleshoot,
            } => (playlist, segment_template, troubleshoot, None),
            OutputFormat::HlsFmp4 {
                playlist,
                segment_template,
                init_filename,
                troubleshoot,
            } => (
                playlist,
                segment_template,
                troubleshoot,
                Some(init_filename),
            ),
        };

        if output_context.video_encoder != VideoEncoder::Copy {
            args.extend(args![
                "-g",
                gop,
                "-keyint_min",
                keyint_min,
                "-force_key_frames",
                force_key_frames_expr,
            ]);
        }

        args.extend(args![
            "-f",
            "hls",
            "-hls_time",
            segment_seconds,
            "-hls_list_size",
            "0",
            "-segment_list_flags",
            "+live",
            "-hls_segment_filename",
            segment_template.to_owned(),
            "-hls_segment_type",
        ]);
        match init_filename {
            None => args.extend(args!["mpegts"]),
            Some(init_filename) => args.extend(args![
                "fmp4",
                "-hls_fmp4_init_filename",
                init_filename.to_owned(),
            ]),
        }

        if *troubleshoot {
            args.extend(args!["-hls_flags", "append_list+independent_segments"]);
        } else {
            args.extend(args![
                "-hls_flags",
                "program_date_time+omit_endlist+append_list+independent_segments"
            ]);
        }

        if init_filename.is_none() {
            // Each FFmpeg job restarts MPEG-TS continuity counters even when
            // timestamps continue. Signal the reset to transport readers too.
            args.extend(args![
                "-hls_segment_options",
                "mpegts_flags=+initial_discontinuity",
            ]);
        }

        args.extend(args![playlist.to_owned()]);

        args
    }
}
