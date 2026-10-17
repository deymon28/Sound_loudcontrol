//! Minimal file logger: `%APPDATA%\LoudControl\loudcontrol.log`, rotated to `.old` above 1 MB.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 1024 * 1024;

struct LogFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl LogFile {
    fn open(path: PathBuf) -> Self {
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
            rotate(&path);
        }
        let file = OpenOptions::new().create(true).append(true).open(&path).ok();
        let size = file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len());
        Self { path, file, size }
    }

    fn write(&mut self, line: &str) {
        // Rotate while running too, so a long session cannot grow the log without bound.
        if self.size + line.len() as u64 > MAX_LOG_BYTES && self.size > 0 {
            self.file = None;
            rotate(&self.path);
            self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
            self.size = 0;
        }
        if let Some(f) = self.file.as_mut()
            && f.write_all(line.as_bytes()).is_ok()
        {
            self.size += line.len() as u64;
        }
    }
}

fn rotate(path: &Path) {
    let _ = std::fs::rename(path, path.with_extension("log.old"));
}

struct FileLogger {
    file: Mutex<LogFile>,
    level: log::LevelFilter,
    echo: bool,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level && metadata.target().starts_with("loudcontrol")
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} [{}] {}\n",
            timestamp(SystemTime::now()),
            record.level(),
            record.target().trim_start_matches("loudcontrol::"),
            record.args()
        );
        if self.echo {
            eprint!("{line}");
        }
        // A panic elsewhere must not silence the log.
        self.file.lock().unwrap_or_else(|e| e.into_inner()).write(&line);
    }

    fn flush(&self) {
        if let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).file.as_mut() {
            let _ = f.flush();
        }
    }
}

/// UTC date and time with milliseconds, e.g. `2026-10-18 21:04:05.123Z`.
fn timestamp(now: SystemTime) -> String {
    let d = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, m, day) = civil_from_days((secs / 86_400) as i64);
    format!(
        "{y:04}-{m:02}-{day:02} {:02}:{:02}:{:02}.{:03}Z",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60,
        d.subsec_millis()
    )
}

/// Days since 1970-01-01 to a (year, month, day) Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn init(path: &Path, echo: bool) {
    let level = match std::env::var("LOUDCONTROL_LOG").as_deref() {
        Ok("debug") => log::LevelFilter::Debug,
        Ok("trace") => log::LevelFilter::Trace,
        Ok("warn") => log::LevelFilter::Warn,
        _ => log::LevelFilter::Info,
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let logger = FileLogger { file: Mutex::new(LogFile::open(path.to_owned())), level, echo };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps_are_utc_dates() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01 00:00:00.000Z");
        // 2024-02-29 23:59:59.250 UTC: leap day.
        let t = UNIX_EPOCH + Duration::from_millis(1_709_251_199_250);
        assert_eq!(timestamp(t), "2024-02-29 23:59:59.250Z");
        let t = UNIX_EPOCH + Duration::from_secs(4_102_444_800);
        assert_eq!(timestamp(t), "2100-01-01 00:00:00.000Z");
    }
}
