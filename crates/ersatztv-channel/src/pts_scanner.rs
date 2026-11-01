use std::path::{Path, PathBuf};
use std::time::Duration;

use ersatztv_channel::config::ChannelConfig;
use ersatztv_channel::error::{ChannelError, IoContext};
use ersatztv_core::process;

#[derive(Clone, Copy, Default)]
pub struct PtsTime {
    pub duration: Duration,
    pub audio_end: Option<Duration>,
}

pub struct PtsScanner {
    output_folder: PathBuf,
    ffprobe_path: PathBuf,
}

impl PtsScanner {
    pub fn new(channel_config: &ChannelConfig, ffprobe_path: &Path) -> PtsScanner {
        PtsScanner {
            output_folder: channel_config.expanded_output_folder().to_owned(),
            ffprobe_path: ffprobe_path.to_owned(),
        }
    }

    pub async fn get_last_pts(&self) -> Result<PtsTime, ChannelError> {
        let mut pts_time = PtsTime::default();

        // find last segment file in output folder
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(&self.output_folder).await.io_context(
            "scan the output folder for the last segment",
            &self.output_folder,
        )?;
        while let Ok(Some(entry)) = dir.next_entry().await {
            if entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ts"))
            {
                entries.push(entry);
            }
        }
        entries.sort_by_key(|a| std::cmp::Reverse(a.file_name()));
        if let Some(last_segment) = entries.first() {
            // call ffprobe
            let path = last_segment
                .path()
                .into_os_string()
                .into_string()
                .map_err(|p| {
                    ChannelError::PtsScannerPathNotUtf8(p.to_string_lossy().into_owned())
                })?;

            let output = process::command(&self.ffprobe_path)
                .args([
                    "-v",
                    "-0",
                    "-show_entries",
                    "packet=codec_type,pts_time,duration_time",
                    "-of",
                    "compact=p=0",
                    &path,
                ])
                .output()
                .await
                .io_context("run ffprobe on the last segment", &path)?;

            pts_time = packet_ends(&String::from_utf8_lossy(&output.stdout));
        }

        Ok(pts_time)
    }
}

fn packet_ends(output: &str) -> PtsTime {
    let mut ends = PtsTime::default();
    for line in output.lines() {
        let mut kind = None;
        let mut pts = None;
        let mut duration = 0.0;
        for field in line.split('|') {
            if let Some(value) = field.strip_prefix("codec_type=") {
                kind = Some(value);
            } else if let Some(value) = field.strip_prefix("pts_time=") {
                pts = value.parse::<f64>().ok();
            } else if let Some(value) = field.strip_prefix("duration_time=") {
                duration = value.parse::<f64>().unwrap_or_default();
            }
        }
        let Some(seconds) = pts.map(|pts| pts + duration) else {
            continue;
        };
        if !seconds.is_finite() || seconds < 0.0 {
            continue;
        }
        let end = Duration::from_secs_f64(seconds);
        match kind {
            Some("video") => ends.duration = ends.duration.max(end),
            Some("audio") => ends.audio_end = Some(ends.audio_end.unwrap_or_default().max(end)),
            _ => {}
        }
    }
    ends
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_tail_does_not_move_the_video_anchor() {
        let ends = packet_ends(
            "codec_type=video|pts_time=8.008000|duration_time=0.033367\n\
             codec_type=audio|pts_time=8.045400|duration_time=0.021333\n",
        );
        assert_eq!(ends.duration, Duration::from_micros(8_041_367));
        assert_eq!(ends.audio_end, Some(Duration::from_micros(8_066_733)));
    }
}
