use crate::cli::{ChannelEncoding, ChannelSpec};
use crate::defmt::{
    decode_frames, filter_level, level_enabled, level_name, DecodeOutput, DecodedFrame, DefmtData,
    Filter, MAX_DECODE_BUFFERED_BYTES,
};
use crate::logger::{DecodedStream, Logger};
use anyhow::{bail, Context, Result};
use brtt::rtt::{
    try_attach_to_rtt, try_attach_to_rtt_incremental, Error as RttError, Rtt, ScanRegion,
};
use brtt::RttChannel;
use chrono::{DateTime, Local};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use probe_rs::Core;
use std::collections::{HashMap, VecDeque};
use std::io::prelude::*;
use std::io::{stdout, BufWriter, IsTerminal};
use std::time::{Duration, Instant};

const RTT_REATTACH_TIMEOUT: Duration = Duration::from_secs(3);
const TARGET_HALT_TIMEOUT: Duration = Duration::from_millis(100);
const UP_CHANNEL_BUFFER_SIZE: usize = 4096;
const RTT_READ_BUDGET_PER_POLL: usize = 64 * 1024;
const MAX_DOWN_BUFFER_BYTES: usize = 64 * 1024;
const MAX_SESSION_RAW_LINE_BYTES: usize = 4096;

struct UpChannelReader<'table> {
    spec: ChannelSpec,
    channel_number: usize,
    buffer: Vec<u8>,
    decoder: Option<Box<dyn defmt_decoder::StreamDecoder + Send + Sync + 'table>>,
    defmt: Option<&'table DefmtData>,
    defmt_buffered_bytes: usize,
}

impl<'table> UpChannelReader<'table> {
    fn new(spec: ChannelSpec, defmt: Option<&'table DefmtData>) -> Result<Self> {
        let channel_number = usize::try_from(spec.index).with_context(|| {
            format!(
                "up channel index {} cannot be represented on this host",
                spec.index
            )
        })?;
        let decoder = match (spec.mode, defmt) {
            (ChannelEncoding::Defmt, Some(defmt)) => Some(defmt.table.new_stream_decoder()),
            (ChannelEncoding::Defmt, None) => {
                bail!("missing defmt table for channel {}", spec.index)
            }
            _ => None,
        };
        Ok(Self {
            spec,
            channel_number,
            buffer: vec![0; UP_CHANNEL_BUFFER_SIZE],
            decoder,
            defmt,
            defmt_buffered_bytes: 0,
        })
    }

    fn restart(&mut self, defmt: Option<&'table DefmtData>) -> Result<()> {
        *self = Self::new(self.spec, defmt)?;
        Ok(())
    }
}

struct SessionState {
    timestamps: bool,
    local_echo: bool,
    line_start: bool,
    started: Instant,
    started_wall: DateTime<Local>,
    defmt_decode_warnings: u64,
    color: bool,
    channel_labels: bool,
    last_channel: Option<u32>,
    streams: HashMap<u32, SessionStream>,
    foreground: Option<ForegroundLine>,
    interactive: bool,
}

struct ForegroundLine {
    channel: u32,
    bytes: Vec<u8>,
}

struct SessionStream {
    terminal: DecodedStream,
    raw_line: Vec<u8>,
    raw_pending_cr: bool,
    raw_rewrite: bool,
    raw_escape: Vec<u8>,
    raw_complete: Vec<(Vec<u8>, bool)>,
}

impl SessionStream {
    fn new() -> Self {
        Self {
            terminal: DecodedStream::new(),
            raw_line: Vec::new(),
            raw_pending_cr: false,
            raw_rewrite: false,
            raw_escape: Vec::new(),
            raw_complete: Vec::new(),
        }
    }

    fn consume(&mut self, bytes: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
        let terminal_complete = self.terminal.consume(bytes);
        self.consume_raw(bytes);
        let raw_complete = std::mem::take(&mut self.raw_complete);
        let complete = terminal_complete
            .into_iter()
            .enumerate()
            .map(|(index, mut line)| {
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                match raw_complete.get(index) {
                    Some((raw, false)) => raw.clone(),
                    _ => line,
                }
            })
            .collect();
        (complete, self.visible_line())
    }

    fn visible_line(&self) -> Vec<u8> {
        if self.raw_rewrite {
            self.terminal.visible_line()
        } else {
            self.raw_line.clone()
        }
    }

    fn consume_raw(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.raw_pending_cr {
                self.raw_pending_cr = false;
                if byte == b'\n' {
                    self.finish_raw_line();
                    continue;
                }
                self.raw_rewrite = true;
            }

            match byte {
                b'\r' => self.raw_pending_cr = true,
                b'\n' => self.finish_raw_line(),
                b'\x1b' => {
                    self.raw_escape.clear();
                    self.raw_escape.push(byte);
                    self.push_raw(byte);
                }
                byte => {
                    self.push_raw(byte);
                    self.update_escape(byte);
                }
            }
        }
    }

    fn update_escape(&mut self, byte: u8) {
        if self.raw_escape.is_empty() {
            return;
        }
        self.raw_escape.push(byte);
        if self.raw_escape.len() == 2 && byte != b'[' {
            self.raw_rewrite = true;
            self.raw_escape.clear();
        } else if self.raw_escape.len() >= 3 && (0x40..=0x7e).contains(&byte) {
            if !(self.raw_escape[1] == b'[' && byte == b'm') {
                self.raw_rewrite = true;
            }
            self.raw_escape.clear();
        }
    }

    fn finish_raw_line(&mut self) {
        self.raw_complete
            .push((std::mem::take(&mut self.raw_line), self.raw_rewrite));
        self.raw_pending_cr = false;
        self.raw_rewrite = false;
        self.raw_escape.clear();
    }

    fn push_raw(&mut self, byte: u8) {
        if self.raw_line.len() < MAX_SESSION_RAW_LINE_BYTES {
            self.raw_line.push(byte);
        } else {
            self.raw_rewrite = true;
        }
    }
}

struct DownBuffer {
    bytes: VecDeque<u8>,
    dropped: u64,
}

/// Bundled rendering targets passed through polling, commands, and reattach.
struct OutputContext<'a, W: Write> {
    state: &'a mut SessionState,
    logger: Option<&'a mut Logger>,
    filters: Option<&'a [Filter]>,
    output: &'a mut W,
}

