use std::fmt::{Display, Formatter};
use std::time::Duration;

use ersatztv_channel::error::ChannelError;
use ersatztv_playout::playout::PlayoutItem;
use time::OffsetDateTime;

use crate::channel_session::item_is_live;

#[derive(Debug)]
pub enum FallbackReason {
    /// no playout item covers the current time; the schedule resumes at `next_start`
    ScheduledGap { next_start: Option<OffsetDateTime> },

    /// no playout item could be selected (missing or invalid playout JSON, dynamic item failed, ...)
    ItemSelectionFailed(ChannelError),

    /// the playout item was selected but could not be transcoded
    TranscodeFailed {
        item_id: String,
        start: OffsetDateTime,
        finish: OffsetDateTime,
        /// a live item is retried after one fallback instead of waiting out its finish
        live: bool,
        error: ChannelError,
    },

    /// the playout item's source ended before its scheduled finish
    SourceEnded {
        item_id: String,
        reached: OffsetDateTime,
        finish: OffsetDateTime,
        /// a live item is retried after one fallback instead of waiting out its finish
        live: bool,
    },
}

impl FallbackReason {
    pub fn from_selection_error(error: ChannelError) -> FallbackReason {
        match error {
            ChannelError::PlayoutJsonNoItem { next_start } => {
                FallbackReason::ScheduledGap { next_start }
            }
            other => FallbackReason::ItemSelectionFailed(other),
        }
    }

    pub fn source_ended(item: &PlayoutItem, reached: OffsetDateTime) -> FallbackReason {
        FallbackReason::SourceEnded {
            item_id: item.id.clone(),
            reached,
            finish: item.finish,
            live: item_is_live(item),
        }
    }

    pub fn from_transcode_error(item: &PlayoutItem, error: ChannelError) -> FallbackReason {
        if let ChannelError::SourceEnded { reached, .. } = error {
            return FallbackReason::source_ended(item, reached);
        }
        FallbackReason::TranscodeFailed {
            item_id: item.id.clone(),
            start: item.start,
            finish: item.finish,
            live: item_is_live(item),
            error,
        }
    }

    /// when the substituted content should end, if known
    pub fn fallback_until(&self) -> Option<OffsetDateTime> {
        match self {
            FallbackReason::ScheduledGap { next_start } => *next_start,
            FallbackReason::ItemSelectionFailed(_) => None,
            FallbackReason::TranscodeFailed { finish, .. } => Some(*finish),
            FallbackReason::SourceEnded { finish, .. } => Some(*finish),
        }
    }

    /// when a fallback starting at `from` should end
    ///
    /// A live source that drops or fails to open is retried after one fallback of `duration`,
    /// capped at its item's finish. Waiting out the finish would keep a live channel dark for
    /// the rest of a long item even after its source came back. A continuous source can end
    /// past its item's finish, and then only the one fallback applies.
    pub fn fallback_finish(&self, from: OffsetDateTime, duration: Duration) -> OffsetDateTime {
        let after_one = from + duration;
        match self {
            FallbackReason::TranscodeFailed {
                live: true, finish, ..
            }
            | FallbackReason::SourceEnded {
                live: true, finish, ..
            } if *finish > from => std::cmp::min(*finish, after_one),
            FallbackReason::TranscodeFailed { live: true, .. }
            | FallbackReason::SourceEnded { live: true, .. } => after_one,
            _ => self.fallback_until().unwrap_or(after_one),
        }
    }

    /// recover the underlying error, for modes that refuse to substitute content
    pub fn into_error(self) -> ChannelError {
        match self {
            FallbackReason::ScheduledGap { next_start } => {
                ChannelError::PlayoutJsonNoItem { next_start }
            }
            FallbackReason::ItemSelectionFailed(error) => error,
            FallbackReason::TranscodeFailed { error, .. } => error,
            FallbackReason::SourceEnded {
                item_id, reached, ..
            } => ChannelError::SourceEnded { item_id, reached },
        }
    }

