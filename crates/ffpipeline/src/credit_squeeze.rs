use std::time::Duration;

use crate::accel::vaapi::LibplaceboShaderVaapi;
use crate::color::FrameColor;
use crate::ffmpeg_info::FfmpegInfo;
use crate::filter_chain::FilterChain;
use crate::frame_size::FrameSize;
use crate::output_settings::CustomShader;
use crate::pipeline::{FrameState, FrameSurface, PixelFormat};
use crate::video_filter::{VideoFilter, VideoFilterOp};

/// Timestamp-preserving split/composite/join, adapted from Tunarr's CreditSqueezeFilter.
/// The program is the frame clock and EOF owner; the looping still cannot extend it.
#[derive(Debug, Clone)]
pub struct CreditSqueeze {
    pub(crate) background_index: usize,
    pub(crate) start: Duration,
    pub(crate) size: FrameSize,
    pub(crate) shader: Option<CustomShader>,
    pub(crate) native_nv12: bool,
    pub(crate) software_output: bool,
}

#[derive(Debug, Clone)]
pub struct CreditSqueezeBranches {
    pub(crate) start: Duration,
    pub(crate) normal: FilterChain,
    pub(crate) credits: FilterChain,
    pub(crate) size: FrameSize,
}

impl CreditSqueezeBranches {
    pub(crate) fn apply_to(&self, state: &mut FrameState) {
        state.size = self.size;
        state.set_square_pixels();
        state.surface = FrameSurface::Vaapi;
        state.pixel_format = PixelFormat::Nv12;
        state.decoder_frames = false;
        state.color = FrameColor::bt709();
    }
}

impl CreditSqueeze {
    pub(crate) fn apply_to(&self, state: &mut FrameState) {
        state.size = self.size;
        state.set_square_pixels();
        state.surface = if self.software_output {
            FrameSurface::System
        } else {
            FrameSurface::Vaapi
        };
        state.pixel_format = PixelFormat::Nv12;
        state.decoder_frames = false;
        state.color = FrameColor::bt709();
    }

    fn effect(&self) -> String {
        self.shader.as_ref().map_or_else(
            || String::from("setsar=1"),
            |shader| {
                LibplaceboShaderVaapi {
                    size: self.size,
                    shader: shader.clone(),
                    native_nv12: self.native_nv12,
                    nv12_handoff: false,
                    hdr: false,
                }
                .as_arg()
                .unwrap_or_else(|| String::from("setsar=1"))
            },
        )
    }

    pub(crate) fn graph(&self, video: &str, background: &str, out: &str) -> String {
        let working =
            self.shader
                .as_ref()
                .and_then(|s| s.input_height())
                .map_or(self.size, |height| FrameSize {
                    width: ((f64::from(height) * f64::from(self.size.width)
                        / f64::from(self.size.height)
                        / 2.0)
                        .round() as u32
                        * 2)
                    .max(2),
                    height,
                });
        let start = self.start.as_secs_f64();
        // A zero start means the viewer joined during credits; do not replay the entrance.
        let (x, y, size) = if self.start.is_zero() {
            ("0.05".to_owned(), "0.225".to_owned(), "0.55".to_owned())
        } else {
            let progress = format!("(0.5-0.5*cos(PI*clip(t-{start},0,1)))");
            (
                format!("0.05*{progress}"),
                format!("0.225*{progress}"),
                format!("(1-0.45*{progress})"),
            )
        };
        let mode = self
            .shader
            .as_ref()
            .and_then(|s| s.options().input_scale_mode.as_deref())
            .map_or(String::new(), |m| format!(":mode={m}"));
        let to_vaapi = "scale_vulkan=format=nv12:out_range=limited,hwmap=derive_device=vaapi:reverse=1,format=vaapi";
        let effect = self.effect();
        // Each branch owns a frame pool; download before joining when the next overlay is software.
        let download = if self.software_output {
            ",hwdownload,format=nv12"
        } else {
            ""
        };
        let mut graph = Vec::new();
        let after = video;
        graph.push(format!("[{after}]scale_vaapi=w={}:h={}:format=nv12{mode},hwmap=derive_device=drm,format=drm_prime,libplacebo=format=rgba,format=vulkan,split=2[cs_clock][cs_program]", working.width, working.height));
        graph.push(format!("[cs_clock]{to_vaapi}[cs_clock_va]"));
        graph.push(format!("[{background}]format=rgba,hwupload,loop=loop=-1:size=1:start=0,setpts=PTS+{start}/TB[cs_background]"));
        graph.push(format!("[cs_background][cs_program]libplacebo=inputs=2:w={}:h={}:format=rgba:colorspace=bt709:range=tv:color_primaries=bt709:color_trc=bt709:pos_x='if(eq(idx,1),ow*{x},0)':pos_y='if(eq(idx,1),oh*{y},0)':pos_w='if(eq(idx,1),ow*{size},ow)':pos_h='if(eq(idx,1),oh*{size},oh)',format=vulkan,{to_vaapi}[cs_composed]", working.width, working.height));
        let credits_out = out;
        graph.push(format!("[cs_clock_va][cs_composed]overlay_vaapi=eof_action=pass:repeatlast=0,{effect},setparams=colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv{download}[{credits_out}]"));
        graph.join(";")
    }
}

/// The join has one colour contract. Convert first; the declaration prevents concat
/// from trying to insert a software colour converter on VAAPI surfaces.
#[derive(Debug, Clone)]
pub struct CreditSqueezeColor;

impl VideoFilterOp for CreditSqueezeColor {
    fn evaluate(&self, _state: &FrameState, _info: &FfmpegInfo) -> Option<VideoFilter> {
        Some(self.clone().into())
    }
    fn apply_to(&self, state: &mut FrameState) {
        state.color = FrameColor::bt709();
        state.pixel_format = PixelFormat::Nv12;
        state.surface = FrameSurface::Vaapi;
    }
    fn required_surface(&self) -> Option<FrameSurface> {
        Some(FrameSurface::Vaapi)
    }
    fn as_arg(&self) -> Option<String> {
        Some(String::from(
            "scale_vaapi=format=nv12:out_color_matrix=bt709:out_color_primaries=bt709:out_color_transfer=bt709:out_range=tv,setparams=colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv",
        ))
    }
}
