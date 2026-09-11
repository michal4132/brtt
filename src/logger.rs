use crate::cli::LogFormat;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub(crate) struct Logger {
    path: PathBuf,
    per_channel: bool,
    format: LogFormat,
    merged: Option<BufWriter<File>>,
    channels: HashMap<u32, BufWriter<File>>,
    streams: HashMap<u32, DecodedStream>,
    text_pending: HashMap<u32, (Vec<u8>, bool)>,
    timestamps: bool,
    started: Instant,
    started_wall: DateTime<Local>,
}

pub(crate) struct DecodedStream {
    line: Vec<Option<Cell>>,
    cursor: usize,
    parser: vte::Parser,
    complete: Vec<Vec<u8>>,
    include_channel: bool,
}

#[derive(Clone)]
struct Cell {
    character: char,
    combining: Option<String>,
}

const MAX_TERMINAL_COLUMNS: usize = 4096;
const MAX_COMBINING_MARKS: usize = 16;

impl Logger {
    pub(crate) fn new(
        path: Option<&Path>,
        per_channel: bool,
        format: LogFormat,
        channel_count: usize,
    ) -> Result<Option<Self>> {
        let Some(path) = path else { return Ok(None) };
        if format == LogFormat::Raw && !per_channel && channel_count > 1 {
            bail!("--log-format raw with multiple up channels requires --log-per-channel");
        }
        let mut logger = Self {
            path: path.to_path_buf(),
            per_channel,
            format,
            merged: None,
            channels: HashMap::new(),
            streams: HashMap::new(),
            text_pending: HashMap::new(),
            timestamps: false,
            started: Instant::now(),
            started_wall: Local::now(),
        };
        if !per_channel {
            logger.merged = Some(BufWriter::new(open_log(path)?));
        }
        Ok(Some(logger))
    }

    /// Enables or disables timestamp prefixes for decoded log lines.
    ///
    /// Raw logs always preserve the exact RTT bytes and are not affected.
    pub(crate) fn set_timestamps(&mut self, enabled: bool) {
        self.timestamps = enabled;
    }

    fn timestamp_prefix(&self, timestamp: Instant) -> String {
        let elapsed = timestamp.saturating_duration_since(self.started);
        let wall = self.started_wall
            + chrono::Duration::from_std(elapsed).unwrap_or_else(|_| chrono::Duration::zero());
        format!("[{}] ", wall.format("%Y-%m-%d %H:%M:%S%.3f"))
    }

    fn file_for_channel(&mut self, channel: u32) -> Result<&mut BufWriter<File>> {
        if self.per_channel {
            use std::collections::hash_map::Entry;
            match self.channels.entry(channel) {
                Entry::Occupied(entry) => Ok(entry.into_mut()),
                Entry::Vacant(entry) => {
                    let path = channel_path(&self.path, channel);
                    Ok(entry.insert(BufWriter::new(open_log(&path)?)))
                }
            }
        } else {
            Ok(self.merged.as_mut().expect("merged file initialized"))
        }
    }

    pub(crate) fn write_raw(&mut self, channel: u32, bytes: &[u8]) -> Result<()> {
        if self.format != LogFormat::Raw || bytes.is_empty() {
            return Ok(());
        }
        self.file_for_channel(channel)?
            .write_all(bytes)
            .with_context(|| format!("writing raw log for channel {channel}"))?;
        Ok(())
    }

    pub(crate) fn write_terminal(
        &mut self,
        channel: u32,
        bytes: &[u8],
        include_channel: bool,
        timestamp: Instant,
    ) -> Result<()> {
        if self.format != LogFormat::Decoded || bytes.is_empty() {
            return Ok(());
        }
        let (complete, include_channel) = {
            let stream = self
                .streams
                .entry(channel)
                .or_insert_with(|| DecodedStream {
                    line: Vec::new(),
                    cursor: 0,
                    parser: vte::Parser::new(),
                    complete: Vec::new(),
                    include_channel,
                });
            (stream.consume(bytes), stream.include_channel)
        };
        if complete.is_empty() {
            return Ok(());
        }
        let tag = include_channel.then(|| format!("[ch{channel}] "));
        let prefix = self.timestamps.then(|| self.timestamp_prefix(timestamp));
        {
            let file = self.file_for_channel(channel)?;
            for line in &complete {
                if let Some(prefix) = &prefix {
                    file.write_all(prefix.as_bytes())?;
                }
                if let Some(tag) = &tag {
                    file.write_all(tag.as_bytes())?;
                }
                file.write_all(line)?;
            }
        }
        self.flush_files()
    }

