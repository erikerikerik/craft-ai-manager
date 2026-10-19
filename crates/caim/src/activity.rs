//! The activity log: one JSON line per finished job, in `activity.jsonl` next to
//! `endpoint.json` (SPEC v1.1 amendment 18, "Activity log").
//!
//! The line carries amendment 18's list - task, model, version, device, requested device, fallback
//! reason, timings, state - and every one of them comes from the finished job's own provenance.
//! `null` means there was nothing to report, which is the case for a job cancelled before it
//! reached a backend and for a model version the backend does not have. Only a finished job is
//! written: `done`, `failed` or `cancelled`, once each.
//!
//! The log is a convenience, never a dependency: a line that cannot be written is a sentence on
//! stderr, and the job that finished still reports what it reported.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::endpoint;
use crate::error::{Fail, Result};

/// The file, in the same folder as `endpoint.json`.
pub const ACTIVITY_NAME: &str = "activity.jsonl";
/// Amendment 18: the newest 1,000 lines are kept.
pub const MAX_LINES: usize = 1_000;
/// ... and the file is only rewritten once it has grown past this, so the usual append is one write.
pub const TRIM_AT: usize = 1_100;
/// A line longer than this is not something CAIM wrote, so it is dropped rather than kept.
const MAX_LINE_BYTES: usize = 8 * 1024;
/// The most of the file that is ever read back at once when trimming.
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

static WRITE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// One finished job at a time. Connection threads finish different jobs at the same moment, and
/// appending a line and trimming the file has to be one transaction: two writers could otherwise
/// interleave their writes into one invalid line, and two trims could each rewrite from their own
/// snapshot and drop a record the other had just added. (Writers in another process are not
/// serialized: one manager owns this file.)
static LOG_LOCK: Mutex<()> = Mutex::new(());

/// One finished job, as one line of the log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// When the job finished, as `YYYY-MM-DDThh:mm:ssZ`.
    pub time: String,
    /// The task the job asked for.
    pub task: Option<String>,
    /// The model that ran, as the backend reports it.
    pub model: Option<String>,
    /// The model version, from the result. `null` when no model ran.
    pub version: Option<String>,
    /// The device the job ran on.
    pub device: Option<String>,
    /// The device that was asked for: `auto`, `cuda` or `cpu`.
    pub device_requested: Option<String>,
    /// Why a backend other than the requested one ran. `null` when the requested one did.
    pub fallback_reason: Option<String>,
    /// How long the fill itself took.
    pub elapsed_ms: Option<u64>,
    /// `done`, `failed` or `cancelled`.
    pub state: String,
}

/// Where the log goes: next to the endpoint file this run writes, or in the data folder when the
/// run writes no endpoint file at all. `None` only when the platform has nowhere to put files.
#[must_use]
pub fn activity_path_for(endpoint: Option<&Path>) -> Option<PathBuf> {
    let folder = endpoint.and_then(|path| path.parent()).filter(|parent| !parent.as_os_str().is_empty()).map(PathBuf::from).or_else(endpoint::data_dir)?;
    Some(folder.join(ACTIVITY_NAME))
}

/// Append one finished job, then trim the file if it has outgrown its keep. The whole append and
/// trim runs under [LOG_LOCK], and the line goes out in one write, so two jobs finishing at the same
/// moment cannot split each other's line or drop each other's record.
pub fn append(path: &Path, record: &Record) -> Result<()> {
    let _log = LOG_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(folder) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) else {
        return Err(Fail::internal(format!("Craft AI Manager has no folder to write {} in.", path.display())));
    };
    std::fs::create_dir_all(folder).map_err(|error| Fail::internal(format!("Craft AI Manager could not create {}: {error}", folder.display())))?;
    let line = format!("{}\n", serde_json::to_string(record)?);
    let written = OpenOptions::new().create(true).append(true).open(path).and_then(|mut file| file.write_all(line.as_bytes()).and_then(|()| file.flush()));
    if let Err(error) = written {
        return Err(Fail::internal(format!("Craft AI Manager could not write its activity log at {}: {error}", path.display())));
    }
    trim(path)
}

