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
    let expected = format!("[{expected_timestamp}] partial line\r\n[{expected_timestamp}] next");
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
        message: "bad".into(),
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
        message: "quiet".into(),
        timestamp: None,
        level: Some(defmt_parser::Level::Info),
        module: Some("app"),
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
fn scan_region_prefers_elf_symbol_over_explicit_region() {
    let elf = ScanRegion::Exact(0x1000);
    let requested = ScanRegion::Exact(0x2000);
    let target_default = ScanRegion::Exact(0x3000);

    let (region, automatic) = resolve_scan_region(Some(&elf), Some(&requested), &target_default);

    assert!(matches!(region, ScanRegion::Exact(0x1000)));
    assert!(!automatic);
}

#[test]
fn scan_region_uses_explicit_region_when_elf_absent() {
    let requested = ScanRegion::range(0x1000..0x2000);
    let target_default = ScanRegion::Exact(0x3000);

    let (region, automatic) = resolve_scan_region(None, Some(&requested), &target_default);

    assert!(matches!(region, ScanRegion::Ranges(_)));
    assert!(!automatic);
}

#[test]
fn scan_region_falls_back_to_target_default_with_automatic_scan() {
    let target_default = ScanRegion::Exact(0x3000);

    let (region, automatic) = resolve_scan_region(None, None, &target_default);

    assert!(matches!(region, ScanRegion::Exact(0x3000)));
    assert!(automatic);
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