    pub(crate) fn write_text(
        &mut self,
        channel: u32,
        bytes: &[u8],
        include_channel: bool,
        timestamp: Instant,
    ) -> Result<()> {
        if self.format != LogFormat::Decoded || bytes.is_empty() {
            return Ok(());
        }
        let mut pending = {
            let entry = self
                .text_pending
                .entry(channel)
                .or_insert_with(|| (Vec::new(), include_channel));
            std::mem::take(&mut entry.0)
        };
        pending.extend_from_slice(bytes);

        let mut start = 0;
        let has_complete_line = memchr::memchr(b'\n', &pending).is_some();
        if has_complete_line {
            let tag = include_channel.then(|| format!("[ch{channel}] "));
            let prefix = self.timestamps.then(|| self.timestamp_prefix(timestamp));
            {
                let file = self.file_for_channel(channel)?;
                while let Some(offset) = memchr::memchr(b'\n', &pending[start..]) {
                    let end = start + offset + 1;
                    if let Some(prefix) = &prefix {
                        file.write_all(prefix.as_bytes())?;
                    }
                    if let Some(tag) = &tag {
                        file.write_all(tag.as_bytes())?;
                    }
                    file.write_all(&pending[start..end])?;
                    start = end;
                }
            }
            self.flush_files()?;
        }
        if start > 0 {
            pending.drain(..start);
        }
        self.text_pending
            .insert(channel, (pending, include_channel));
        Ok(())
    }

    /// Writes any buffered partial lines and flushes the log files.
    pub(crate) fn flush(&mut self) -> Result<()> {
        let prefix = self
            .timestamps
            .then(|| self.timestamp_prefix(Instant::now()));
        let stream_tails: Vec<_> = self
            .streams
            .iter()
            .filter_map(|(&channel, stream)| {
                let line = stream.visible_line();
                (!line.is_empty()).then_some((channel, stream.include_channel, line))
            })
            .collect();
        for (channel, include_channel, line) in stream_tails {
            let tag = include_channel.then(|| format!("[ch{channel}] "));
            let file = self.file_for_channel(channel)?;
            if let Some(prefix) = &prefix {
                file.write_all(prefix.as_bytes())?;
            }
            if let Some(tag) = &tag {
                file.write_all(tag.as_bytes())?;
            }
            file.write_all(&line)?;
        }

        let text_tails: Vec<_> = self
            .text_pending
            .iter()
            .filter_map(|(&channel, (bytes, include_channel))| {
                (!bytes.is_empty()).then_some((channel, *include_channel, bytes.clone()))
            })
            .collect();
        for (channel, include_channel, bytes) in text_tails {
            let tag = include_channel.then(|| format!("[ch{channel}] "));
            let file = self.file_for_channel(channel)?;
            if let Some(prefix) = &prefix {
                file.write_all(prefix.as_bytes())?;
            }
            if let Some(tag) = &tag {
                file.write_all(tag.as_bytes())?;
            }
            file.write_all(&bytes)?;
        }

        self.flush_files()
    }

    /// Flushes buffered log data without emitting partial lines.
    pub(crate) fn flush_files(&mut self) -> Result<()> {
        if let Some(file) = &mut self.merged {
            file.flush().context("flushing log file")?;
        }
        for file in self.channels.values_mut() {
            file.flush().context("flushing per-channel log file")?;
        }
        Ok(())
    }

    /// Finalizes partial lines and clears all per-target terminal state.
    ///
    /// Used when the target restarts so output from a new boot is not merged
    /// with the previous session's partial line.
    pub(crate) fn reset(&mut self) -> Result<()> {
        let prefix = self
            .timestamps
            .then(|| self.timestamp_prefix(Instant::now()));
        let stream_tails: Vec<_> = self
            .streams
            .iter()
            .filter_map(|(&channel, stream)| {
                let line = stream.visible_line();
                (!line.is_empty()).then_some((channel, stream.include_channel, line))
            })
            .collect();
        for (channel, include_channel, line) in stream_tails {
            let tag = include_channel.then(|| format!("[ch{channel}] "));
            let file = self.file_for_channel(channel)?;
            if let Some(prefix) = &prefix {
                file.write_all(prefix.as_bytes())?;
            }
            if let Some(tag) = &tag {
                file.write_all(tag.as_bytes())?;
            }
            file.write_all(&line)?;
            file.write_all(b"\n")?;
        }

        let text_tails: Vec<_> = self
            .text_pending
            .iter()
            .filter_map(|(&channel, (bytes, include_channel))| {
                (!bytes.is_empty()).then_some((channel, *include_channel, bytes.clone()))
            })
            .collect();
        for (channel, include_channel, bytes) in text_tails {
            let tag = include_channel.then(|| format!("[ch{channel}] "));
            let file = self.file_for_channel(channel)?;
            if let Some(prefix) = &prefix {
                file.write_all(prefix.as_bytes())?;
            }
            if let Some(tag) = &tag {
                file.write_all(tag.as_bytes())?;
            }
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
        }

        for stream in self.streams.values_mut() {
            stream.line.clear();
            stream.cursor = 0;
            stream.parser = vte::Parser::new();
            stream.complete.clear();
        }
        for (bytes, _) in self.text_pending.values_mut() {
            bytes.clear();
        }
        self.flush_files()
    }
}

