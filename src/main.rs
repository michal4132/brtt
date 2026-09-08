use brtt::channel::RttChannel;
use brtt::rtt::{Rtt, ScanRegion};

use probe_rs::{
    config::TargetSelector, probe::list::Lister, probe::DebugProbeInfo, Core, Permissions,
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;
use std::io::prelude::*;
use std::io::stdout;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq, Clone)]
enum ProbeInfo {
    Number(usize),
    List,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ChannelMode {
    Ascii,
    Defmt,
}

impl std::str::FromStr for ChannelMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ascii" => Ok(ChannelMode::Ascii),
            "defmt" => Ok(ChannelMode::Defmt),
            _ => Err(format!(
                "invalid channel mode '{value}', expected ascii or defmt"
            )),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct ChannelSpec {
    index: u32,
    mode: ChannelMode,
}

impl std::str::FromStr for ChannelSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split(':');
        let index = parts.next().unwrap_or_default();
        let mode = parts.next().unwrap_or("ascii");

        if parts.next().is_some() {
            return Err(format!(
                "invalid channel specification '{value}', expected INDEX[:MODE]"
            ));
        }

        if index.is_empty() {
            return Err("channel index cannot be empty".to_string());
        }

        let index = index
            .parse::<u32>()
            .map_err(|_| format!("invalid channel index '{index}', expected a u32"))?;
        let mode = mode.parse()?;

        Ok(ChannelSpec { index, mode })
    }
}

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

impl std::str::FromStr for ProbeInfo {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<ProbeInfo, &'static str> {
        if s == "list" {
            Ok(ProbeInfo::List)
        } else if let Ok(n) = s.parse::<usize>() {
            Ok(ProbeInfo::Number(n))
        } else {
            Err("Invalid probe number.")
        }
    }
}

fn parse_scan_region(
    mut src: &str,
) -> Result<ScanRegion, Box<dyn std::error::Error + Send + Sync + 'static>> {
    src = src.trim();
    if src.is_empty() {
        return Ok(ScanRegion::Ram);
    }

    let parts = src
        .split("..")
        .map(|p| {
            if p.starts_with("0x") || p.starts_with("0X") {
                u64::from_str_radix(&p[2..], 16)
            } else {
                p.parse()
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    match *parts.as_slice() {
        [addr] => Ok(ScanRegion::Exact(addr)),
        [start, end] => Ok(ScanRegion::range(start..end)),
        _ => Err("Invalid range: multiple '..'s".into()),
    }
}

#[derive(Debug, clap::Parser)]
#[clap(
    name = "brtt",
    about = "Better RTT (Real-Time Transfer) client",
    version = clap::crate_version!(),
)]
struct Opts {
    #[clap(
        short,
        long,
        default_value = "0",
        help = "Specify probe number or 'list' to list probes."
    )]
    probe: ProbeInfo,

    #[clap(
        short,
        long,
        help = "Target chip type. Leave unspecified to auto-detect."
    )]
    chip: Option<String>,

    #[clap(short, long, help = "List RTT channels and exit.")]
    list: bool,

    #[clap(
        short,
        long,
        action = clap::ArgAction::Append,
        value_name = "CHANNEL[:MODE]",
        help = "Up channel specification. MODE is ascii or defmt; defaults to ascii. May be repeated; defmt decoding is not implemented yet."
    )]
    up: Vec<ChannelSpec>,

    #[clap(
        short,
        long,
        action = clap::ArgAction::Append,
        value_name = "CHANNEL[:MODE]",
        help = "Down channel for keyboard input. Defaults to channel 0."
    )]
    down: Option<u32>,

    #[clap(short, long, help = "Reset the target after RTT session was opened")]
    reset: bool,

    #[clap(
        long,
        default_value="",
        value_parser = parse_scan_region,
        help = "Memory region to scan for control block. You can specify either an exact starting address '0x1000' or a range such as '0x0000..0x1000'. Both decimal and hex are accepted.")]
    scan_region: ScanRegion,
}

fn selected_channel(specs: &[ChannelSpec], direction: &str) -> Result<usize> {
    let spec = match specs {
        [] => return Ok(0),
        [spec] => spec,
        _ => bail!(
            "Multiple {direction} channels are not supported yet; use only one specification."
        ),
    };

    usize::try_from(spec.index).with_context(|| {
        format!(
            "{direction} channel index {} cannot be represented on this host",
            spec.index
        )
    })
}