    pub fn log(&self, now: &OffsetDateTime) {
        match self {
            FallbackReason::ScheduledGap { next_start } => log::debug!(
                "no playout item covers {now}, replacing with fallback until {}",
                next_start.map_or_else(|| String::from("the next reload"), |s| s.to_string())
            ),
            FallbackReason::ItemSelectionFailed(error) => {
                log::error!("no item could be selected for {now}, replacing with fallback: {error}")
            }
            FallbackReason::TranscodeFailed {
                item_id,
                live: true,
                error,
                ..
            } => log::error!(
                "live item {item_id} failed, replacing with black/silence and then retrying: {error}"
            ),
            FallbackReason::TranscodeFailed {
                item_id,
                start,
                finish,
                error,
                ..
            } => log::error!(
                "item {item_id} ({start} .. {finish}) failed, replacing with fallback: {error}"
            ),
            FallbackReason::SourceEnded {
                item_id,
                reached,
                live: true,
                ..
            } => log::warn!(
                "live item {item_id} source ended at {reached}, replacing with black/silence and then retrying"
            ),
            FallbackReason::SourceEnded {
                item_id,
                reached,
                finish,
                ..
            } => log::warn!(
                "item {item_id} source ended at {reached}, replacing with fallback until {finish}"
            ),
        }
    }
}

impl Display for FallbackReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            FallbackReason::ScheduledGap {
                next_start: Some(next_start),
            } => {
                write!(f, "no playout item is scheduled until {next_start}")
            }
            FallbackReason::ScheduledGap { next_start: None } => {
                write!(f, "no playout item is scheduled")
            }
            FallbackReason::ItemSelectionFailed(error) => {
                write!(f, "unable to select playout item: {error}")
            }
            FallbackReason::TranscodeFailed { item_id, error, .. } => {
                write!(f, "playout item {item_id} failed to transcode: {error}")
            }
            FallbackReason::SourceEnded { item_id, .. } => {
                write!(f, "playout item {item_id} ended early")
            }
        }
    }
}

/// builds an ASS subtitle document that shows `message` centered near the bottom of the
/// frame for the whole fallback item
pub fn error_card_subtitle(message: &str, width: u32, height: u32) -> String {
    let font_size = (f64::from(height) / 20.0).round() as u32;
    let margin_v = (f64::from(height) * 0.05).round() as u32;
    let text = message.replace("\r\n", "\\N").replace('\n', "\\N");

    format!(
        "[Script Info]\n\
         ScriptType: v4.00+\n\
         WrapStyle: 0\n\
         ScaledBorderAndShadow: yes\n\
         YCbCr Matrix: None\n\
         PlayResX: {width}\n\
         PlayResY: {height}\n\
         \n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, OutlineColour, BorderStyle, Outline, Shadow, Alignment, Encoding\n\
         Style: Default,Roboto,{font_size},&HFFFFFF,,0,1,0,2,1\n\
         \n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
         Dialogue: 0,0:00:00.00,99:99:99.99,Default,,0,0,{margin_v},,{text}\n"
    )
}

#[cfg(test)]
mod tests {
    use ersatztv_playout::playout::PlayoutItemSource;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn gap_ends_at_next_start() {
        let next_start = datetime!(2026-01-01 12:00 UTC);
        let reason = FallbackReason::from_selection_error(ChannelError::PlayoutJsonNoItem {
            next_start: Some(next_start),
        });

        assert_eq!(reason.fallback_until(), Some(next_start));
        assert!(matches!(
            reason.into_error(),
            ChannelError::PlayoutJsonNoItem { next_start: Some(s) } if s == next_start
        ));
    }

    #[test]
    fn error_card_subtitle_scales_with_height_and_escapes_newlines() {
        let ass = error_card_subtitle("line one\nline two", 1920, 1080);

        assert!(ass.contains("PlayResX: 1920\nPlayResY: 1080\n"));
        assert!(ass.contains("Style: Default,Roboto,54,"));
        assert!(
            ass.contains(
                "Dialogue: 0,0:00:00.00,99:99:99.99,Default,,0,0,54,,line one\\Nline two\n"
            )
        );
    }