/// Keep the newest [MAX_LINES] lines, but only once the file has passed [TRIM_AT]: rewriting on
/// every append would be a bigger cost than the log is worth.
///
/// A file far larger than the log can legitimately hold is read from the end, so trimming never
/// reads the whole of it into memory.
fn trim(path: &Path) -> Result<()> {
    let file = File::open(path).map_err(|error| Fail::internal(format!("Craft AI Manager could not read its activity log at {}: {error}", path.display())))?;
    let skip = file.metadata().ok().map(|metadata| metadata.len().saturating_sub(MAX_READ_BYTES)).unwrap_or(0);
    let mut reader = BufReader::new(file);
    if skip > 0 {
        reader
            .seek(SeekFrom::Start(skip))
            .map_err(|error| Fail::internal(format!("Craft AI Manager could not read its activity log at {}: {error}", path.display())))?;
        // The first thing read is the tail of a line that started before the seek.
        let mut partial = String::new();
        reader
            .read_line(&mut partial)
            .map_err(|error| Fail::internal(format!("Craft AI Manager could not read its activity log at {}: {error}", path.display())))?;
    }
    let mut kept: VecDeque<String> = VecDeque::new();
    let mut lines = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => return Err(Fail::internal(format!("Craft AI Manager could not read its activity log at {}: {error}", path.display()))),
        }
        lines = lines.saturating_add(1);
        let text = line.trim_end_matches(['\n', '\r']);
        if text.len() > MAX_LINE_BYTES {
            continue;
        }
        kept.push_back(text.to_string());
        if kept.len() > MAX_LINES {
            kept.pop_front();
        }
    }
    if skip == 0 && lines <= TRIM_AT {
        return Ok(());
    }
    let temp = temp_path(path);
    let body = kept.iter().map(|entry| format!("{entry}\n")).collect::<String>();
    let written = std::fs::File::create(&temp).and_then(|mut file| file.write_all(body.as_bytes()).and_then(|()| file.flush()));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(Fail::internal(format!("Craft AI Manager could not rewrite its activity log at {}: {error}", path.display())));
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(Fail::internal(format!("Craft AI Manager could not put its activity log at {}: {error}", path.display())));
    }
    Ok(())
}

/// A temp name in the same folder as the target, so the rename stays inside one folder.
fn temp_path(path: &Path) -> PathBuf {
    let counter = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("{}.tmp-{}-{counter}", path.file_name().map(std::ffi::OsStr::to_string_lossy).unwrap_or_default(), std::process::id());
    path.with_file_name(name)
}

/// The timestamp of the line being written, right now.
#[must_use]
pub fn now_stamp() -> String {
    utc_stamp(crate::store::now_unix())
}

/// Seconds since the epoch as `YYYY-MM-DDThh:mm:ssZ`, computed rather than looked up: there is no
/// time zone database in here, and the log does not need one.
#[must_use]
pub fn utc_stamp(unix: u64) -> String {
    let seconds = unix % 86_400;
    let (year, month, day) = civil_from_days(i64::try_from(unix / 86_400).unwrap_or(i64::MAX));
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", seconds / 3_600, seconds % 3_600 / 60, seconds % 60)
}

