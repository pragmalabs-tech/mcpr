use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::entry::LogEntry;
use super::sink::LogSink;

const DEFAULT_MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
pub const DEFAULT_MAX_FILES: usize = 10;
const BUFFER_CAPACITY: usize = 32 * 1024; // 32 KB

/// Rotation strategy for log files.
#[derive(Clone, Debug)]
pub enum Rotation {
    /// Rotate when the file exceeds a size threshold (in bytes).
    Size(u64),
    /// Rotate daily (new file per calendar day).
    Daily,
}

impl Default for Rotation {
    fn default() -> Self {
        Rotation::Size(DEFAULT_MAX_FILE_SIZE)
    }
}

/// Configuration for the file sink.
#[derive(Clone, Debug)]
pub struct FileSinkConfig {
    pub dir: PathBuf,
    pub rotation: Rotation,
    /// Maximum number of log files to keep. Oldest are deleted when exceeded.
    /// Defaults to 10. This caps total disk usage to roughly `max_files * rotation_size`.
    pub max_files: usize,
}

/// JSONL file sink with buffered writes and rotation.
///
/// Writes one JSON object per line to `{dir}/mcpr-{date}.log`.
/// Rotation happens by size or daily, depending on config.
pub struct FileSink {
    config: FileSinkConfig,
    inner: Mutex<FileSinkInner>,
}

struct FileSinkInner {
    writer: BufWriter<File>,
    current_path: PathBuf,
    current_date: String,
    bytes_written: u64,
}

impl FileSink {
    pub fn new(config: FileSinkConfig) -> std::io::Result<Self> {
        fs::create_dir_all(&config.dir)?;
        let (path, date) = Self::log_path(&config.dir);
        let file = Self::open_append(&path)?;
        let bytes_written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            config,
            inner: Mutex::new(FileSinkInner {
                writer: BufWriter::with_capacity(BUFFER_CAPACITY, file),
                current_path: path,
                current_date: date,
                bytes_written,
            }),
        })
    }

    fn log_path(dir: &Path) -> (PathBuf, String) {
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let path = dir.join(format!("mcpr-{date}.log"));
        (path, date)
    }

    fn open_append(path: &Path) -> std::io::Result<File> {
        OpenOptions::new().create(true).append(true).open(path)
    }

    fn should_rotate(&self, inner: &FileSinkInner) -> bool {
        match &self.config.rotation {
            Rotation::Size(max) => inner.bytes_written >= *max,
            Rotation::Daily => {
                let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                inner.current_date != today
            }
        }
    }

    fn rotate(&self, inner: &mut FileSinkInner) -> std::io::Result<()> {
        // Flush the current writer before rotating
        inner.writer.flush()?;

        match &self.config.rotation {
            Rotation::Size(_) => {
                // Rename current file with a sequence number
                let mut seq = 1u32;
                loop {
                    let rotated = inner.current_path.with_extension(format!("{seq}.log"));
                    if !rotated.exists() {
                        fs::rename(&inner.current_path, &rotated)?;
                        break;
                    }
                    seq += 1;
                }
                // Re-open same path (now empty)
                let file = Self::open_append(&inner.current_path)?;
                inner.writer = BufWriter::with_capacity(BUFFER_CAPACITY, file);
                inner.bytes_written = 0;
            }
            Rotation::Daily => {
                // Open new file with today's date
                let (path, date) = Self::log_path(&self.config.dir);
                let existing_size = path.metadata().map(|m| m.len()).unwrap_or(0);
                let file = Self::open_append(&path)?;
                inner.current_path = path;
                inner.current_date = date;
                inner.writer = BufWriter::with_capacity(BUFFER_CAPACITY, file);
                inner.bytes_written = existing_size;
            }
        }
        Ok(())
    }

    /// Delete oldest log files if the total count exceeds `max_files`.
    fn cleanup_old_files(&self) {
        let max = self.config.max_files;
        if max == 0 {
            return;
        }

        let Ok(entries) = fs::read_dir(&self.config.dir) else {
            return;
        };

        let mut log_files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("mcpr-") && n.ends_with(".log"))
            })
            .collect();

        if log_files.len() <= max {
            return;
        }

        // Sort by modification time (oldest first)
        log_files.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());

        let to_remove = log_files.len() - max;
        for path in log_files.into_iter().take(to_remove) {
            let _ = fs::remove_file(path);
        }
    }
}

