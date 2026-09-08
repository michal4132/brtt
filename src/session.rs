use crate::cli::{ChannelMode, ChannelSpec};
use anyhow::{bail, Context, Result};
use brtt::rtt::Rtt;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use probe_rs::Core;
use std::io::prelude::*;
use std::io::stdout;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct ChannelEvent {
    channel_idx: u32,
    mode: ChannelMode,
    bytes: Vec<u8>,
    timestamp: Instant,
}

struct UpChannelReader {
    spec: ChannelSpec,
    buffer: [u8; 128],
}

impl UpChannelReader {
    fn new(spec: ChannelSpec) -> Self {
        Self {
            spec,
            buffer: [0; 128],
        }
    }
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
}

#[derive(Debug, PartialEq, Eq)]
enum InputAction {
    Send(Vec<u8>),
    Command(SessionCommand),
    Ignore,
}

impl EscapeState {
    fn handle_key(self, key: KeyEvent) -> (Self, InputAction) {
        if key.kind != KeyEventKind::Press {
            return (self, InputAction::Ignore);
        }

        if is_control_key(key, 'c') {
            return (
                EscapeState::Normal,
                InputAction::Command(SessionCommand::Quit),
            );
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

fn validate_channel_mode(spec: &ChannelSpec) -> Result<()> {
    if spec.mode == ChannelMode::Defmt {
        bail!(
            "Defmt output for up channel {} is not implemented yet; use :ascii or :ascii",
            spec.index
        );
    }

    Ok(())
}

fn validate_up_specs(rtt: &mut Rtt, specs: &[ChannelSpec]) -> Result<()> {
    for spec in specs {
        validate_channel_mode(spec)?;

        let channel = usize::try_from(spec.index).with_context(|| {
            format!(
                "up channel index {} cannot be represented on this host",
                spec.index
            )
        })?;

        if rtt.up_channel(channel).is_none() {
            bail!("Error: up channel {} does not exist.", spec.index);
        }
    }

    Ok(())
}

fn poll_up_channels(
    rtt: &mut Rtt,
    core: &mut Core,
    readers: &mut [UpChannelReader],
) -> Result<Vec<ChannelEvent>> {
    let mut events = Vec::with_capacity(readers.len());

    for reader in readers {
        let channel = usize::try_from(reader.spec.index).with_context(|| {
            format!(
                "up channel index {} cannot be represented on this host",
                reader.spec.index
            )
        })?;

        let count = match rtt.up_channel(channel) {
            Some(channel) => channel.read(core, &mut reader.buffer)?,
            None => continue,
        };

        if count > 0 {
            events.push(ChannelEvent {
                channel_idx: reader.spec.index,
                mode: reader.spec.mode,
                bytes: reader.buffer[..count].to_vec(),
                timestamp: Instant::now(),
            });
        }
    }

    events.sort_by_key(|event| event.timestamp);
    Ok(events)
}

fn render_events(events: &[ChannelEvent], output: &mut impl Write) -> std::io::Result<()> {
    for event in events {
        match event.mode {
            ChannelMode::Ascii | ChannelMode::Ascii => {
                for &byte in &event.bytes {
                    if byte == b'\n' {
                        output.write_all(b"\r")?;
                    }
                    output.write_all(&[byte])?;
                }
            }
            ChannelMode::Defmt => unreachable!(
                "defmt event from channel {} was not rejected before polling",
                event.channel_idx
            ),
        }
    }

    output.flush()
}

fn write_help(output: &mut impl Write) -> std::io::Result<()> {
    writeln!(output, "\r\nCtrl-T commands:")?;
    writeln!(output, "  q  Quit")?;
    writeln!(output, "  ?  Show this help")?;
    writeln!(output, "  Ctrl-T  Send a literal Ctrl-T")?;
    output.flush()
}

pub(crate) struct SessionConfig {
    pub(crate) up_specs: Vec<ChannelSpec>,
    pub(crate) up_configured: bool,
    pub(crate) down_channel: usize,
    pub(crate) down_configured: bool,
    pub(crate) reset: bool,
}

pub(crate) fn run_session(core: &mut Core, mut rtt: Rtt, config: SessionConfig) -> Result<()> {
    if config.up_configured {
        validate_up_specs(&mut rtt, &config.up_specs)?;
    }
    let mut up_readers = config
        .up_specs
        .into_iter()
        .map(UpChannelReader::new)
        .collect::<Vec<_>>();

    if config.down_configured && rtt.down_channel(config.down_channel).is_none() {
        bail!(
            "Error: down channel {} does not exist.",
            config.down_channel
        );
    }

    if config.reset {
        core.reset()?;
    }

    let mut down_buf = Vec::new();
    let mut escape_state = EscapeState::Normal;
    let stdin_setup = rtt.down_channel(config.down_channel).is_some();

    if stdin_setup {
        terminal::enable_raw_mode()?;
    }

    let mut output = stdout();
    let result = 'read_loop: loop {
        let events = match poll_up_channels(&mut rtt, core, &mut up_readers) {
            Ok(events) => events,
            Err(err) => {
                break 'read_loop Err(anyhow::anyhow!("\nError reading from RTT: {err}"));
            }
        };

        if !events.is_empty() {
            if let Err(err) = render_events(&events, &mut output) {
                break 'read_loop Err(anyhow::anyhow!("Error writing to stdout: {err}"));
            }
        }

        if stdin_setup && event::poll(Duration::from_millis(0))? {
            if let Event::Key(key_event) = event::read()? {
                let (next_state, action) = escape_state.handle_key(key_event);
                escape_state = next_state;

                match action {
                    InputAction::Send(bytes) => down_buf.extend_from_slice(&bytes),
                    InputAction::Command(SessionCommand::Help) => {
                        if let Err(err) = write_help(&mut output) {
                            break 'read_loop Err(anyhow::anyhow!(
                                "Error writing command help: {err}"
                            ));
                        }
                    }
                    InputAction::Command(SessionCommand::Quit) => break 'read_loop Ok(()),
                    InputAction::Ignore => {}
                }
            }
        }

        if let Some(down_channel) = rtt.down_channel(config.down_channel) {
            if !down_buf.is_empty() {
                let count = match down_channel.write(core, down_buf.as_mut()) {
                    Ok(count) => count,
                    Err(err) => {
                        break 'read_loop Err(anyhow::anyhow!("\nError writing to RTT: {err}"));
                    }
                };

                if count > 0 {
                    down_buf.drain(..count);
                }
            }
        }
    };

    if stdin_setup {
        terminal::disable_raw_mode()?;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
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
    fn ctrl_t_ctrl_t_sends_literal_ctrl_t() {
        let (state, action) =
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));

        assert_eq!(state, EscapeState::Normal);
        assert_eq!(action, InputAction::Send(vec![0x14]));
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
    }

    #[test]
    fn channel_events_keep_tags_and_render_in_order() {
        let events = vec![
            ChannelEvent {
                channel_idx: 2,
                mode: ChannelMode::Ascii,
                bytes: b"log\n".to_vec(),
                timestamp: Instant::now(),
            },
            ChannelEvent {
                channel_idx: 0,
                mode: ChannelMode::Ascii,
                bytes: b"shell".to_vec(),
                timestamp: Instant::now(),
            },
        ];
        let mut output = Vec::new();

        render_events(&events, &mut output).unwrap();

        assert_eq!(events[0].channel_idx, 2);
        assert_eq!(events[1].channel_idx, 0);
        assert_eq!(output, b"log\r\nshell");
    }
}
