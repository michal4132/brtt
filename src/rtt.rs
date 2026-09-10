//! RTT support and automatic control-block discovery.

use probe_rs::{config::MemoryRegion, Core, MemoryInterface};
use std::thread;
use std::time::{Duration, Instant};

pub use probe_rs::rtt::{try_attach_to_rtt, try_attach_to_rtt_shared, Error, Rtt, ScanRegion};

const SCAN_CHUNK_SIZE: usize = 4096;
const RTT_MAGIC_OVERLAP: usize = Rtt::RTT_ID.len() - 1;

/// Attaches to the first valid RTT block found by scanning memory incrementally.
pub fn attach_region_incremental(core: &mut Core<'_>, region: &ScanRegion) -> Result<Rtt, Error> {
    let started = Instant::now();
    let ranges = match region {
        ScanRegion::Ram => core
            .memory_regions()
            .filter_map(MemoryRegion::as_ram_region)
            .filter(|region| !region.is_alias)
            .map(|region| region.range.clone())
            .collect(),
        ScanRegion::Ranges(ranges) => ranges.clone(),
        ScanRegion::Exact(address) => {
            return Rtt::attach_region(core, &ScanRegion::Exact(*address))
        }
    };
    if ranges.is_empty() {
        return Err(Error::NoControlBlockLocation);
    }

    let mut bytes_read = 0usize;
    let mut chunks_read = 0usize;
    for range in ranges {
        if range.end < range.start {
            continue;
        }

        let mut address = range.start;
        let mut overlap = Vec::with_capacity(RTT_MAGIC_OVERLAP);
        while address < range.end {
            let remaining = usize::try_from(range.end - address).unwrap_or(usize::MAX);
            let read_len = remaining.min(SCAN_CHUNK_SIZE);
            let mut chunk = vec![0u8; read_len];
            if let Err(error) = core.read(address, &mut chunk) {
                log::debug!(
                    "Automatic RTT scan could not read range starting at {address:#010x}: {error}"
                );
                break;
            }
            bytes_read += read_len;
            chunks_read += 1;

            let overlap_len = overlap.len();
            let mut combined = overlap;
            combined.extend_from_slice(&chunk);
            let first_new_address = address.saturating_sub(overlap_len as u64);
            let mut search_from = 0;
            while let Some(relative) = find_magic(&combined[search_from..]) {
                let offset = search_from + relative;
                let candidate = first_new_address + offset as u64;
                search_from = offset + 1;

                match Rtt::attach_at(core, candidate) {
                    Ok(rtt) => {
                        log::debug!(
                            "Automatic RTT scan found control block at {candidate:#010x} after reading {bytes_read} bytes in {chunks_read} chunks ({:?})",
                            started.elapsed()
                        );
                        return Ok(rtt);
                    }
                    Err(Error::ControlBlockNotFound | Error::ControlBlockCorrupted(_)) => {}
                    Err(error) => return Err(error),
                }
            }

            overlap = combined.into_iter().rev().take(RTT_MAGIC_OVERLAP).collect();
            overlap.reverse();
            address += read_len as u64;
        }
    }

    log::debug!(
        "Automatic RTT scan found no control block after reading {bytes_read} bytes in {chunks_read} chunks ({:?})",
        started.elapsed()
    );
    Err(Error::ControlBlockNotFound)
}

fn find_magic(data: &[u8]) -> Option<usize> {
    data.windows(Rtt::RTT_ID.len())
        .position(|window| window == Rtt::RTT_ID)
}

/// Retries incremental automatic RTT attachment until `timeout` expires.
pub fn try_attach_to_rtt_incremental(
    core: &mut Core<'_>,
    timeout: Duration,
    region: &ScanRegion,
) -> Result<Rtt, Error> {
    let started = Instant::now();
    loop {
        match attach_region_incremental(core, region) {
            Err(Error::NoControlBlockLocation) => return Err(Error::NoControlBlockLocation),
            Err(error) if started.elapsed() < timeout => {
                log::debug!(
                    "Failed to initialize RTT automatically: {error}. Retrying until timeout."
                );
                thread::sleep(Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_is_found_when_split_at_each_chunk_boundary() {
        for split in 1..Rtt::RTT_ID.len() {
            let mut first = vec![0u8; SCAN_CHUNK_SIZE - (Rtt::RTT_ID.len() - split)];
            first.extend_from_slice(&Rtt::RTT_ID[..split]);
            let mut second = Rtt::RTT_ID[split..].to_vec();
            second.extend_from_slice(&[0u8; 4]);

            let mut combined = first[first.len() - RTT_MAGIC_OVERLAP..].to_vec();
            combined.extend_from_slice(&second);
            assert_eq!(find_magic(&combined), Some(RTT_MAGIC_OVERLAP - split));
        }
    }

    #[test]
    fn magic_is_not_found_without_a_complete_identifier() {
        assert_eq!(find_magic(&Rtt::RTT_ID[..Rtt::RTT_ID.len() - 1]), None);
    }
}