/// Howard Hinnant's `civil_from_days`: the calendar date `days` after 1970-01-01, which is what
/// keeps this right across leap years and centuries without a table.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z < 0 { z - 146_096 } else { z } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = i64::try_from(yoe).unwrap_or(i64::MAX).saturating_add(era.saturating_mul(400));
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::{ACTIVITY_NAME, MAX_LINES, TRIM_AT, activity_path_for, append, utc_stamp};
    use crate::activity::Record;
    use std::path::PathBuf;

    fn folder(who: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("caim-activity-it-{}-{who}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the folder is made");
        path
    }

    fn record(state: &str) -> Record {
        Record {
            time: utc_stamp(1_760_000_000),
            task: Some("fill_masked_area".to_string()),
            model: Some("lama".to_string()),
            version: None,
            device: Some("cpu".to_string()),
            device_requested: None,
            fallback_reason: None,
            elapsed_ms: Some(1_234),
            state: state.to_string(),
        }
    }

    fn lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path).expect("the log is readable").lines().map(str::to_string).collect()
    }

    /// A JSON key as it appears in the file, quotes and all.
    fn q(name: &str) -> String {
        format!("\"{name}\"")
    }

    #[test]
    fn the_stamp_is_the_utc_time_of_those_seconds() {
        assert_eq!(utc_stamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_stamp(1_735_689_600), "2025-01-01T00:00:00Z");
        assert_eq!(utc_stamp(1_704_067_199), "2023-12-31T23:59:59Z");
        // A leap day, and the century that is not one.
        assert_eq!(utc_stamp(1_740_614_400), "2025-02-27T00:00:00Z");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_stamp(1_893_456_000), "2030-01-01T00:00:00Z");
    }

    #[test]
    fn one_line_per_finished_job_is_appended_with_the_fields_the_amendment_asks_for() {
        let path = folder("append").join(ACTIVITY_NAME);
        append(&path, &record("done")).expect("the first line goes in");
        append(&path, &record("cancelled")).expect("the second line goes in");
        let written = lines(&path);
        assert_eq!(written.len(), 2, "one line each");
        let first: Record = serde_json::from_str(written.first().expect("a line")).expect("the line is JSON");
        assert_eq!(first.state, "done");
        assert_eq!(first.task.as_deref(), Some("fill_masked_area"));
        assert_eq!(first.elapsed_ms, Some(1_234));
        assert!(written.first().expect("a line").contains(&(q("version") + ":null")), "the fields MG-10 and MG-11 fill in are written as null");
        assert!(written.first().expect("a line").contains(&(q("device_requested") + ":null")));
        assert!(written.first().expect("a line").contains(&(q("fallback_reason") + ":null")));
    }

    #[test]
    fn a_log_over_1100_lines_is_trimmed_to_the_newest_1000() {
        let path = folder("trim").join(ACTIVITY_NAME);
        {
            let mut file = std::fs::File::create(&path).expect("the log is made");
            for n in 0..TRIM_AT {
                use std::io::Write;
                file.write_all(format!("{{\"n\": {n}}}\n").as_bytes()).expect("a line is written");
            }
        }
        append(&path, &record("done")).expect("one more line");
        let kept = lines(&path);
        assert_eq!(kept.len(), MAX_LINES, "the newest {MAX_LINES} lines survive");
        assert_eq!(kept.first().expect("a line"), "{\"n\": 101}", "the oldest 101 are gone");
        assert!(kept.last().expect("a line").contains("\"state\":\"done\""), "and the line just written is the last one");
    }

    #[test]
    fn a_log_at_the_keep_is_left_alone() {
        let path = folder("no-trim").join(ACTIVITY_NAME);
        {
            let mut file = std::fs::File::create(&path).expect("the log is made");
            for n in 0..TRIM_AT.saturating_sub(2) {
                use std::io::Write;
                file.write_all(format!("{{\"n\": {n}}}\n").as_bytes()).expect("a line is written");
            }
        }
        let before = std::fs::metadata(&path).expect("the log is there").len();
        append(&path, &record("failed")).expect("one more line");
        let kept = lines(&path);
        assert_eq!(kept.len(), TRIM_AT.saturating_sub(1), "nothing is dropped below the trim point");
        let after = std::fs::metadata(&path).expect("the log is there").len();
        assert!(after > before && after <= before + 512, "the append is one write, not a rewrite of {before} bytes");
    }

    #[test]
    fn jobs_finishing_at_the_same_time_each_keep_one_whole_line() {
        // Connection threads finish different jobs at the same moment, so the append and its trim run
        // one at a time: every job keeps exactly one line, and no line is a mix of two.
        let path = folder("threads").join(ACTIVITY_NAME);
        let mut workers = Vec::new();
        for worker in 0..4 {
            let path = path.clone();
            workers.push(std::thread::spawn(move || {
                for number in 0..5 {
                    let mut entry = record("done");
                    entry.model = Some(format!("worker-{worker}-{number}"));
                    append(&path, &entry).expect("a line goes in");
                }
            }));
        }
        for worker in workers {
            let _ = worker.join();
        }
        let written = lines(&path);
        assert_eq!(written.len(), 20, "every finished job kept one line");
        for line in &written {
            let entry: Record = serde_json::from_str(line).expect("every line is whole JSON");
            assert!(entry.model.unwrap_or_default().starts_with("worker-"), "and no line is a mix of two: {line}");
        }
    }

    #[test]
    fn the_log_sits_next_to_the_endpoint_file() {
        let endpoint = std::path::PathBuf::from("some").join("where").join("endpoint.json");
        let path = activity_path_for(Some(&endpoint)).expect("a path");
        assert_eq!(path, PathBuf::from("some").join("where").join(ACTIVITY_NAME), "the same folder as endpoint.json");
        let none = activity_path_for(None).expect("the data folder has a place for it");
        assert_eq!(none.file_name().expect("a name").to_string_lossy(), ACTIVITY_NAME);
        assert_eq!(none.parent().expect("a folder"), crate::endpoint::data_dir().expect("the data folder"));
    }
}
