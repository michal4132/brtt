# brtt - Better RTT Client

`brtt` is a host program for interacting with RTT channels on a target device. It is primarily designed to be used as a terminal for the Zephyr shell, providing a seamless debugging and interaction experience.

## Usage

```
brtt [OPTIONS]
```

### Options

- `-p, --probe <PROBE>`: Specify the probe number. Use `list` to see all available probes. [default: 0]
- `-c, --chip <CHIP>`: Specify the target chip type (e.g., `nRF52840_xxAA`). If not provided, `brtt` will attempt to auto-detect it.
- `-l, --list`: List available RTT up and down channels on the target and exit.
- `-u, --up <UP>`: The number of the RTT "up" channel (target to host) to use. Defaults to channel 0.
- `-d, --down <DOWN>`: The number of the RTT "down" channel (host to target) for keyboard input. Defaults to channel 0.
- `-r, --reset`: Reset the target after opening the RTT session.
- `--scan-region <SCAN_REGION>`: Specify a memory region to scan for the RTT control block. Can be an exact address (e.g., `0x20000000`) or a range (e.g., `0x20000000..0x20010000`).
