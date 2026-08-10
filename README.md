# OpenDIAG - Open Source OBD2 Interface (CLI Tool)

A command-line OBD-II diagnostic and ECU flashing tool, written in Rust.

This program talks to a vehicle through the **OpenDIAG OBD2 adapter**, over serial
or TCP. It reads live sensor data and trouble codes like any scan tool, and —
for supported ECUs — performs UDS firmware programming from Honda `.rwd`
containers or raw `.bin` images.

This tool targets the OpenDIAG adapter specifically. It is not a general-purpose
ELM327 utility: the flashing path depends on adapter features that generic
ELM327 devices do not have.

> [!WARNING]
> **Flashing can permanently damage an ECU.** Writing firmware erases the
> target's program memory before the new image is transferred; an interruption,
> a wrong image, or a wrong start address can leave a module that no longer
> boots and cannot be recovered over the diagnostic port. Do not flash a vehicle
> you cannot afford to have off the road.
>
> **No warranty. Use at your own risk.** This software is provided as-is, and
> everything it does to a vehicle is your responsibility. See
> [Disclaimer](#disclaimer).

## Requirements

- **Rust** — install from [rustup.rs](https://rustup.rs/).
- **An OpenDIAG OBD2 adapter**, connected over USB/serial or reachable on a TCP
  socket. A protocol emulator on a TCP socket also works for development.

### Why not a generic ELM327?

The adapter speaks an ELM327-style command set, so simple read-only commands may
happen to work on other hardware, but that is not a supported configuration and
nothing here is tested against it. Two things in particular are OpenDIAG-specific:

- **`ATPROGV`** — programming voltage control. Honda ECUs need voltage on pin 12
  of the OBD connector throughout a flash, and OpenDIAG drives it with this
  command. It is a vendor extension; a generic adapter will not implement it and
  the flash will fail at the first step.
- **Raw CAN framing.** The protocol layer puts the adapter into raw mode
  (`ATCAF0`) and constructs ISO-TP frames itself, which depends on the adapter
  passing frames through untouched.

## Building

```bash
cargo build --release
```

The binary lands at `target/release/opendiag`. Debug builds
(`cargo build`) additionally enable frame-level tracing through the
`debug_print!` / `debug_hexdump!` macros, which compile to nothing in release.

## Connecting

Every command that talks to the vehicle needs `--port`. The value selects the
transport:

| `--port` value | Transport |
| --- | --- |
| `socket://HOST:PORT` | TCP — a network-attached adapter, or an emulator |
| anything else | Serial device path, e.g. `/dev/ttyUSB0` or `COM3` |

Most commands also need `--id`, the CAN header of the ECU you want to address,
given as hex (`0x18DA01F1`) or decimal. The exceptions are `scan-ecu` and
`service`, which do not target a single module, and the three `list-*` commands,
which never open the port at all.

```bash
# Adapter on USB/serial, engine ECU
opendiag --port /dev/ttyUSB0 --id 0x18DA01F1 read-dtc

# Adapter over TCP, or an emulator
opendiag --port socket://192.168.0.10:35000 --id 0x18DA01F1 pids
```

## Quick start

```bash
# What is on the bus?
opendiag --port /dev/ttyUSB0 scan-ecu

# Read stored trouble codes
opendiag --port /dev/ttyUSB0 --id 0x18DA01F1 read-dtc
```

```
Reading stored DTCs...

2 stored DTC(s) found:
  P0101: Mass or Volume Air Flow (MAF) Circuit Range/Performance
  P0200: Injector Circuit/Open
```

```bash
# Dump every supported live PID and its current value
opendiag --port /dev/ttyUSB0 --id 0x18DA01F1 pids

# Vehicle information (VIN, calibration IDs) instead of live data
opendiag --port /dev/ttyUSB0 --id 0x18DA01F1 pids --sid 0x09

# Clear codes once you have fixed the fault
opendiag --port /dev/ttyUSB0 --id 0x18DA01F1 clear-dtc
```

## Commands

### Diagnostics

| Command | Description |
| --- | --- |
| `read-dtc` | Read stored trouble codes (mode 03), decoded to descriptions |
| `read-pending-dtc` | Read pending trouble codes (mode 07) |
| `clear-dtc` | Clear stored codes and freeze-frame data (mode 04) |
| `pids [--sid <SID>]` | Discover every supported PID and read its value. `--sid 0x01` (default) for live data, `0x09` for vehicle information |
| `voltage` | Read the adapter's measured battery voltage |
| `reset` | Re-run the adapter initialisation sequence |

### Exploration

| Command | Description |
| --- | --- |
| `scan-ecu` | Broadcast a supported-PIDs request and list every module that answers |
| `read-all-uds-ids` | Sweep UDS data identifiers `0x0000`–`0xFFFF` and print whatever reads back |
| `transfer <HEX_DATA>` | Send a raw diagnostic payload and hexdump the reply. ISO-TP segmentation is handled for you — pass the service bytes only, e.g. `transfer 22F190` |


## How it works

Four layers, each behind a trait so the layer above is transport- and
vehicle-agnostic:

1. **Transport** (`src/ports.rs`) — `OpenDiagPort`. Implementations supply raw
   read/write; the trait's default `send_command` owns the adapter's `>`-prompt
   handshake and all timeout logic. Serial and TCP implementations live in
   `src/ports/`.
2. **OBD / ISO-TP** (`src/obd/protocol.rs`) — the adapter is put into raw CAN
   mode (`ATCAF0`), so ISO-TP is implemented here by hand: PCI bytes,
   First/Consecutive frame sequencing, Flow Control parsing and STmin delays.
3. **UDS** (`src/obd/uds.rs`) — diagnostic services routed through a single
   choke point that retries on "response pending" and validates every positive
   response's service id.
4. **Vehicle-specific behaviour** — flash strategies (`src/flash.rs`) and
   car services (`src/services.rs`), each in a registry so a new ECU is an
   addition rather than an edit.


## Development

```bash
cargo build            # debug build, frame tracing enabled
cargo build --release  # release build
cargo test             # run the test suite
cargo test <name>      # run a single test by substring match
```

## Disclaimer

This software is provided **as-is, without warranty of any kind**, express or
implied, including but not limited to warranties of merchantability and fitness
for a particular purpose, as set out in sections 15 and 16 of the
[GNU General Public License](LICENSE). **You use it entirely at your own risk.**

- **No liability.** In no event shall the authors or contributors be liable for
  any claim, damage, or other loss arising from the use of this software — a
  bricked ECU, an immobilised vehicle, diagnostic or repair costs, lost data,
  personal injury, or anything else — whether in contract, tort, or otherwise.
- **You are responsible for what you send to a vehicle.** Nothing here can tell
  whether an image belongs in the module you are writing it to. Verify the
  target, the file, and the addresses yourself.
- **Only work on vehicles you own or are authorised to modify.** Programming an
  ECU may void warranties, and doing it to someone else's vehicle without
  permission may be unlawful.
- **Check your local law.** Modifying emissions-related calibrations is
  regulated in many jurisdictions and may make a vehicle illegal to drive on
  public roads. Handling manufacturer firmware may also carry its own
  restrictions. This project takes no position on either — that is yours to
  establish.
- **Safety.** A module that stops responding mid-flash can leave a vehicle
  undriveable without warning. Work on a stationary vehicle, in a place where it
  can stay put, on a stable supply.

## License

OpenDIAG is free software: you may redistribute it and modify it under the terms
of **version 3 of the GNU General Public License**, as published by the Free
Software Foundation. No later version applies. The full text is in
[LICENSE](LICENSE).

The GPL is copyleft: anything you distribute that is derived from this code must
be released under the same licence, with source. It carries no warranty — see
sections 15 and 16 of the licence, and [Disclaimer](#disclaimer).