impl DecodedStream {
    pub(crate) fn new() -> Self {
        Self {
            line: Vec::new(),
            cursor: 0,
            parser: vte::Parser::new(),
            complete: Vec::new(),
            include_channel: false,
        }
    }

    pub(crate) fn consume(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
        std::mem::take(&mut self.complete)
    }

    fn finish_line(&mut self) -> Vec<u8> {
        let mut line = self.visible_line();
        line.push(b'\n');
        self.line.clear();
        self.cursor = 0;
        line
    }

    pub(crate) fn visible_line(&self) -> Vec<u8> {
        let end = self
            .line
            .iter()
            .rposition(Option::is_some)
            .map_or(0, |index| index + 1);
        let mut output = Vec::new();
        for cell in &self.line[..end] {
            match cell {
                Some(cell) => {
                    let mut buffer = [0; 4];
                    output.extend_from_slice(cell.character.encode_utf8(&mut buffer).as_bytes());
                    if let Some(combining) = &cell.combining {
                        output.extend_from_slice(combining.as_bytes());
                    }
                }
                None => output.push(b' '),
            }
        }
        output
    }
}

impl Perform for DecodedStream {
    fn print(&mut self, character: char) {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width == 0 {
            let end = self.cursor.min(self.line.len());
            if let Some(cell) = self.line[..end].iter_mut().rev().flatten().next() {
                let combining = cell.combining.get_or_insert_with(String::new);
                if combining.chars().count() < MAX_COMBINING_MARKS {
                    combining.push(character);
                }
            }
            return;
        }
        self.cursor = self.cursor.min(MAX_TERMINAL_COLUMNS);
        let end = self.cursor.saturating_add(width).min(MAX_TERMINAL_COLUMNS);
        if end <= self.cursor {
            return;
        }
        if self.line.len() < end {
            self.line.resize(end, None);
        }
        self.line[self.cursor] = Some(Cell {
            character,
            combining: None,
        });
        self.line[self.cursor + 1..end].fill(None);
        self.cursor += width;
        self.cursor = self.cursor.min(MAX_TERMINAL_COLUMNS);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.cursor = 0,
            b'\n' => {
                let line = self.finish_line();
                self.complete.push(line);
            }
            8 => self.cursor = self.cursor.saturating_sub(1),
            b'\t' => {
                self.cursor = self
                    .cursor
                    .saturating_add(8 - self.cursor % 8)
                    .min(MAX_TERMINAL_COLUMNS);
                if self.line.len() < self.cursor {
                    self.line.resize(self.cursor, None);
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _: &[u8], _: bool, action: char) {
        let value = || {
            params
                .iter()
                .next()
                .and_then(|values| values.first().copied())
                .unwrap_or(1) as usize
        };
        match action {
            'K' => match value() {
                0 => self.line.truncate(self.cursor.min(self.line.len())),
                1 => {
                    let end = self.cursor.saturating_add(1).min(self.line.len());
                    self.line[..end].fill(None);
                }
                2 => self.line.clear(),
                _ => {}
            },
            'G' | '`' => self.cursor = value().saturating_sub(1).min(MAX_TERMINAL_COLUMNS),
            'C' => {
                self.cursor = self
                    .cursor
                    .saturating_add(value())
                    .min(MAX_TERMINAL_COLUMNS)
            }
            'D' => self.cursor = self.cursor.saturating_sub(value()),
            _ => {}
        }
    }
}

fn open_log(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening log file '{}'", path.display()))
}

fn channel_path(path: &Path, channel: u32) -> PathBuf {
    let mut name = path
        .file_stem()
        .map(|stem| stem.to_os_string())
        .unwrap_or_else(|| OsString::from("log"));
    name.push(format!(".ch{channel}"));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "brtt-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn timestamp_at(logger: &Logger, offset_ms: i64) -> (String, Instant) {
        let timestamp = logger.started + Duration::from_millis(offset_ms as u64);
        let prefix = format!(
            "[{}] ",
            (logger.started_wall + chrono::Duration::milliseconds(offset_ms))
                .format("%Y-%m-%d %H:%M:%S%.3f")
        );
        (prefix, timestamp)
    }

    #[test]
    fn channel_paths_insert_suffix_before_extension() {
        assert_eq!(
            channel_path(Path::new("capture.log"), 2),
            PathBuf::from("capture.ch2.log")
        );
        assert_eq!(
            channel_path(Path::new("capture"), 2),
            PathBuf::from("capture.ch2")
        );
        assert_eq!(
            channel_path(Path::new("logs/capture"), 2),
            PathBuf::from("logs/capture.ch2")
        );
    }

    #[cfg(unix)]
    #[test]
    fn channel_paths_preserve_non_utf8_names() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"capture-\xff.log"));
        let channel_path = channel_path(path, 2);

        assert_eq!(channel_path.as_os_str().as_bytes(), b"capture-\xff.ch2.log");
    }

    #[test]
    fn merged_decoded_logs_are_channel_tagged() {
        let path = test_path("merged");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 2)
            .unwrap()
            .unwrap();
        logger
            .write_text(0, b"one\n", true, Instant::now())
            .unwrap();
        logger
            .write_text(1, b"two\n", true, Instant::now())
            .unwrap();
        logger.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"[ch0] one\n[ch1] two\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn raw_merged_logs_reject_multiple_channels() {
        let path = test_path("raw");
        assert!(Logger::new(Some(&path), false, LogFormat::Raw, 2).is_err());
    }

    #[test]
    fn merged_decoded_logs_keep_partial_channels_separate() {
        let path = test_path("merged-partial");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 2)
            .unwrap()
            .unwrap();
        logger.write_text(0, b"foo", true, Instant::now()).unwrap();
        logger
            .write_text(1, b"bar\n", true, Instant::now())
            .unwrap();
        logger.write_text(0, b"\n", true, Instant::now()).unwrap();
        logger.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"[ch1] bar\n[ch0] foo\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn per_channel_raw_logs_preserve_bytes() {
        let path = test_path("raw-per-channel.log");
        let mut logger = Logger::new(Some(&path), true, LogFormat::Raw, 2)
            .unwrap()
            .unwrap();
        logger.write_raw(1, &[0, 1, 0xff]).unwrap();
        logger.flush().unwrap();
        let channel_path = channel_path(&path, 1);
        assert_eq!(fs::read(&channel_path).unwrap(), &[0, 1, 0xff]);
        fs::remove_file(channel_path).unwrap();
    }

    #[test]
    fn per_channel_decoded_logs_do_not_need_channel_tags() {
        let path = test_path("decoded-per-channel.log");
        let mut logger = Logger::new(Some(&path), true, LogFormat::Decoded, 2)
            .unwrap()
            .unwrap();
        logger
            .write_text(1, b"message\n", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        let channel_path = channel_path(&path, 1);
        assert_eq!(fs::read(&channel_path).unwrap(), b"message\n");
        fs::remove_file(channel_path).unwrap();
    }

    #[test]
    fn decoded_logger_flushes_an_unfinished_line() {
        let path = test_path("decoded-tail");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_text(0, b"unfinished", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"unfinished");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoded_logger_collapses_terminal_redraws() {
        let path = test_path("decoded-redraw");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(
                0,
                b"\r\x1b[2K> help\r\x1b[2K> \r\x1b[2K> help\r\n",
                false,
                Instant::now(),
            )
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"> help\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoded_logger_handles_escape_sequences_split_between_reads() {
        let path = test_path("decoded-split-escape");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, b"old\r\x1b[", false, Instant::now())
            .unwrap();
        logger
            .write_terminal(0, b"2Knew\n", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoded_logger_keeps_echoed_commands() {
        let path = test_path("decoded-command");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, b"> pwd\r\n", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"> pwd\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoded_logger_overwrites_without_truncating_the_tail() {
        let path = test_path("decoded-overwrite");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, b"abc\x1b[1GX\n", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"Xbc\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoded_logger_keeps_tail_when_tab_moves_cursor_backwards() {
        let path = test_path("decoded-tab");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, b"abcdefghij\x1b[3G\t\n", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"abcdefghij\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn plain_decoded_text_does_not_interpret_terminal_controls() {
        let path = test_path("decoded-plain-text");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_text(0, b"value: \x1b[2K\n", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"value: \x1b[2K\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_logger_preserves_utf8_and_display_width() {
        let path = test_path("terminal-utf8");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, "ż界\n".as_bytes(), false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), "ż界\n".as_bytes());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_logger_keeps_combining_marks_with_their_base_character() {
        let path = test_path("terminal-combining");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, "e\u{301}\n".as_bytes(), false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), "e\u{301}\n".as_bytes());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_logger_erases_the_cursor_cell_with_csi_one_k() {
        let path = test_path("terminal-erase-before");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, b"abc\x1b[1K\n", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_logger_flush_preserves_parser_state() {
        let path = test_path("terminal-flush-state");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, b"old\r\x1b[", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();
        logger
            .write_terminal(0, b"2Knew\n", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"oldnew\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn logger_reset_clears_partial_terminal_state() {
        let path = test_path("reset-state");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, b"boot", false, Instant::now())
            .unwrap();
        logger.reset().unwrap();
        logger
            .write_terminal(0, b"next\n", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"boot\nnext\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn merged_decoded_logs_get_timestamps_before_channel_tags() {
        let path = test_path("merged-timestamps");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 2)
            .unwrap()
            .unwrap();
        logger.set_timestamps(true);
        let (prefix, timestamp) = timestamp_at(&logger, 250);
        logger.write_text(0, b"one\n", true, timestamp).unwrap();
        logger.write_text(1, b"two\n", true, timestamp).unwrap();
        logger.flush().unwrap();

        let expected = format!("{prefix}[ch0] one\n{prefix}[ch1] two\n");
        assert_eq!(fs::read(&path).unwrap(), expected.as_bytes());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn per_channel_decoded_logs_get_timestamps() {
        let path = test_path("decoded-per-channel-timestamps.log");
        let mut logger = Logger::new(Some(&path), true, LogFormat::Decoded, 2)
            .unwrap()
            .unwrap();
        logger.set_timestamps(true);
        let (prefix, timestamp) = timestamp_at(&logger, 100);
        logger
            .write_text(1, b"message\n", false, timestamp)
            .unwrap();
        logger.flush().unwrap();

        let channel_path = channel_path(&path, 1);
        let expected = format!("{prefix}message\n");
        assert_eq!(fs::read(&channel_path).unwrap(), expected.as_bytes());
        fs::remove_file(channel_path).unwrap();
    }

    #[test]
    fn timestamp_toggle_applies_to_subsequent_lines_only() {
        let path = test_path("timestamp-toggle");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        let (prefix, timestamp) = timestamp_at(&logger, 123);
        logger.write_text(0, b"before\n", false, timestamp).unwrap();
        logger.set_timestamps(true);
        logger.write_text(0, b"during\n", false, timestamp).unwrap();
        logger.set_timestamps(false);
        logger.write_text(0, b"after\n", false, timestamp).unwrap();
        logger.flush().unwrap();

        let expected = format!("before\n{prefix}during\nafter\n");
        assert_eq!(fs::read(&path).unwrap(), expected.as_bytes());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn raw_logs_ignore_timestamps() {
        let path = test_path("raw-timestamps.log");
        let mut logger = Logger::new(Some(&path), true, LogFormat::Raw, 1)
            .unwrap()
            .unwrap();
        logger.set_timestamps(true);
        logger.write_raw(0, &[0, 1, b'\n', 0xff]).unwrap();
        logger.flush().unwrap();

        let channel_path = channel_path(&path, 0);
        assert_eq!(fs::read(&channel_path).unwrap(), &[0, 1, b'\n', 0xff]);
        fs::remove_file(channel_path).unwrap();
    }

    #[test]
    fn decoded_terminal_logs_get_timestamps() {
        let path = test_path("terminal-timestamps");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger.set_timestamps(true);
        let (prefix, timestamp) = timestamp_at(&logger, 42);
        logger
            .write_terminal(0, b"boot\n", false, timestamp)
            .unwrap();
        logger.flush().unwrap();

        let expected = format!("{prefix}boot\n");
        assert_eq!(fs::read(&path).unwrap(), expected.as_bytes());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn flush_timestamps_an_unfinished_decoded_line() {
        let path = test_path("timestamp-tail");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger.set_timestamps(true);
        logger
            .write_text(0, b"unfinished", false, Instant::now())
            .unwrap();
        logger.flush().unwrap();

        let content = String::from_utf8(fs::read(&path).unwrap()).unwrap();
        assert!(content.starts_with('['), "missing timestamp in {content:?}");
        assert!(
            content.ends_with("] unfinished"),
            "unexpected tail in {content:?}"
        );
        fs::remove_file(path).unwrap();
    }
}