    #[test]
    fn source_ended_fills_rest_of_item() {
        let item = PlayoutItem {
            id: String::from("item"),
            start: datetime!(2026-01-01 12:00 UTC),
            finish: datetime!(2026-01-01 12:30 UTC),
            source: None,
            tracks: None,
            watermark: None,
            graphics: Vec::new(),
        };
        let reason = FallbackReason::from_transcode_error(
            &item,
            ChannelError::SourceEnded {
                item_id: item.id.clone(),
                reached: item.start,
            },
        );

        assert!(matches!(
            reason,
            FallbackReason::SourceEnded { reached, .. } if reached == item.start
        ));
        assert_eq!(reason.fallback_until(), Some(item.finish));
        assert_eq!(
            reason.fallback_finish(item.start, Duration::from_secs(60)),
            item.finish
        );
    }

    fn live_item() -> PlayoutItem {
        PlayoutItem {
            id: String::from("live"),
            start: datetime!(2026-01-01 12:00 UTC),
            finish: datetime!(2026-01-01 13:00 UTC),
            source: Some(PlayoutItemSource::Http {
                uri: String::from("https://example.com/live.m3u8"),
                is_live: Some(true),
                continuous: None,
                in_point_ms: None,
                out_point_ms: None,
                headers: None,
                user_agent: None,
                timeout_us: None,
                reconnect: None,
                reconnect_delay_max: None,
                keep_alive: None,
                probe_hint: None,
            }),
            tracks: None,
            watermark: None,
            graphics: Vec::new(),
        }
    }

    #[test]
    fn live_source_ended_retries_after_one_fallback() {
        let item = live_item();
        let reached = datetime!(2026-01-01 12:05 UTC);
        let reason = FallbackReason::from_transcode_error(
            &item,
            ChannelError::SourceEnded {
                item_id: item.id.clone(),
                reached,
            },
        );

        assert!(matches!(
            reason,
            FallbackReason::SourceEnded { live: true, .. }
        ));
        assert_eq!(
            reason.fallback_finish(reached, Duration::from_secs(60)),
            datetime!(2026-01-01 12:06 UTC)
        );
    }

    #[test]
    fn a_live_source_that_ended_past_its_item_gets_one_fallback() {
        let item = live_item();
        let reached = datetime!(2026-01-01 14:30 UTC);
        let reason = FallbackReason::source_ended(&item, reached);

        assert_eq!(
            reason.fallback_finish(reached, Duration::from_secs(60)),
            datetime!(2026-01-01 14:31 UTC)
        );
    }

    #[test]
    fn live_retry_never_runs_past_the_item() {
        let item = live_item();
        let reached = datetime!(2026-01-01 12:59:30 UTC);
        let reason = FallbackReason::source_ended(&item, reached);

        assert_eq!(
            reason.fallback_finish(reached, Duration::from_secs(60)),
            item.finish
        );
    }

    #[test]
    fn live_transcode_failure_retries_after_one_fallback() {
        let item = live_item();
        let reason = FallbackReason::from_transcode_error(
            &item,
            ChannelError::StreamFailure(String::from("503")),
        );

        assert!(matches!(
            reason,
            FallbackReason::TranscodeFailed { live: true, .. }
        ));
        assert_eq!(
            reason.fallback_finish(item.start, Duration::from_secs(60)),
            datetime!(2026-01-01 12:01 UTC)
        );
    }

    #[test]
    fn selection_error_has_unknown_end() {
        let reason = FallbackReason::from_selection_error(ChannelError::PlayoutJsonNoFileForTime(
            datetime!(2026-01-01 12:00 UTC),
        ));

        assert_eq!(reason.fallback_until(), None);
        assert!(matches!(
            reason.into_error(),
            ChannelError::PlayoutJsonNoFileForTime(_)
        ));
    }
}