impl DownBuffer {
    fn new() -> Self {
        Self {
            bytes: VecDeque::new(),
            dropped: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        let space = MAX_DOWN_BUFFER_BYTES.saturating_sub(self.bytes.len());
        if data.len() > space {
            let dropped = data.len() - space;
            self.dropped += dropped as u64;
            log::warn!("down channel buffer is full; dropping {dropped} byte(s)");
            self.bytes.extend(data[..space].iter().copied());
        } else {
            self.bytes.extend(data.iter().copied());
        }
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn clear(&mut self) {
        self.bytes.clear();
    }

    fn writable(&mut self) -> &mut [u8] {
        let (head, _) = self.bytes.as_mut_slices();
        head
    }

    fn consume(&mut self, count: usize) {
        self.bytes.drain(..count);
    }
}

impl SessionState {
    fn new() -> Self {
        Self {
            timestamps: false,
            local_echo: false,
            line_start: true,
            started: Instant::now(),
            started_wall: Local::now(),
            defmt_decode_warnings: 0,
            color: false,
            channel_labels: false,
            last_channel: None,
            streams: HashMap::new(),
            foreground: None,
            interactive: true,
        }
    }

    /// Clears all per-target rendering state after a target restart.
    fn reset_target(&mut self) {
        self.line_start = true;
        self.last_channel = None;
        self.streams.clear();
        self.foreground = None;
    }
}

fn finish_renderer_before_reset<W: Write>(
    render: &mut OutputContext<'_, W>,
) -> std::io::Result<()> {
    if render.state.interactive {
        erase_foreground(render.state, render.output)?;
        return Ok(());
    }

    let partials: Vec<_> = render
        .state
        .streams
        .iter()
        .filter_map(|(&channel, stream)| {
            let bytes = stream.visible_line();
            if bytes.is_empty() {
                None
            } else {
                Some((channel, bytes))
            }
        })
        .collect();
    for (channel, bytes) in partials {
        render_channel_bytes(
            &bytes,
            Some(channel),
            Instant::now(),
            render.state,
            render.output,
        )?;
        render.output.write_all(b"\n")?;
        render.state.line_start = true;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    Normal,
    AwaitingCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionCommand {
    Quit,
    Help,
    ShowConfig,
    ClearScreen,
    ToggleTimestamps,
    ToggleLocalEcho,
    ResetTarget,
}

#[derive(Debug, PartialEq, Eq)]
enum InputAction {
    Send(Vec<u8>),
    Command(SessionCommand),
    Ignore,
}

impl EscapeState {
    fn handle_key(self, key: KeyEvent) -> (Self, InputAction) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return (self, InputAction::Ignore);
        }

        match self {
            EscapeState::Normal if is_control_key(key, 't') => {
                (EscapeState::AwaitingCommand, InputAction::Ignore)
            }
            EscapeState::Normal => (EscapeState::Normal, key_to_action(key)),
            EscapeState::AwaitingCommand => {
                if is_control_key(key, 't') {
                    return (EscapeState::Normal, InputAction::Send(vec![0x14]));
                }

                match key.code {
                    KeyCode::Char('q') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::Quit),
                    ),
                    KeyCode::Char('?') => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::Help),
                    ),
                    KeyCode::Char('c') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ShowConfig),
                    ),
                    KeyCode::Char('l') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ClearScreen),
                    ),
                    KeyCode::Char('t') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ToggleTimestamps),
                    ),
                    KeyCode::Char('e') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ToggleLocalEcho),
                    ),
                    KeyCode::Char('R') => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ResetTarget),
                    ),
                    _ => (EscapeState::Normal, key_to_action(key)),
                }
            }
        }
    }
}

fn is_control_key(key: KeyEvent, character: char) -> bool {
    key.code == KeyCode::Char(character) && key.modifiers == KeyModifiers::CONTROL
}

fn key_to_action(key: KeyEvent) -> InputAction {
    let mut bytes = Vec::new();

    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if c.is_ascii_alphabetic() {
                bytes.push((c.to_ascii_lowercase() as u8) & 0x1f);
            }
        }
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::ALT) => {
            bytes.push(0x1b);
            bytes.extend_from_slice(c.to_string().as_bytes());
        }
        KeyCode::Char(c) => bytes.extend_from_slice(c.to_string().as_bytes()),
        KeyCode::Enter => bytes.push(b'\n'),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::Backspace => bytes.push(8u8),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        _ => {}
    }

    if bytes.is_empty() {
        InputAction::Ignore
    } else {
        InputAction::Send(bytes)
    }
}

fn channel_by_number<T: RttChannel>(channels: &mut [T], number: usize) -> Option<&mut T> {
    channels
        .iter_mut()
        .find(|channel| channel.number() == number)
}

fn validate_up_specs(rtt: &mut Rtt, specs: &[ChannelSpec]) -> Result<()> {
    for spec in specs {
        let channel = usize::try_from(spec.index).with_context(|| {
            format!(
                "up channel index {} cannot be represented on this host",
                spec.index
            )
        })?;

        if channel_by_number(rtt.up_channels(), channel).is_none() {
            bail!("Error: up channel {} does not exist.", spec.index);
        }
    }

    Ok(())
}

fn validate_channels(rtt: &mut Rtt, config: &SessionConfig) -> Result<()> {
    validate_up_specs(rtt, &config.up_specs)?;
    if let Some(down_channel) = config.down_channel {
        if channel_by_number(rtt.down_channels(), down_channel).is_none() {
            bail!("Error: down channel {down_channel} does not exist.");
        }
    }
    Ok(())
}

fn reset_and_reattach(
    core: &mut Core,
    rtt: &mut Rtt,
    scan_region: &ScanRegion,
    automatic_scan: bool,
) -> Result<()> {
    core.halt(TARGET_HALT_TIMEOUT)
        .context("Error halting target before reset")?;
    Rtt::clear_control_block(core, &ScanRegion::Exact(rtt.ptr()))
        .context("Error clearing stale RTT control block before reset")?;
    core.reset().context("Error resetting target")?;
    *rtt = if automatic_scan {
        try_attach_to_rtt_incremental(core, RTT_REATTACH_TIMEOUT, scan_region)
    } else {
        try_attach_to_rtt(core, RTT_REATTACH_TIMEOUT, scan_region)
    }
    .context("Error reattaching to RTT after target reset")?;
    Ok(())
}