impl LogSink for FileSink {
    fn emit(&self, entry: &LogEntry) {
        let mut inner = self.inner.lock().unwrap();

        // Check rotation before writing
        if self.should_rotate(&inner) {
            match self.rotate(&mut inner) {
                Ok(()) => self.cleanup_old_files(),
                Err(e) => {
                    eprintln!("mcpr: log rotation failed: {e}");
                    return;
                }
            }
        }

        // Serialize and write
        match serde_json::to_string(entry) {
            Ok(line) => {
                let bytes = line.len() as u64 + 1; // +1 for newline
                if let Err(e) = writeln!(inner.writer, "{line}") {
                    eprintln!("mcpr: log write failed: {e}");
                } else {
                    inner.bytes_written += bytes;
                }
            }
            Err(e) => {
                eprintln!("mcpr: log serialize failed: {e}");
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            let _ = inner.writer.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entry(method: &str, path: &str, status: u16) -> LogEntry {
        LogEntry::new(method, path, status, "test")
    }

    #[test]
    fn writes_jsonl_to_file() {
        let dir = TempDir::new().unwrap();
        let config = FileSinkConfig {
            dir: dir.path().to_path_buf(),
            rotation: Rotation::Size(DEFAULT_MAX_FILE_SIZE),
            max_files: DEFAULT_MAX_FILES,
        };
        let sink = FileSink::new(config).unwrap();

        sink.emit(&make_entry("POST", "/mcp", 200));
        sink.emit(&make_entry("GET", "/health", 200));
        sink.flush();

        // Read the log file
        let entries: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
            .flat_map(|e| {
                fs::read_to_string(e.path())
                    .unwrap()
                    .lines()
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .collect();

        assert_eq!(entries.len(), 2);

        // Verify each line is valid JSON with expected fields
        let first: serde_json::Value = serde_json::from_str(&entries[0]).unwrap();
        assert_eq!(first["method"], "POST");
        assert_eq!(first["path"], "/mcp");
        assert_eq!(first["status"], 200);
        assert!(first["timestamp_utc"].as_str().is_some());

        let second: serde_json::Value = serde_json::from_str(&entries[1]).unwrap();
        assert_eq!(second["method"], "GET");
        assert_eq!(second["path"], "/health");
    }

    #[test]
    fn rotates_by_size() {
        let dir = TempDir::new().unwrap();
        let config = FileSinkConfig {
            dir: dir.path().to_path_buf(),
            rotation: Rotation::Size(100), // Very small threshold
            max_files: DEFAULT_MAX_FILES,
        };
        let sink = FileSink::new(config).unwrap();

        // Write enough entries to trigger rotation
        for i in 0..10 {
            sink.emit(&make_entry("POST", &format!("/path/{i}"), 200));
        }
        sink.flush();

        // Should have multiple log files after rotation
        let log_files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("mcpr-")
            })
            .collect();

        assert!(
            log_files.len() > 1,
            "expected multiple log files after rotation, got {}",
            log_files.len()
        );
    }

    #[test]
    fn serializes_all_fields() {
        let dir = TempDir::new().unwrap();
        let config = FileSinkConfig {
            dir: dir.path().to_path_buf(),
            rotation: Rotation::Size(DEFAULT_MAX_FILE_SIZE),
            max_files: DEFAULT_MAX_FILES,
        };
        let sink = FileSink::new(config).unwrap();

        let entry = LogEntry::new("POST", "/mcp", 200, "rewritten")
            .mcp_method("tools/call")
            .detail("get_weather")
            .session_id("sid-123")
            .upstream("http://localhost:9000/mcp")
            .size(147)
            .upstream_duration(7)
            .jsonrpc_error(-32602, "Invalid params");

        sink.emit(&entry);
        sink.flush();

        let log_file = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension().is_some_and(|ext| ext == "log"))
            .unwrap();

        let content = fs::read_to_string(log_file.path()).unwrap();
        let val: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(val["method"], "POST");
        assert_eq!(val["path"], "/mcp");
        assert_eq!(val["status"], 200);
        assert_eq!(val["note"], "rewritten");
        assert_eq!(val["mcp_method"], "tools/call");
        assert_eq!(val["detail"], "get_weather");
        assert_eq!(val["session_id"], "sid-123");
        assert_eq!(val["upstream_url"], "http://localhost:9000/mcp");
        assert_eq!(val["resp_size"], 147);
        assert_eq!(val["upstream_ms"], 7);
        assert_eq!(val["jsonrpc_error"][0], -32602);
        assert_eq!(val["jsonrpc_error"][1], "Invalid params");
        assert!(val["timestamp_utc"].as_str().unwrap().contains("T"));
    }

    #[test]
    fn creates_dir_if_missing() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("logs").join("nested");
        let config = FileSinkConfig {
            dir: nested.clone(),
            rotation: Rotation::Size(DEFAULT_MAX_FILE_SIZE),
            max_files: DEFAULT_MAX_FILES,
        };

        let sink = FileSink::new(config).unwrap();
        sink.emit(&make_entry("GET", "/test", 200));
        sink.flush();

        assert!(nested.exists());
    }

    #[test]
    fn cleans_up_old_files() {
        let dir = TempDir::new().unwrap();
        let config = FileSinkConfig {
            dir: dir.path().to_path_buf(),
            rotation: Rotation::Size(100), // Very small to trigger many rotations
            max_files: 3,
        };
        let sink = FileSink::new(config).unwrap();

        // Write enough entries to trigger many rotations
        for i in 0..50 {
            sink.emit(&make_entry("POST", &format!("/path/{i}"), 200));
        }
        sink.flush();

        let log_files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("mcpr-")
            })
            .collect();

        assert!(
            log_files.len() <= 3,
            "expected at most 3 log files, got {}",
            log_files.len()
        );
    }
}