fn configured_up_specs(specs: &[ChannelSpec]) -> Vec<ChannelSpec> {
    if specs.is_empty() {
        vec![ChannelSpec {
            index: 0,
            mode: ChannelMode::Ascii,
        }]
    } else {
        specs.to_vec()
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
            ChannelMode::Ascii => {
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

fn main() -> Result<()> {
    env_logger::init();
    let opts = Opts::parse();

    let lister = Lister::new();

    let probes = lister.list_all();

    if probes.is_empty() {
        bail!(
            "No debug probes available. Make sure your probe is plugged in, supported and up-to-date."
        );
    }

    let probe_number = match opts.probe {
        ProbeInfo::List => {
            list_probes(std::io::stdout(), &probes);
            return Ok(());
        }
        ProbeInfo::Number(i) => i,
    };

    if probe_number >= probes.len() {
        list_probes(std::io::stderr(), &probes);
        bail!("Probe {probe_number} does not exist.");
    }

    let probe = match probes[probe_number].open() {
        Ok(probe) => probe,
        Err(err) => {
            bail!("Error opening probe: {err}");
        }
    };

    let target_selector = TargetSelector::from(opts.chip.as_deref());

    let mut session = match probe.attach(target_selector, Permissions::default()) {
        Ok(session) => session,
        Err(err) => {
            let mut err_str = format!("Error creating debug session: {err}");

            if opts.chip.is_none() {
                if let probe_rs::Error::ChipNotFound(_) = err {
                    err_str
                        .push_str("\nHint: Use '--chip' to specify the target chip type manually");
                }
            }

            bail!("{err}");
        }
    };

    let mut core = session.core(0).context("Error attaching to core # 0")?;

    eprintln!("Attaching to RTT...");

    let mut rtt =
        Rtt::attach_region(&mut core, &opts.scan_region).context("Error attaching to RTT")?;
    eprintln!("Found control block at {:#010x}", rtt.ptr());

    if opts.list {
        println!("Up channels:");
        list_channels(rtt.up_channels());

        println!("Down channels:");
        list_channels(rtt.down_channels());

        return Ok(());
    }

    let up_specs = configured_up_specs(&opts.up);
    if !opts.up.is_empty() {
        validate_up_specs(&mut rtt, &up_specs)?;
    }
    let mut up_readers = up_specs
        .into_iter()
        .map(UpChannelReader::new)
        .collect::<Vec<_>>();

    let down_channel = opts.down.unwrap_or(0) as usize;
    if opts.down.is_some() && rtt.down_channel(down_channel).is_none() {
        bail!("Error: down channel {down_channel} does not exist.");
    }

    let mut down_buf = vec![];

    if opts.reset {
        core.reset()?;
    }

    let stdin_setup = rtt.down_channel(down_channel).is_some();

    if stdin_setup {
        terminal::enable_raw_mode()?;
    }

    let mut output = stdout();
    let r = 'read_loop: loop {
        let events = match poll_up_channels(&mut rtt, &mut core, &mut up_readers) {
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

        if let Some(_down_channel) = rtt.down_channel(down_channel) {
            if event::poll(Duration::from_millis(0))? {
                let event = event::read()?;

                let mut bytes = vec![];
                if let Event::Key(key_event) = event {
                    // Only process key press events, not releases or repeats
                    if key_event.kind == event::KeyEventKind::Press {
                        if key_event.modifiers == KeyModifiers::CONTROL
                            && key_event.code == KeyCode::Char('c')
                        {
                            break 'read_loop Ok(Ok(()));
                        }
                        match key_event.code {
                            KeyCode::Char(c) => bytes.extend_from_slice(c.to_string().as_bytes()),
                            KeyCode::Enter => bytes.push(b'\n'),
                            KeyCode::Tab => bytes.push(b'\t'),
                            KeyCode::Backspace => bytes.push(8u8), // Backspace character
                            KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
                            KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
                            KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
                            KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
                            _ => {}
                        }
                        down_buf.extend_from_slice(bytes.as_slice());
                    }
                }
            }
        }

        if let Some(down_channel) = rtt.down_channel(down_channel) {
            if !down_buf.is_empty() {
                let count = match down_channel.write(&mut core, down_buf.as_mut()) {
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

    r?
}

fn list_probes(mut stream: impl std::io::Write, probes: &[DebugProbeInfo]) {
    writeln!(stream, "Available probes:").unwrap();

    for (i, probe) in probes.iter().enumerate() {
        writeln!(
            stream,
            "  {}: {} {}",
            i,
            probe.identifier,
            probe
                .serial_number
                .as_deref()
                .unwrap_or("(no serial number)")
        )
        .unwrap();
    }
}

fn list_channels(channels: &[impl RttChannel]) {
    if channels.is_empty() {
        println!("  (none)");
        return;
    }

    for chan in channels.iter() {
        println!(
            "  {}: {} (buffer size {})",
            chan.number(),
            chan.name().unwrap_or("(no name)"),
            chan.buffer_size(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn channel_spec_defaults_to_raw() {
        assert_eq!(
            "7".parse::<ChannelSpec>(),
            Ok(ChannelSpec {
                index: 7,
                mode: ChannelMode::Ascii,
            })
        );
    }

    #[test]
    fn channel_spec_parses_all_modes() {
        assert_eq!(
            "1:ascii".parse::<ChannelSpec>(),
            Ok(ChannelSpec {
                index: 1,
                mode: ChannelMode::Ascii,
            })
        );
        assert_eq!(
            "2:ascii".parse::<ChannelSpec>(),
            Ok(ChannelSpec {
                index: 2,
                mode: ChannelMode::Ascii,
            })
        );
        assert_eq!(
            "3:defmt".parse::<ChannelSpec>(),
            Ok(ChannelSpec {
                index: 3,
                mode: ChannelMode::Defmt,
            })
        );
    }

    #[test]
    fn channel_spec_rejects_invalid_values() {
        for value in ["", ":ascii", "1:", "1:ascii:x", "-1", "not-a-channel"] {
            assert!(value.parse::<ChannelSpec>().is_err(), "accepted {value:?}");
        }

        assert!("1:binary".parse::<ChannelSpec>().is_err());
        assert!("4294967296".parse::<ChannelSpec>().is_err());
    }

    #[test]
    fn channel_spec_accepts_u32_max() {
        assert_eq!(
            "4294967295".parse::<ChannelSpec>(),
            Ok(ChannelSpec {
                index: u32::MAX,
                mode: ChannelMode::Ascii,
            })
        );
    }

    #[test]
    fn opts_accept_repeated_channel_specs_in_order() {
        let opts = Opts::try_parse_from([
            "brtt", "-u", "3:ascii", "--up", "4", "-d", "1:defmt", "--down", "2:ascii",
        ])
        .unwrap();

        assert_eq!(
            opts.up,
            vec![
                ChannelSpec {
                    index: 3,
                    mode: ChannelMode::Ascii,
                },
                ChannelSpec {
                    index: 4,
                    mode: ChannelMode::Ascii,
                },
            ]
        );
        assert_eq!(
            opts.down,
            vec![
                ChannelSpec {
                    index: 1,
                    mode: ChannelMode::Defmt,
                },
                ChannelSpec {
                    index: 2,
                    mode: ChannelMode::Ascii,
                },
            ]
        );
    }

    #[test]
    fn opts_leave_channels_empty_when_unspecified() {
        let opts = Opts::try_parse_from(["brtt"]).unwrap();

        assert!(opts.up.is_empty());
        assert!(opts.down.is_empty());
    }

    #[test]
    fn configured_up_specs_default_to_channel_zero() {
        assert_eq!(
            configured_up_specs(&[]),
            vec![ChannelSpec {
                index: 0,
                mode: ChannelMode::Ascii,
            }]
        );
    }

    #[test]
    fn configured_up_specs_preserve_channel_order_and_modes() {
        let specs = vec![
            ChannelSpec {
                index: 2,
                mode: ChannelMode::Ascii,
            },
            ChannelSpec {
                index: 5,
                mode: ChannelMode::Ascii,
            },
        ];

        assert_eq!(configured_up_specs(&specs), specs);
    }

    #[test]
    fn defmt_channels_are_rejected_until_decoding_is_implemented() {
        let error = validate_channel_mode(&ChannelSpec {
            index: 1,
            mode: ChannelMode::Defmt,
        })
        .unwrap_err();

        assert!(error.to_string().contains("not implemented yet"));
    }

    #[test]
    fn selected_channel_preserves_default_and_rejects_multiple_down_channels() {
        assert_eq!(selected_channel(&[], "up").unwrap(), 0);
        assert_eq!(
            selected_channel(
                &[ChannelSpec {
                    index: 4,
                    mode: ChannelMode::Ascii,
                }],
                "up",
            )
            .unwrap(),
            4
        );
        assert!(selected_channel(
            &[
                ChannelSpec {
                    index: 1,
                    mode: ChannelMode::Ascii,
                },
                ChannelSpec {
                    index: 2,
                    mode: ChannelMode::Ascii,
                },
            ],
            "down",
        )
        .is_err());
    }

    #[test]
    fn channel_events_keep_channel_tags_and_render_in_order() {
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