/// Reattaches after the target restarted and RTT state changed under us.
fn reattach_after_target_restart<'table, W: Write>(
    core: &mut Core,
    rtt: &mut Rtt,
    config: &'table SessionConfig,
    up_readers: &mut [UpChannelReader<'table>],
    down_buf: &mut DownBuffer,
    render: &mut OutputContext<'_, W>,
) -> Result<()> {
    *rtt = if config.automatic_scan {
        try_attach_to_rtt_incremental(core, RTT_REATTACH_TIMEOUT, &config.scan_region)
    } else {
        try_attach_to_rtt(core, RTT_REATTACH_TIMEOUT, &config.scan_region)
    }
    .context("Error reattaching to RTT after target restart")?;
    validate_channels(rtt, config)?;
    for reader in up_readers {
        reader.restart(config.defmt.as_ref())?;
    }
    finish_renderer_before_reset(render)?;
    render.state.reset_target();
    down_buf.clear();
    if let Some(logger) = render.logger.as_deref_mut() {
        logger.reset()?;
    }
    if render.state.interactive {
        render
            .output
            .write_all(b"\r\nRTT control block changed; reattached to target.\r\n")?;
        render.output.flush()?;
    }
    Ok(())
}

#[derive(Default)]
struct PollStats {
    bytes: usize,
    messages: usize,
}

enum PollOutcome {
    Data(PollStats),
    Reattach,
}

fn process_defmt<'table, W: Write>(
    decoder: &mut Option<Box<dyn defmt_decoder::StreamDecoder + Send + Sync + 'table>>,
    defmt: Option<&'table DefmtData>,
    channel: u32,
    buffered_bytes: &mut usize,
    bytes: &[u8],
    locations: Option<&defmt_decoder::Locations>,
    render: &mut OutputContext<'_, W>,
) -> Result<bool> {
    if !bytes.is_empty() && buffered_bytes.saturating_add(bytes.len()) > MAX_DECODE_BUFFERED_BYTES {
        *decoder = Some(
            defmt
                .expect("defmt table missing")
                .table
                .new_stream_decoder(),
        );
        *buffered_bytes = 0;
        render_defmt_warning(
            channel,
            "defmt decoder input exceeded 64 KiB; resetting decoder",
            Instant::now(),
            render.state,
            render.logger.as_deref_mut(),
            render.output,
        )?;
    }

    let defmt = defmt.expect("defmt table missing");
    let can_recover = defmt.table.encoding().can_recover();
    let decoded = {
        let decoder = decoder.as_mut().expect("defmt decoder initialized");
        decode_frames(decoder.as_mut(), bytes, locations, can_recover)
    };
    for item in decoded.frames {
        match item {
            DecodeOutput::Frame(frame) => {
                render_defmt_frame(
                    channel,
                    &frame,
                    Instant::now(),
                    render.filters,
                    render.state,
                    render.logger.as_deref_mut(),
                    render.output,
                )?;
            }
            DecodeOutput::Warning(warning) => render_defmt_warning(
                channel,
                &warning,
                Instant::now(),
                render.state,
                render.logger.as_deref_mut(),
                render.output,
            )?,
        }
    }

    if decoded.restart {
        *decoder = Some(defmt.table.new_stream_decoder());
        *buffered_bytes = 0;
    } else {
        // Track all input since decoder creation. The decoder does not expose
        // its internal pending-byte count, so resetting only after a frame
        // could still allow an incomplete suffix to grow without a bound.
        *buffered_bytes = buffered_bytes.saturating_add(bytes.len());
    }
    Ok(decoded.hit_frame_limit)
}

fn poll_up_channels<'table, W: Write>(
    rtt: &mut Rtt,
    core: &mut Core,
    readers: &mut [UpChannelReader<'table>],
    locations: Option<&defmt_decoder::Locations>,
    render: &mut OutputContext<'_, W>,
) -> Result<PollOutcome> {
    let mut stats = PollStats::default();

    let mut made_progress;
    let mut budget = RTT_READ_BUDGET_PER_POLL;
    loop {
        made_progress = false;
        for reader in readers.iter_mut() {
            if budget == 0 {
                break;
            }
            let max = reader.buffer.len().min(budget);
            let count = match channel_by_number(rtt.up_channels(), reader.channel_number) {
                Some(channel) => match channel.read(core, &mut reader.buffer[..max]) {
                    Ok(count) => count,
                    Err(RttError::ReadPointerChanged) => return Ok(PollOutcome::Reattach),
                    Err(error) => {
                        return Err(anyhow::Error::from(error)).with_context(|| {
                            format!("Error reading from RTT up channel {}", reader.spec.index)
                        });
                    }
                },
                None => 0,
            };
            if count == 0 {
                if reader.spec.mode == ChannelEncoding::Defmt {
                    while process_defmt(
                        &mut reader.decoder,
                        reader.defmt,
                        reader.spec.index,
                        &mut reader.defmt_buffered_bytes,
                        &[],
                        locations,
                        render,
                    )? {}
                }
                continue;
            }

            made_progress = true;
            budget -= count;
            stats.bytes += count;
            if let Some(logger) = render.logger.as_deref_mut() {
                logger.write_raw(reader.spec.index, &reader.buffer[..count])?;
            }

            match reader.spec.mode {
                ChannelEncoding::Terminal => {
                    render_terminal_event(
                        reader.spec.index,
                        &reader.buffer[..count],
                        Instant::now(),
                        render.state,
                        render.logger.as_deref_mut(),
                        render.output,
                    )?;
                    stats.messages += 1;
                }
                ChannelEncoding::Defmt => {
                    let hit_frame_limit = process_defmt(
                        &mut reader.decoder,
                        reader.defmt,
                        reader.spec.index,
                        &mut reader.defmt_buffered_bytes,
                        &reader.buffer[..count],
                        locations,
                        render,
                    )?;
                    if hit_frame_limit {
                        while process_defmt(
                            &mut reader.decoder,
                            reader.defmt,
                            reader.spec.index,
                            &mut reader.defmt_buffered_bytes,
                            &[],
                            locations,
                            render,
                        )? {}
                    }
                }
            }
        }
        if !made_progress || budget == 0 {
            break;
        }
    }

    Ok(PollOutcome::Data(stats))
}

fn render_bytes(
    bytes: &[u8],
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    render_channel_bytes_colored(bytes, None, timestamp, state, output, None)
}

fn render_channel_bytes(
    bytes: &[u8],
    channel_idx: Option<u32>,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    render_channel_bytes_colored(bytes, channel_idx, timestamp, state, output, None)
}

fn render_channel_bytes_colored(
    bytes: &[u8],
    channel_idx: Option<u32>,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
    line_color: Option<&'static str>,
) -> std::io::Result<()> {
    render_channel_bytes_colored_inner(bytes, channel_idx, timestamp, state, output, line_color)?;
    Ok(())
}

