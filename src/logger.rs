use crate::cli::LogFormat;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub(crate) struct Logger {
    path: PathBuf,
    per_channel: bool,
    format: LogFormat,
    include_channel: bool,
    merged: Option<BufWriter<File>>,
    channels: HashMap<u32, ChannelLog>,
}

/// Log sink and decoding state for one RTT up channel.
struct ChannelLog {
    /// Per-channel file from `--log-per-channel`; `None` while merged.
    file: Option<BufWriter<File>>,
    state: ChannelState,
}

/// Decoding state selected once per channel from its RTT channel mode.
enum ChannelState {
    /// Raw RTT bytes, written without decoding.
    Raw,
    /// Shell output, decoded through a VT terminal model.
    Terminal(Box<DecodedStream>),
    /// Formatted defmt text, buffered until a newline.
    Text(Vec<u8>),
}

pub(crate) struct DecodedStream {
    line: Vec<Option<Cell>>,
    cursor: usize,
    parser: vte::Parser,
    complete: Vec<Vec<u8>>,
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
        include_channel: bool,
    ) -> Result<Option<Self>> {
        let Some(path) = path else { return Ok(None) };
        if format == LogFormat::Raw && !per_channel && include_channel {
            bail!("--log-format raw with multiple up channels requires --log-per-channel");
        }
        let mut logger = Self {
            path: path.to_path_buf(),
            per_channel,
            format,
            include_channel,
            merged: None,
            channels: HashMap::new(),
        };
        if !per_channel {
            logger.merged = Some(BufWriter::new(open_log(path)?));
        }
        Ok(Some(logger))
    }

    fn channel_log(&mut self, channel: u32, state: impl FnOnce() -> ChannelState) -> Result<()> {
        use std::collections::hash_map::Entry;
        if let Entry::Vacant(entry) = self.channels.entry(channel) {
            let file = if self.per_channel {
                Some(BufWriter::new(open_log(&channel_path(
                    &self.path, channel,
                ))?))
            } else {
                None
            };
            entry.insert(ChannelLog {
                file,
                state: state(),
            });
        }
        Ok(())
    }

    fn channel_mut(&mut self, channel: u32) -> &mut ChannelLog {
        self.channels
            .get_mut(&channel)
            .expect("channel log initialized")
    }

    fn file_for_channel(&mut self, channel: u32) -> Result<&mut BufWriter<File>> {
        if self.per_channel {
            let log = self.channel_mut(channel);
            Ok(log.file.as_mut().expect("per-channel log file initialized"))
        } else {
            Ok(self.merged.as_mut().expect("merged file initialized"))
        }
    }

    /// Append exact RTT bytes; raw log mode only.
    pub(crate) fn write_bytes(&mut self, channel: u32, bytes: &[u8]) -> Result<()> {
        if self.format != LogFormat::Raw || bytes.is_empty() {
            return Ok(());
        }
        self.channel_log(channel, || ChannelState::Raw)?;
        self.file_for_channel(channel)?
            .write_all(bytes)
            .with_context(|| format!("writing raw log for channel {channel}"))?;
        Ok(())
    }

    /// Append terminal-channel bytes after VT decoding; decoded mode only.
    pub(crate) fn write_chars(&mut self, channel: u32, bytes: &[u8]) -> Result<()> {
        if self.format != LogFormat::Decoded || bytes.is_empty() {
            return Ok(());
        }
        self.channel_log(channel, || {
            ChannelState::Terminal(Box::new(DecodedStream::new()))
        })?;
        let complete = self.channel_mut(channel).terminal_stream().consume(bytes);
        if complete.is_empty() {
            return Ok(());
        }
        let include_channel = self.include_channel;
        {
            let file = self.file_for_channel(channel)?;
            for line in &complete {
                if include_channel {
                    write!(file, "[ch{channel}] ")?;
                }
                file.write_all(line)?;
            }
        }
        self.flush_files()
    }

    /// Append an already-decoded defmt line; decoded mode only.
    pub(crate) fn write_defmt_decoded(&mut self, channel: u32, line: &[u8]) -> Result<()> {
        if self.format != LogFormat::Decoded || line.is_empty() {
            return Ok(());
        }
        self.channel_log(channel, || ChannelState::Text(Vec::new()))?;
        let mut pending = std::mem::take(self.channel_mut(channel).text_slot());
        pending.extend_from_slice(line);

        let mut start = 0;
        let has_complete_line = memchr::memchr(b'\n', &pending).is_some();
        if has_complete_line {
            let include_channel = self.include_channel;
            {
                let file = self.file_for_channel(channel)?;
                while let Some(offset) = memchr::memchr(b'\n', &pending[start..]) {
                    let end = start + offset + 1;
                    if include_channel {
                        write!(file, "[ch{channel}] ")?;
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
        *self.channel_mut(channel).text_slot() = pending;
        Ok(())
    }

    fn partial_lines(&self) -> Vec<(u32, Vec<u8>)> {
        self.channels
            .iter()
            .filter_map(|(&channel, log)| {
                let line = match &log.state {
                    ChannelState::Terminal(stream) => stream.visible_line(),
                    ChannelState::Text(pending) => pending.clone(),
                    ChannelState::Raw => Vec::new(),
                };
                (!line.is_empty()).then_some((channel, line))
            })
            .collect()
    }

    fn write_tails(&mut self, tails: &[(u32, Vec<u8>)], newline: bool) -> Result<()> {
        let include_channel = self.include_channel;
        for (channel, line) in tails {
            let file = self.file_for_channel(*channel)?;
            if include_channel {
                write!(file, "[ch{channel}] ")?;
            }
            file.write_all(line)?;
            if newline {
                file.write_all(b"\n")?;
            }
        }
        Ok(())
    }

    /// Writes any buffered partial lines and flushes the log files.
    pub(crate) fn flush(&mut self) -> Result<()> {
        let tails = self.partial_lines();
        self.write_tails(&tails, false)?;
        self.flush_files()
    }

    /// Flushes buffered log data without emitting partial lines.
    pub(crate) fn flush_files(&mut self) -> Result<()> {
        if let Some(file) = &mut self.merged {
            file.flush().context("flushing log file")?;
        }
        for log in self.channels.values_mut() {
            if let Some(file) = &mut log.file {
                file.flush().context("flushing per-channel log file")?;
            }
        }
        Ok(())
    }

    /// Finalizes partial lines and clears all per-target terminal state.
    ///
    /// Used when the target restarts so output from a new boot is not merged
    /// with the previous session's partial line.
    pub(crate) fn reset(&mut self) -> Result<()> {
        let tails = self.partial_lines();
        self.write_tails(&tails, true)?;
        for log in self.channels.values_mut() {
            match &mut log.state {
                ChannelState::Terminal(stream) => stream.reset(),
                ChannelState::Text(pending) => pending.clear(),
                ChannelState::Raw => {}
            }
        }
        self.flush_files()
    }
}

impl ChannelLog {
    fn terminal_stream(&mut self) -> &mut DecodedStream {
        match &mut self.state {
            ChannelState::Terminal(stream) => stream,
            _ => unreachable!("channel log is not a terminal stream"),
        }
    }

    fn text_slot(&mut self) -> &mut Vec<u8> {
        match &mut self.state {
            ChannelState::Text(pending) => pending,
            _ => unreachable!("channel log is not defmt text"),
        }
    }
}

impl DecodedStream {
    pub(crate) fn new() -> Self {
        Self {
            line: Vec::new(),
            cursor: 0,
            parser: vte::Parser::new(),
            complete: Vec::new(),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.line.clear();
        self.cursor = 0;
        self.parser = vte::Parser::new();
        self.complete.clear();
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
#[path = "../tests/logger.rs"]
mod tests;
