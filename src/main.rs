mod cli;
mod defmt;
mod logger;
mod probe_handler;
mod session;

use brtt::rtt::{try_attach_to_rtt, try_attach_to_rtt_incremental};
use brtt::RttChannel;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{ChannelEncoding, Opts, ProbeInfo};
use session::{run_session, SessionConfig};
use std::io::Write;

const RTT_ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("brtt=warn"))
        .format(|buffer, record| writeln!(buffer, "[brtt {}] {}", record.level(), record.args()))
        .init();
    let opts = Opts::parse();

    let up_specs = cli::configured_up_specs(&opts.up);
    opts.validate(&up_specs)?;
    let has_defmt = opts.debug_defmt_table
        || up_specs
            .iter()
            .any(|spec| spec.mode == ChannelEncoding::Defmt);
    let elf_bytes = opts.elf.as_deref().map(defmt::read_elf).transpose()?;
    let defmt_data = if has_defmt {
        let path = opts
            .elf
            .as_deref()
            .context("--elf is required when using an up channel with :defmt")?;
        let bytes = elf_bytes
            .as_deref()
            .context("failed to read the ELF provided via --elf")?;
        Some(defmt::DefmtData::load(path, bytes)?)
    } else {
        None
    };
    if opts.debug_defmt_table {
        let data = defmt_data.as_ref().ok_or_else(|| {
            anyhow::anyhow!("--debug-defmt-table requires --elf with a defmt table")
        })?;
        data.debug_summary(&mut std::io::stdout())?;
        return Ok(());
    }

    let elf_region = match (opts.elf.as_deref(), elf_bytes.as_deref()) {
        (Some(path), Some(bytes)) => Some(defmt::rtt_region_from_elf(path, bytes)?),
        _ => None,
    };
    drop(elf_bytes);

    let probes = probe_handler::list();

    if matches!(opts.probe, Some(ProbeInfo::List)) {
        probe_handler::list_probes(std::io::stdout(), &probes);
        return Ok(());
    }

    let attached = probe_handler::attach(&probes, opts.probe.as_ref(), opts.chip.as_deref())?;
    let mut session = attached.session;

    let automatic_scan = elf_region.is_none() && opts.scan_region.is_none();
    let scan_region = match (&elf_region, &opts.scan_region) {
        (Some(region), Some(_)) => {
            eprintln!("Ignoring --scan-region because --elf provides _SEGGER_RTT.");
            region.clone()
        }
        (Some(region), None) => region.clone(),
        (None, Some(region)) => region.clone(),
        (None, None) => session.target().rtt_scan_regions.clone(),
    };
    let mut core = session.core(0).context("Error attaching to core #0")?;

    probe_handler::ensure_supported_target(core.is_64_bit())?;

    eprintln!("Attaching to RTT...");

    let mut rtt = if automatic_scan {
        try_attach_to_rtt_incremental(&mut core, RTT_ATTACH_TIMEOUT, &scan_region)
    } else {
        try_attach_to_rtt(&mut core, RTT_ATTACH_TIMEOUT, &scan_region)
    }
    .context("Error attaching to RTT")?;
    eprintln!("Found control block at {:#010x}", rtt.ptr());

    if opts.list {
        println!("Up channels:");
        list_channels(rtt.up_channels());

        println!("Down channels:");
        list_channels(rtt.down_channels());

        return Ok(());
    }

    let config = SessionConfig::from_opts(
        opts,
        attached.label,
        attached.chip,
        defmt_data,
        scan_region,
        automatic_scan,
    )?;

    run_session(&mut core, rtt, config)
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