fn erase_foreground(
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<Option<ForegroundLine>> {
    let foreground = state.foreground.take();
    if foreground.is_some() && state.interactive {
        output.write_all(b"\r\x1b[2K")?;
    }
    state.line_start = true;
    state.last_channel = None;
    Ok(foreground)
}

fn render_channel_bytes_colored_inner(
    bytes: &[u8],
    channel_idx: Option<u32>,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
    line_color: Option<&'static str>,
) -> std::io::Result<()> {
    for &byte in bytes {
        if byte == b'\r' {
            output.write_all(b"\r")?;
            state.line_start = true;
            continue;
        }
        let line_start = state.line_start;
        let channel_switch =
            channel_idx.is_some() && state.last_channel != channel_idx && state.channel_labels;
        if state.timestamps && line_start {
            let elapsed = timestamp.saturating_duration_since(state.started);
            let wall_timestamp = state.started_wall
                + chrono::Duration::from_std(elapsed).unwrap_or_else(|_| chrono::Duration::zero());
            write!(
                output,
                "[{}] ",
                wall_timestamp.format("%Y-%m-%d %H:%M:%S%.3f")
            )?;
        }

        if state.channel_labels && (line_start || channel_switch) {
            if let Some(channel_idx) = channel_idx {
                if state.color {
                    write!(output, "{}", channel_color(channel_idx))?;
                }
                write!(output, "[ch{channel_idx}] ")?;
                if state.color {
                    output.write_all(b"\x1b[0m")?;
                }
                state.last_channel = Some(channel_idx);
            }
        }

        if let Some(line_color) = line_color {
            if line_start || channel_switch {
                output.write_all(line_color.as_bytes())?;
            }
        }

        if byte == b'\n' {
            if state.interactive {
                output.write_all(b"\r")?;
            }
            state.line_start = true;
        } else {
            state.line_start = false;
        }
        output.write_all(&[byte])?;
        if byte == b'\n' && line_color.is_some() {
            output.write_all(b"\x1b[0m")?;
        }
    }

    if line_color.is_some() && !state.line_start {
        output.write_all(b"\x1b[0m")?;
    }

    Ok(())
}

fn render_terminal_chunk(
    channel: u32,
    bytes: &[u8],
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let complete = {
        let stream = state
            .streams
            .entry(channel)
            .or_insert_with(SessionStream::new);
        stream.consume(bytes)
    };

    let (complete, partial) = complete;
    for line in complete {
        let foreground = erase_foreground(state, output)?;
        render_channel_bytes(&line, Some(channel), timestamp, state, output)?;
        if state.interactive {
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
        if let Some(saved) = foreground {
            if saved.channel != channel {
                render_channel_bytes(&saved.bytes, Some(saved.channel), timestamp, state, output)?;
                state.foreground = Some(saved);
            }
        }
    }

    if state.interactive {
        let partial = (!partial.is_empty()).then_some(partial);
        if let Some(partial) = partial {
            if state.foreground.as_ref().map(|line| line.channel) == Some(channel) {
                erase_foreground(state, output)?;
            } else if state.foreground.is_some() {
                return Ok(());
            }
            render_channel_bytes(&partial, Some(channel), timestamp, state, output)?;
            state.foreground = Some(ForegroundLine {
                channel,
                bytes: partial,
            });
        }
    }
    Ok(())
}

fn render_complete_line(
    channel: u32,
    bytes: &[u8],
    timestamp: Instant,
    color: Option<&'static str>,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let foreground = erase_foreground(state, output)?;
    render_channel_bytes_colored(bytes, Some(channel), timestamp, state, output, color)?;
    if !state.line_start {
        if state.interactive {
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
    }
    if let Some(saved) = foreground {
        render_channel_bytes(&saved.bytes, Some(saved.channel), timestamp, state, output)?;
        state.foreground = Some(saved);
    }
    Ok(())
}

fn channel_color(channel_idx: u32) -> &'static str {
    [
        "\x1b[36m", "\x1b[35m", "\x1b[34m", "\x1b[32m", "\x1b[33m", "\x1b[31m",
    ][channel_idx as usize % 6]
}

fn render_terminal_event(
    channel: u32,
    bytes: &[u8],
    timestamp: Instant,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> std::io::Result<()> {
    if let Some(logger) = logger {
        logger
            .write_terminal(channel, bytes, state.channel_labels)
            .map_err(io_error)?;
    }
    render_terminal_chunk(channel, bytes, timestamp, state, output)
}

fn render_defmt_frame(
    channel: u32,
    frame: &DecodedFrame,
    timestamp: Instant,
    filters: Option<&[Filter]>,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> std::io::Result<()> {
    if let (Some(level), Some(filters)) = (frame.level, filters) {
        let minimum = filter_level(frame.module.as_deref(), filters);
        if !level_enabled(level, minimum) {
            return Ok(());
        }
    }
    let mut line = String::new();
    if let Some(timestamp) = &frame.timestamp {
        line.push('[');
        line.push_str(timestamp);
        line.push_str("] ");
    }
    if let Some(level) = frame.level {
        line.push_str(level_name(level));
        line.push(' ');
    }
    line.push_str(&frame.message);
    line.push('\n');
    if let Some(logger) = logger {
        logger
            .write_text(channel, line.as_bytes(), state.channel_labels)
            .map_err(io_error)?;
    }
    let level_color = if state.color {
        let color = defmt_level_color(frame.level);
        (!color.is_empty()).then_some(color)
    } else {
        None
    };
    render_complete_line(
        channel,
        line.as_bytes(),
        timestamp,
        level_color,
        state,
        output,
    )
}

fn render_defmt_warning(
    channel: u32,
    warning: &str,
    timestamp: Instant,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> std::io::Result<()> {
    state.defmt_decode_warnings += 1;
    let line = format!(
        "[defmt warning #{}] {warning}\n",
        state.defmt_decode_warnings
    );
    if let Some(logger) = logger {
        logger
            .write_text(channel, line.as_bytes(), state.channel_labels)
            .map_err(io_error)?;
    }
    render_complete_line(channel, line.as_bytes(), timestamp, None, state, output)
}

fn defmt_level_color(level: Option<defmt_parser::Level>) -> &'static str {
    match level {
        Some(defmt_parser::Level::Error) => "\x1b[31m",
        Some(defmt_parser::Level::Warn) => "\x1b[33m",
        Some(defmt_parser::Level::Debug | defmt_parser::Level::Trace) => "\x1b[2m",
        _ => "",
    }
}

fn io_error(error: anyhow::Error) -> std::io::Error {
    std::io::Error::other(error)
}

fn write_help(output: &mut impl Write) -> std::io::Result<()> {
    write!(
        output,
        "\r\nCtrl-T commands:\r\n  q  Quit\r\n  ?  Show this help\r\n  c  Show configuration\r\n  l  Clear screen\r\n  t  Toggle timestamps\r\n  e  Toggle local echo\r\n  R  Reset target\r\n  Ctrl-T  Send a literal Ctrl-T\r\n\r\nCtrl-C is sent to the target.\r\n\r\nNot implemented from tio (not applicable to RTT):\r\n  serial port settings, device auto-connect/reconnect, input/output hex modes,\r\n  output delays, character mapping, scripts, socket/exec redirection, RS-485,\r\n  connect alerts, and tio-specific log file options.\r\n"
    )?;
    output.flush()
}

fn write_session_banner(output: &mut impl Write) -> std::io::Result<()> {
    write!(
        output,
        "brtt {}\r\nPress ctrl-t ? for help\r\nConnected to target\r\n",
        env!("CARGO_PKG_VERSION")
    )?;
    output.flush()
}

fn write_config(
    config: &SessionConfig,
    state: &SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    write!(output, "\r\nConfiguration:\r\n")?;
    write!(output, "  Probe: {}\r\n", config.probe)?;
    write!(output, "  Chip: {}\r\n", config.chip)?;
    write!(output, "  Up channels:")?;
    for spec in &config.up_specs {
        write!(output, " {}:{}", spec.index, spec.mode.name())?;
    }
    write!(output, "\r\n")?;
    if let Some(down_channel) = config.down_channel {
        write!(output, "  Down channel: {down_channel}\r\n")?;
    } else {
        write!(output, "  Down channel: disabled\r\n")?;
    }
    write!(
        output,
        "  Poll interval: {} ms\r\n",
        config.poll_interval.as_millis()
    )?;
    write!(output, "  Timestamps: {}\r\n", on_or_off(state.timestamps))?;
    write!(output, "  Local echo: {}\r\n", on_or_off(state.local_echo))?;
    output.flush()
}

fn clear_screen(output: &mut impl Write) -> std::io::Result<()> {
    execute!(
        output,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    output.flush()
}

fn on_or_off(enabled: bool) -> &'static str {
    if enabled {
        "on"
    } else {
        "off"
    }
}

fn write_toggle_status(output: &mut impl Write, label: &str, enabled: bool) -> std::io::Result<()> {
    write!(output, "\r\n{label}: {}\r\n", on_or_off(enabled))
}

fn interactive_input_available(has_down_channel: bool) -> bool {
    has_down_channel && std::io::stdin().is_terminal()
}

fn dispatch_command<'table, W: Write>(
    command: SessionCommand,
    core: &mut Core,
    rtt: &mut Rtt,
    config: &'table SessionConfig,
    up_readers: &mut [UpChannelReader<'table>],
    down_buf: &mut DownBuffer,
    render: &mut OutputContext<'_, W>,
) -> Result<bool> {
    if command == SessionCommand::Quit {
        return Ok(true);
    }
    let restore_foreground = command != SessionCommand::ResetTarget;
    let suspended = erase_foreground(render.state, render.output)?;
    match command {
        SessionCommand::Quit => unreachable!("quit handled before rendering command output"),
        SessionCommand::Help => {
            write_help(render.output)?;
            render.state.line_start = true;
        }
        SessionCommand::ShowConfig => {
            write_config(config, render.state, render.output)?;
            render.state.line_start = true;
        }
        SessionCommand::ClearScreen => {
            clear_screen(render.output)?;
            render.state.line_start = true;
        }
        SessionCommand::ToggleTimestamps => {
            render.state.timestamps = !render.state.timestamps;
            write_toggle_status(render.output, "Timestamps", render.state.timestamps)?;
            render.output.flush()?;
            render.state.line_start = true;
        }
        SessionCommand::ToggleLocalEcho => {
            render.state.local_echo = !render.state.local_echo;
            write_toggle_status(render.output, "Local echo", render.state.local_echo)?;
            render.output.flush()?;
            render.state.line_start = true;
        }
        SessionCommand::ResetTarget => {
            reset_and_reattach(core, rtt, &config.scan_region, config.automatic_scan)?;
            validate_channels(rtt, config)?;
            for reader in up_readers {
                reader.restart(config.defmt.as_ref())?;
            }
            finish_renderer_before_reset(render)?;
            render.state.reset_target();
            down_buf.clear();
            if let Some(logger) = render.logger.as_deref_mut() {
                logger.reset()?;
            }
            write!(render.output, "\r\nTarget reset.\r\n")?;
            render.output.flush()?;
            render.state.line_start = true;
        }
    }

    if restore_foreground {
        if let Some(saved) = suspended {
            render.state.line_start = true;
            render.state.last_channel = None;
            render_channel_bytes_colored_inner(
                &saved.bytes,
                Some(saved.channel),
                Instant::now(),
                render.state,
                render.output,
                None,
            )?;
            render.state.foreground = Some(saved);
        }
    }

    Ok(false)
}

pub(crate) struct SessionConfig {
    pub(crate) probe: String,
    pub(crate) chip: String,
    pub(crate) up_specs: Vec<ChannelSpec>,
    pub(crate) down_channel: Option<usize>,
    pub(crate) poll_interval: Duration,
    pub(crate) reset: bool,
    pub(crate) timestamps: bool,
    pub(crate) defmt: Option<DefmtData>,
    pub(crate) defmt_filters: Option<Vec<Filter>>,
    pub(crate) color: crate::cli::ColorMode,
    pub(crate) log: Option<std::path::PathBuf>,
    pub(crate) log_per_channel: bool,
    pub(crate) log_format: crate::cli::LogFormat,
    pub(crate) scan_region: ScanRegion,
    pub(crate) automatic_scan: bool,
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

pub(crate) fn run_session(core: &mut Core, mut rtt: Rtt, config: SessionConfig) -> Result<()> {
    if config.reset {
        reset_and_reattach(core, &mut rtt, &config.scan_region, config.automatic_scan)?;
    }

    validate_channels(&mut rtt, &config)?;
    let defmt_ref = config.defmt.as_ref();
    let mut up_readers = config
        .up_specs
        .iter()
        .copied()
        .map(|spec| UpChannelReader::new(spec, defmt_ref))
        .collect::<Result<Vec<_>>>()?;

    let mut logger = Logger::new(
        config.log.as_deref(),
        config.log_per_channel,
        config.log_format,
        config.up_specs.len(),
    )?;

    let mut down_buf = DownBuffer::new();
    let mut escape_state = EscapeState::Normal;
    let mut state = SessionState::new();
    state.timestamps = config.timestamps;
    state.interactive = std::io::stdout().is_terminal();
    state.channel_labels = config.up_specs.len() > 1;
    state.color = match config.color {
        crate::cli::ColorMode::Always => true,
        crate::cli::ColorMode::Never => false,
        crate::cli::ColorMode::Auto => {
            std::io::IsTerminal::is_terminal(&std::io::stdout())
                && std::env::var_os("NO_COLOR").is_none()
        }
    };
    let stdin_setup = config.down_channel.is_some_and(|down_channel| {
        interactive_input_available(channel_by_number(rtt.down_channels(), down_channel).is_some())
    });
    if stdin_setup {
        // Ask the target shell to redraw its normal prompt at session start.
        down_buf.push(b"\n");
    }

    let _raw_mode = if stdin_setup {
        terminal::enable_raw_mode()?;
        Some(RawModeGuard)
    } else {
        None
    };

    let locations = config
        .defmt
        .as_ref()
        .and_then(|defmt| defmt.locations.as_ref());
    let filters = config.defmt_filters.as_deref();

    let mut output = BufWriter::new(stdout().lock());
    if state.interactive {
        write_session_banner(&mut output)?;
    }
    let result = 'read_loop: loop {
        let mut render = OutputContext {
            state: &mut state,
            logger: logger.as_mut(),
            filters,
            output: &mut output,
        };
        let stats = match poll_up_channels(&mut rtt, core, &mut up_readers, locations, &mut render)
        {
            Ok(PollOutcome::Data(stats)) => stats,
            Ok(PollOutcome::Reattach) => {
                if let Err(err) = reattach_after_target_restart(
                    core,
                    &mut rtt,
                    &config,
                    &mut up_readers,
                    &mut down_buf,
                    &mut render,
                ) {
                    break 'read_loop Err(err);
                }
                continue 'read_loop;
            }
            Err(err) => break 'read_loop Err(err),
        };

        let had_data = stats.bytes > 0 || stats.messages > 0;
        if had_data {
            if let Some(logger) = render.logger.as_deref_mut() {
                if let Err(err) = logger.flush_files() {
                    break 'read_loop Err(err);
                }
            }
            if let Err(err) = render.output.flush() {
                break 'read_loop Err(anyhow::anyhow!("Error writing to stdout: {err}"));
            }
        }

        if stdin_setup && down_buf.len() < MAX_DOWN_BUFFER_BYTES {
            let timeout = if had_data {
                Duration::ZERO
            } else {
                config.poll_interval
            };
            let input_ready = match event::poll(timeout) {
                Ok(ready) => ready,
                Err(err) => break 'read_loop Err(err.into()),
            };
            if input_ready {
                let event = match event::read() {
                    Ok(event) => event,
                    Err(err) => break 'read_loop Err(err.into()),
                };
                if let Event::Key(key_event) = event {
                    let (next_state, action) = escape_state.handle_key(key_event);
                    escape_state = next_state;

                    match action {
                        InputAction::Send(bytes) => {
                            if render.state.local_echo {
                                if let Err(err) = render_bytes(
                                    &bytes,
                                    Instant::now(),
                                    render.state,
                                    render.output,
                                ) {
                                    break 'read_loop Err(anyhow::anyhow!(
                                        "Error writing local echo: {err}"
                                    ));
                                }
                                if let Err(err) = render.output.flush() {
                                    break 'read_loop Err(anyhow::anyhow!(
                                        "Error writing to stdout: {err}"
                                    ));
                                }
                            }
                            down_buf.push(&bytes);
                        }
                        InputAction::Command(command) => {
                            match dispatch_command(
                                command,
                                core,
                                &mut rtt,
                                &config,
                                &mut up_readers,
                                &mut down_buf,
                                &mut render,
                            ) {
                                Ok(true) => break 'read_loop Ok(()),
                                Ok(false) => {
                                    if let Err(err) = render.output.flush() {
                                        break 'read_loop Err(anyhow::anyhow!(
                                            "Error writing to stdout: {err}"
                                        ));
                                    }
                                }
                                Err(err) => break 'read_loop Err(err),
                            }
                        }
                        InputAction::Ignore => {}
                    }
                }
            }
        } else if !had_data {
            std::thread::sleep(config.poll_interval);
        }

        if let Some(down_channel_number) = config.down_channel {
            if let Some(down_channel) = channel_by_number(rtt.down_channels(), down_channel_number)
            {
                if !down_buf.is_empty() {
                    let count = match down_channel.write(core, down_buf.writable()) {
                        Ok(count) => count,
                        Err(err) => {
                            break 'read_loop Err(anyhow::anyhow!("\nError writing to RTT: {err}"));
                        }
                    };

                    if count > 0 {
                        down_buf.consume(count);
                    }
                }
            }
        }
    };

    if let Some(logger) = &mut logger {
        logger.flush()?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    struct TestChannel(usize);

    impl RttChannel for TestChannel {
        fn number(&self) -> usize {
            self.0
        }

        fn name(&self) -> Option<&str> {
            None
        }

        fn buffer_size(&self) -> usize {
            0
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn channel_lookup_uses_rtt_number_not_slice_index() {
        let mut channels = [TestChannel(1), TestChannel(3)];

        assert_eq!(channel_by_number(&mut channels, 1).unwrap().number(), 1);
        assert_eq!(channel_by_number(&mut channels, 3).unwrap().number(), 3);
        assert!(channel_by_number(&mut channels, 0).is_none());
        assert!(channel_by_number(&mut channels, 2).is_none());
    }

    #[test]
    fn ctrl_t_enters_command_mode_without_sending() {
        let (state, action) =
            EscapeState::Normal.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));

        assert_eq!(state, EscapeState::AwaitingCommand);
        assert_eq!(action, InputAction::Ignore);
    }

    #[test]
    fn ctrl_t_q_quits() {
        let (state, action) =
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE));

        assert_eq!(state, EscapeState::Normal);
        assert_eq!(action, InputAction::Command(SessionCommand::Quit));
    }

    #[test]
    fn ctrl_t_question_requests_help() {
        let (state, action) =
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('?'), KeyModifiers::SHIFT));

        assert_eq!(state, EscapeState::Normal);
        assert_eq!(action, InputAction::Command(SessionCommand::Help));
    }

    #[test]
    fn ctrl_t_core_commands_dispatch_to_their_commands() {
        for (character, command) in [
            ('c', SessionCommand::ShowConfig),
            ('l', SessionCommand::ClearScreen),
            ('t', SessionCommand::ToggleTimestamps),
            ('e', SessionCommand::ToggleLocalEcho),
        ] {
            assert_eq!(
                EscapeState::AwaitingCommand
                    .handle_key(key(KeyCode::Char(character), KeyModifiers::NONE)),
                (EscapeState::Normal, InputAction::Command(command))
            );
        }

        assert_eq!(
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('R'), KeyModifiers::SHIFT)),
            (
                EscapeState::Normal,
                InputAction::Command(SessionCommand::ResetTarget)
            )
        );
    }

    #[test]
    fn ctrl_t_ctrl_t_sends_literal_ctrl_t() {
        let (state, action) =
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));

        assert_eq!(state, EscapeState::Normal);
        assert_eq!(action, InputAction::Send(vec![0x14]));
    }

    #[test]
    fn ctrl_c_is_forwarded_to_the_target() {
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            (EscapeState::Normal, InputAction::Send(vec![0x03]))
        );
    }

    #[test]
    fn ordinary_keys_keep_existing_encoding() {
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            (EscapeState::Normal, InputAction::Send(b"x".to_vec()))
        );
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            (EscapeState::Normal, InputAction::Send(vec![b'\n']))
        );
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Up, KeyModifiers::NONE)),
            (EscapeState::Normal, InputAction::Send(b"\x1b[A".to_vec()))
        );
    }

    #[test]
    fn control_and_alt_keys_keep_terminal_encoding() {
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            (EscapeState::Normal, InputAction::Send(vec![1]))
        );
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            (EscapeState::Normal, InputAction::Send(vec![21]))
        );
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('b'), KeyModifiers::ALT)),
            (EscapeState::Normal, InputAction::Send(b"\x1bb".to_vec()))
        );
    }

    #[test]
    fn repeated_keys_are_forwarded() {
        let mut repeated = key(KeyCode::Char('x'), KeyModifiers::NONE);
        repeated.kind = KeyEventKind::Repeat;

        assert_eq!(
            EscapeState::Normal.handle_key(repeated),
            (EscapeState::Normal, InputAction::Send(b"x".to_vec()))
        );
    }

    #[test]
    fn unknown_command_keys_pass_through_and_reset_state() {
        assert_eq!(
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            (EscapeState::Normal, InputAction::Send(b"x".to_vec()))
        );
    }

    #[test]
    fn key_releases_do_not_change_escape_state() {
        let mut released = key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        released.kind = KeyEventKind::Release;

        assert_eq!(
            EscapeState::Normal.handle_key(released),
            (EscapeState::Normal, InputAction::Ignore)
        );
    }

    #[test]
    fn help_lists_current_commands() {
        let mut output = Vec::new();

        write_help(&mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("q  Quit"));
        assert!(output.contains("?  Show this help"));
        assert!(output.contains("c  Show configuration"));
        assert!(output.contains("l  Clear screen"));
        assert!(output.contains("t  Toggle timestamps"));
        assert!(output.contains("e  Toggle local echo"));
        assert!(output.contains("R  Reset target"));
        assert!(output.contains("Ctrl-C is sent to the target"));
        assert!(output.contains("Not implemented from tio"));
    }

    #[test]
    fn session_banner_shows_escape_help() {
        let mut output = Vec::new();

        write_session_banner(&mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Press ctrl-t ? for help"));
        assert!(output.contains("Connected to target"));
    }

    #[test]
    fn timestamps_are_added_once_per_line_across_partial_events() {
        let mut state = SessionState::new();
        state.timestamps = true;
        let timestamp = state.started + Duration::from_millis(123);
        let mut output = Vec::new();

        render_bytes(b"partial", timestamp, &mut state, &mut output).unwrap();
        render_bytes(b" line\nnext", timestamp, &mut state, &mut output).unwrap();

        let expected_timestamp = (state.started_wall + chrono::Duration::milliseconds(123))
            .format("%Y-%m-%d %H:%M:%S%.3f")
            .to_string();
        let expected =
            format!("[{expected_timestamp}] partial line\r\n[{expected_timestamp}] next");
        assert_eq!(output, expected.as_bytes());
    }

    #[test]
    fn timestamps_are_disabled_by_default() {
        let mut state = SessionState::new();
        let mut output = Vec::new();

        render_bytes(b"text\n", Instant::now(), &mut state, &mut output).unwrap();

        assert_eq!(output, b"text\r\n");
    }

    #[test]
    fn redirected_terminal_output_buffers_fragments_until_a_complete_line() {
        let mut state = SessionState::new();
        state.interactive = false;
        let mut output = Vec::new();
        let timestamp = Instant::now();

        render_terminal_chunk(0, b"partial ", timestamp, &mut state, &mut output).unwrap();
        assert!(output.is_empty());

        render_terminal_chunk(0, b"line\n", timestamp, &mut state, &mut output).unwrap();

        assert_eq!(output, b"partial line\n");
    }

    #[test]
    fn bare_carriage_return_overwrites_from_column_zero() {
        let mut state = SessionState::new();
        state.interactive = false;
        let mut output = Vec::new();

        render_terminal_chunk(0, b"abcdef\rxy\n", Instant::now(), &mut state, &mut output).unwrap();

        assert_eq!(output, b"xycdef\n");
    }

    #[test]
    fn toggle_status_returns_cursor_to_column_zero() {
        let mut output = Vec::new();

        write_toggle_status(&mut output, "Timestamps", true).unwrap();

        assert_eq!(output, b"\r\nTimestamps: on\r\n");
    }

    #[test]
    fn carriage_return_newline_is_not_rendered_as_two_lines() {
        let mut state = SessionState::new();
        state.channel_labels = true;
        let mut output = Vec::new();

        render_terminal_chunk(
            0,
            b"first\r\nsecond\r\n",
            Instant::now(),
            &mut state,
            &mut output,
        )
        .unwrap();

        assert_eq!(output, b"[ch0] first\r\n[ch0] second\r\n");
    }

    #[test]
    fn completed_line_does_not_restore_its_consumed_partial_prompt() {
        let mut state = SessionState::new();
        state.channel_labels = true;
        let mut output = Vec::new();

        render_terminal_chunk(0, b"> ", Instant::now(), &mut state, &mut output).unwrap();
        render_terminal_chunk(0, b"help\r\n", Instant::now(), &mut state, &mut output).unwrap();

        assert_eq!(output, b"[ch0] > \r\x1b[2K[ch0] > help\r\n");
    }

    #[test]
    fn config_and_clear_screen_outputs_include_session_settings() {
        let config = SessionConfig {
            probe: "probe-id".to_string(),
            chip: "nRF52840_xxAA".to_string(),
            up_specs: vec![ChannelSpec {
                index: 2,
                mode: ChannelEncoding::Terminal,
            }],
            down_channel: Some(1),
            poll_interval: Duration::from_millis(10),
            reset: false,
            timestamps: false,
            defmt: None,
            defmt_filters: None,
            color: crate::cli::ColorMode::Never,
            log: None,
            log_per_channel: false,
            log_format: crate::cli::LogFormat::Decoded,
            scan_region: ScanRegion::Exact(0x2000_0000),
            automatic_scan: false,
        };
        let state = SessionState::new();
        let mut output = Vec::new();

        write_config(&config, &state, &mut output).unwrap();
        let config_output = String::from_utf8(output).unwrap();
        assert!(config_output.contains("Probe: probe-id"));
        assert!(config_output.contains("Chip: nRF52840_xxAA"));
        assert!(config_output.contains("Up channels: 2:terminal"));
        assert!(config_output.contains("Down channel: 1"));
        assert!(config_output.contains("Poll interval: 10 ms"));

        let mut clear_output = Vec::new();
        clear_screen(&mut clear_output).unwrap();
        assert!(clear_output.starts_with(b"\x1b[2J"));
    }

    #[test]
    fn terminal_chunks_are_rendered_with_channel_labels_in_read_order() {
        let mut output = Vec::new();
        let mut state = SessionState::new();
        state.channel_labels = true;
        let timestamp = Instant::now();

        render_terminal_chunk(2, b"log\n", timestamp, &mut state, &mut output).unwrap();
        render_terminal_chunk(0, b"shell", timestamp, &mut state, &mut output).unwrap();

        assert_eq!(output, b"[ch2] log\r\n[ch0] shell");
    }

    #[test]
    fn multiple_channels_are_labeled_on_each_line() {
        let mut output = Vec::new();
        let mut state = SessionState::new();
        state.channel_labels = true;
        let timestamp = Instant::now();

        render_terminal_chunk(0, b"zero\none", timestamp, &mut state, &mut output).unwrap();
        render_terminal_chunk(1, b"one\n", timestamp, &mut state, &mut output).unwrap();

        assert_eq!(
            output,
            b"[ch0] zero\r\n[ch0] one\r\x1b[2K[ch1] one\r\n[ch0] one"
        );
    }

    #[test]
    fn channel_labels_use_stable_palette_colors() {
        assert_eq!(channel_color(0), "\x1b[36m");
        assert_eq!(channel_color(6), channel_color(0));

        let mut output = Vec::new();
        let mut state = SessionState::new();
        state.channel_labels = true;
        state.color = true;

        render_terminal_chunk(1, b"line\n", Instant::now(), &mut state, &mut output).unwrap();

        assert_eq!(output, b"\x1b[35m[ch1] \x1b[0mline\r\n");
    }

    #[test]
    fn defmt_level_color_composes_after_channel_color() {
        let frame = DecodedFrame {
            message: "bad".to_string(),
            timestamp: None,
            level: Some(defmt_parser::Level::Error),
            module: None,
        };
        let mut output = Vec::new();
        let mut state = SessionState::new();
        state.channel_labels = true;
        state.color = true;

        render_defmt_frame(
            1,
            &frame,
            Instant::now(),
            None,
            &mut state,
            None,
            &mut output,
        )
        .unwrap();

        assert_eq!(output, b"\x1b[35m[ch1] \x1b[0m\x1b[31merror bad\r\n\x1b[0m");
    }

    #[test]
    fn filtered_defmt_frames_are_not_rendered_or_logged() {
        let frame = DecodedFrame {
            message: "quiet".to_string(),
            timestamp: None,
            level: Some(defmt_parser::Level::Info),
            module: Some("app".to_string()),
        };
        let filters = vec![Filter {
            module: "".into(),
            level: defmt_parser::Level::Warn,
        }];
        let mut output = Vec::new();
        let mut state = SessionState::new();

        render_defmt_frame(
            0,
            &frame,
            Instant::now(),
            Some(&filters),
            &mut state,
            None,
            &mut output,
        )
        .unwrap();

        assert!(output.is_empty());
        assert_eq!(state.defmt_decode_warnings, 0);
    }

    #[test]
    fn reset_target_clears_renderer_state_and_queued_input() {
        let mut state = SessionState::new();
        state.line_start = false;
        state.last_channel = Some(1);
        state.foreground = Some(ForegroundLine {
            channel: 1,
            bytes: b"> ".to_vec(),
        });
        state.streams.insert(1, SessionStream::new());

        state.reset_target();

        assert!(state.line_start);
        assert_eq!(state.last_channel, None);
        assert!(state.foreground.is_none());
        assert!(state.streams.is_empty());

        let mut down = DownBuffer::new();
        down.push(b"typed but unsent");
        down.clear();
        assert!(down.is_empty());
    }

    #[test]
    fn down_buffer_caps_growth_and_drops_excess() {
        let mut down = DownBuffer::new();
        let chunk = vec![b'x'; MAX_DOWN_BUFFER_BYTES + 32];

        down.push(&chunk);

        assert_eq!(down.bytes.len(), MAX_DOWN_BUFFER_BYTES);
        assert_eq!(down.dropped, 32);
        assert_eq!(down.writable().len(), MAX_DOWN_BUFFER_BYTES);

        down.consume(16);
        assert_eq!(down.bytes.len(), MAX_DOWN_BUFFER_BYTES - 16);
    }

    #[test]
    fn terminal_lines_are_bounded_instead_of_growing_without_bound() {
        let mut state = SessionState::new();
        state.interactive = false;
        let mut output = Vec::new();
        let long = vec![b'a'; 16 * 1024 + 64];

        render_terminal_chunk(0, &long, Instant::now(), &mut state, &mut output).unwrap();
        render_terminal_chunk(0, b"\n", Instant::now(), &mut state, &mut output).unwrap();

        assert_eq!(output.len(), 4097);
    }

    #[test]
    fn terminal_output_preserves_sgr_colors_without_cursor_rewrites() {
        let mut state = SessionState::new();
        let mut output = Vec::new();

        render_terminal_chunk(
            0,
            b"\x1b[31mred\x1b[0m\n",
            Instant::now(),
            &mut state,
            &mut output,
        )
        .unwrap();

        assert_eq!(output, b"\x1b[31mred\x1b[0m\r\n");
    }
}
