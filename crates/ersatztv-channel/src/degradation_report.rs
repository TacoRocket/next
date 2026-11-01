use std::collections::VecDeque;
use std::path::PathBuf;

use ersatztv_playout::playout::PlayoutItem;
use ffpipeline::degradation::{DegradationCode, PipelineDegradation};
use serde::Serialize;
use time::OffsetDateTime;

/// Items kept in the report. The channel transcodes a little ahead of what viewers see, so the
/// file has to reach back past the newest item; a few cover any realistic lead.
const RETAINED_ITEMS: usize = 8;

/// Writes `ffmpeg.degradation_report`: where each recent item runs other than as configured.
///
/// The file is replaced whole as each item starts, so a reader never sees a partial write.
/// Writing is best effort; a failure is logged once and never stops playback.
pub struct DegradationReport {
    path: PathBuf,
    items: VecDeque<ReportItem>,
    write_failed: bool,
}

#[derive(Serialize)]
struct ReportFile<'a> {
    items: &'a VecDeque<ReportItem>,
}

#[derive(Clone, Serialize)]
struct ReportItem {
    id: String,
    #[serde(with = "time::serde::rfc3339")]
    start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    finish: OffsetDateTime,
    degradations: Vec<ReportedDegradation>,
}

#[derive(Clone, Serialize)]
struct ReportedDegradation {
    code: DegradationCode,
    message: String,
    /// When this degradation began, carried across consecutive items that share it
    #[serde(with = "time::serde::rfc3339")]
    since: OffsetDateTime,
}

impl DegradationReport {
    pub fn new(path: PathBuf) -> Self {
        DegradationReport {
            path,
            items: VecDeque::new(),
            write_failed: false,
        }
    }

    pub async fn record(
        &mut self,
        item: &PlayoutItem,
        degradations: &[PipelineDegradation],
        now: OffsetDateTime,
    ) {
        self.push(item, degradations, now);
        self.write().await;
    }

    fn push(
        &mut self,
        item: &PlayoutItem,
        degradations: &[PipelineDegradation],
        now: OffsetDateTime,
    ) {
        // a rebuilt item (a retry, or a restart mid-item) replaces its entry and keeps its times
        let previous = match self.items.back() {
            Some(last) if last.id == item.id => self.items.pop_back(),
            last => last.cloned(),
        };
        let since = |code: DegradationCode| {
            previous
                .as_ref()
                .and_then(|p| p.degradations.iter().find(|d| d.code == code))
                .map_or(now, |d| d.since)
        };

        self.items.push_back(ReportItem {
            id: item.id.clone(),
            start: item.start,
            finish: item.finish,
            degradations: degradations
                .iter()
                .map(|d| ReportedDegradation {
                    code: d.code,
                    message: d.message.clone(),
                    since: since(d.code),
                })
                .collect(),
        });
        while self.items.len() > RETAINED_ITEMS {
            self.items.pop_front();
        }
    }

    async fn write(&mut self) {
        let result = async {
            let json = serde_json::to_vec(&ReportFile { items: &self.items })?;
            let mut temp = self.path.clone().into_os_string();
            temp.push(".tmp");
            tokio::fs::write(&temp, json).await?;
            tokio::fs::rename(&temp, &self.path).await?;
            Ok::<(), std::io::Error>(())
        }
        .await;

        match result {
            Ok(()) => self.write_failed = false,
            Err(err) if !self.write_failed => {
                log::warn!(
                    "failed to write the degradation report {}: {err}",
                    self.path.display()
                );
                self.write_failed = true;
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn item(id: &str, start: OffsetDateTime, finish: OffsetDateTime) -> PlayoutItem {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "start": start.format(&time::format_description::well_known::Rfc3339).unwrap(),
            "finish": finish.format(&time::format_description::well_known::Rfc3339).unwrap(),
        }))
        .unwrap()
    }

    fn degradation(code: DegradationCode) -> PipelineDegradation {
        PipelineDegradation {
            code,
            message: format!("{code:?}"),
        }
    }

    const T0: OffsetDateTime = datetime!(2026-11-01 12:00 UTC);
    const T1: OffsetDateTime = datetime!(2026-11-01 12:30 UTC);
    const T2: OffsetDateTime = datetime!(2026-11-01 13:00 UTC);
    const T3: OffsetDateTime = datetime!(2026-11-01 13:30 UTC);

    fn report() -> DegradationReport {
        DegradationReport::new(PathBuf::from("unused"))
    }

    #[test]
    fn a_degradation_keeps_its_start_across_consecutive_items() {
        let mut report = report();
        let decode = degradation(DegradationCode::HardwareDecodeUnavailable);
        report.push(&item("a", T0, T1), std::slice::from_ref(&decode), T0);
        report.push(&item("b", T1, T2), std::slice::from_ref(&decode), T1);

        assert_eq!(report.items[1].degradations[0].since, T0);
    }

    #[test]
    fn an_item_without_it_resets_the_start() {
        let mut report = report();
        let decode = degradation(DegradationCode::HardwareDecodeUnavailable);
        report.push(&item("a", T0, T1), std::slice::from_ref(&decode), T0);
        report.push(&item("b", T1, T2), &[], T1);
        report.push(&item("c", T2, T3), std::slice::from_ref(&decode), T2);

        assert_eq!(report.items[2].degradations[0].since, T2);
    }

    #[test]
    fn a_rebuilt_item_replaces_its_entry_and_keeps_its_start() {
        let mut report = report();
        let encode = degradation(DegradationCode::HardwareEncodeUnavailable);
        report.push(&item("a", T0, T1), std::slice::from_ref(&encode), T0);
        report.push(&item("a", T0, T1), std::slice::from_ref(&encode), T1);

        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].degradations[0].since, T0);
    }

    #[test]
    fn only_the_last_few_items_are_kept() {
        let mut report = report();
        for i in 0..RETAINED_ITEMS + 3 {
            report.push(&item(&format!("item-{i}"), T0, T1), &[], T0);
        }

        assert_eq!(report.items.len(), RETAINED_ITEMS);
        assert_eq!(report.items[0].id, "item-3");
    }

    #[tokio::test]
    async fn the_file_lists_items_with_codes_and_rfc3339_times() {
        let dir = std::env::temp_dir().join(format!("etv-degradation-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("degradations.json");
        let mut report = DegradationReport::new(path.clone());
        report
            .record(
                &item("a", T0, T1),
                &[degradation(DegradationCode::CustomShaderHdrSkipped)],
                T0,
            )
            .await;

        let written: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        assert_eq!(
            written,
            serde_json::json!({
                "items": [{
                    "id": "a",
                    "start": "2026-11-01T12:00:00Z",
                    "finish": "2026-11-01T12:30:00Z",
                    "degradations": [{
                        "code": "custom_shader_hdr_skipped",
                        "message": "CustomShaderHdrSkipped",
                        "since": "2026-11-01T12:00:00Z",
                    }],
                }],
            })
        );
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
