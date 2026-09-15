# OpenDIAG CLI

Open-source command-line tool for reading OBD-II trouble codes, live data,
and vehicle information through SAE J2534 v05.00 pass-thru adapters.
Standard OBD-II diagnostics do not require a vehicle profile. Available
readings depend on what the vehicle supports and what its adapter can access.

The CLI uses a reusable Rust library and also supports manufacturer-specific
diagnostics and ECU programming where an implementation is available.
These features require support for the particular ECU; an OBD-II connection
alone does not establish compatibility.

## Install or build

Tagged releases publish prebuilt archives on the
[releases page](https://github.com/maximus64/opendiag-cli/releases) for Linux
(x86_64), macOS (Apple silicon), and Windows (32-bit, matching the 32-bit
J2534 drivers). Unpack the archive and put `opendiag` on your `PATH`; the
vehicle and ECU definitions are compiled into the binary. On macOS the binary
is unsigned, so clear its quarantine flag with
`xattr -d com.apple.quarantine opendiag`.

To build from source instead:

```sh
cargo build --locked --release
cargo run --locked -- --help
```

The examples below use `opendiag` from your `PATH`. From a Linux/macOS source
checkout, you can use `./target/release/opendiag` instead.

Create the adapter configuration and locate the file to edit:

```sh
opendiag config init
opendiag config path
```

Set the adapter's J2534 library in the generated TOML file:

```toml
version = 1
default_adapter = "bench"

[adapters.bench]
driver = "/path/to/your/j2534/library"
# device = "device name"                  # if the library offers several
# driver_config = "/path/to/opendiag.ini"  # OpenDIAG driver only
```

Use the actual path to your adapter's `.dll`, `.so`, or `.dylib` file.
The adapter entry is selected by `default_adapter` or `--adapter bench`.
Library precedence is `--driver`, `OPENDIAG_J2534_LIB`, the selected
`--adapter`, the configured default, then the platform default. Windows uses
the 32-bit J2534 v05.00 registry; Linux/macOS use the system library path.
Windows builds must target `i686-pc-windows-gnu`. The OpenDIAG driver is built
from [opendiag-firmware](https://github.com/maximus64/opendiag-firmware);
other J2534 v05.00 drivers can also be selected.

```sh
opendiag config show
opendiag adapter list --probe
opendiag adapter voltage
```

`--config` and `OPENDIAG_CONFIG` override the OS config path. An adapter's
`driver_config` sets `OPENDIAG_DRIVER_CONFIG` for the OpenDIAG driver's
TCP/serial settings; otherwise it uses its normal `opendiag.ini` discovery.

## Standard OBD-II diagnostics

Connect the adapter to the vehicle's diagnostic connector and turn the
ignition on. These commands automatically detect a supported OBD bus:

```sh
opendiag ecu scan
opendiag dtc read
opendiag dtc read --pending
opendiag live --once
opendiag live ENGINE_RPM COOLANT_TEMP VEHICLE_SPEED
opendiag info
```

`live --once` displays supported readings. Use their names to select a smaller
set for continuous monitoring; not every vehicle supports every signal in
the example. Press Ctrl-C to stop. `info` reads available Mode 09 information,
such as VIN and calibration identifiers; some vehicles do not supply it.

Without an ECU selector, standard OBD commands query the discovered OBD ECUs.
This covers their standard emissions diagnostics, not every control module or
manufacturer-specific fault in the vehicle.

To clear trouble codes and freeze-frame data, run:

```sh
opendiag dtc clear
```

The command asks for confirmation before clearing.

### Protocol and ECU selection

Automatic detection supports the following protocols. The adapter and its
driver must support the selected protocol.

| Protocol | `--protocol` | Detection bitrate |
|---|---|---|
| CAN, 11-bit identifiers | `can11` | 500 and 250 kbit/s |
| CAN, 29-bit identifiers | `can29` | 500 and 250 kbit/s |
| ISO 9141-2 | `iso9141` | 10,400 bit/s |
| KWP2000, 5-baud initialization | `kwp-5baud` | 10,400 bit/s |
| KWP2000, fast initialization | `kwp-fast` | 10,400 bit/s |
| J1850 PWM | `j1850pwm` | 41,600 bit/s |
| J1850 VPW | `j1850vpw` | 10,400 bit/s |

Detection tries both CAN identifier widths at 500 kbit/s, then 250 kbit/s,
followed by the legacy protocols in the order listed above. If you already
know the protocol, select it to skip the other probes:

```sh
opendiag --protocol can11 live --once
opendiag --protocol iso9141 dtc read
```

Use addresses reported by `ecu scan` when selecting an ECU. CAN uses the
full request address; ISO 9141, KWP2000, and J1850 use a reply-source byte.
On those legacy buses, standard OBD requests are broadcast, so selecting
an ECU selects its replies without preventing other ECUs from hearing them.
For example, if a CAN scan reports request address `0x7E0`:

```sh
opendiag --protocol can11 live --once --ecu 0x7E0 ENGINE_RPM
```

### Vehicle identification

```sh
opendiag identify
```

Identification reads the VIN through Mode 09, then tries UDS F190 on CAN
if needed. It can match an embedded profile when the catalog has sufficient
VIN and ECU identity information. An unknown model or missing VIN does not
prevent standard OBD diagnostics; use `ecu scan`, `dtc read`, or `live`.
If the vehicle does not report a VIN, `identify --vin VIN` accepts one you
supply. Identification works offline.

## Log signals

Choose signals supported by the vehicle, using `live --once` to inspect them:

```sh
opendiag datalog -o drive.csv --signal ENGINE_RPM --signal VEHICLE_SPEED
opendiag datalog -o temperatures.csv --signal COOLANT_TEMP \
  --interval-ms 1000 --samples 60
```

CSV starts with `unix_ms,elapsed_ms` and a column for each selected reading.
Each completed row is flushed to disk. Recoverable missing readings leave
empty cells, and subsequent samples retry them. `--samples` limits the row
count; `--duration SECONDS` and Ctrl-C stop after the current sample.
The requested interval is a target; the attainable rate depends on the bus,
adapter, and selected readings.

Oxygen sensor readings may expand into separate voltage, current, fuel-trim,
and lambda columns according to the supported PIDs. Names follow the ECU's
advertised sensor layout. Use the names shown by `live --once`; unavailable
values are left empty in CSV.

## Optional ECU-specific features

### Profiles and services

Embedded profiles describe additional signals, services, and programming
support. They are optional for standard OBD-II diagnostics and cover only the
ECUs defined in the catalog. Browse the available profiles, then replace
`VEHICLE_ID` below with an ID from the list:

```sh
opendiag vehicle list
opendiag vehicle show VEHICLE_ID
opendiag --vehicle VEHICLE_ID service list
```

`vehicle search TEXT` searches the catalog. `--vehicle` accepts vehicle IDs,
not internal ECU definition IDs. Signals and services come from matched ECU
definitions; use a qualified name such as `engine.SIGNAL` when a short name
is ambiguous. A listed procedure may still be unimplemented or untested.

Automatic selections are cached per VIN and invalidated when the catalog
changes. Cached ECU identities are rechecked before destructive procedures.
An explicit `--vehicle` selects a profile manually; profiles with ECU variants
for different model years may still need a VIN year read.

### Data identifiers

DID reads and scans require an ECU with a supported manufacturer diagnostic
service. DID numbers, byte layouts, and scaling are ECU-specific; do not
assume they represent the same reading across vehicles.

```sh
opendiag did read --help
opendiag did scan --help
opendiag datalog --help
```

For known DIDs, datalog accepts repeatable `--field` options or a
`--fields-file` with one field per line. A field is
`NAME=DID[:OFFSET[:LENGTH]]` or `NAME=DID:OFFSET.BIT`. Numbers are hexadecimal
with `0x`, otherwise decimal. Integer lengths are 1–8 bytes, big endian;
whole DIDs are logged as hex strings. Each DID is read once per sample.

### ECU programming

Programming requires an implemented strategy and a compatible image for the
selected ECU. It is not a generic OBD-II function. Use `opendiag flash --help`
for image and strategy options, and start with `--dry-run` to perform preflight
without writing firmware.

Raw images require an explicit `--vehicle`, a start address from the profile
or CLI, and any strategy-specific security inputs. Use `--ecu` when several
ECUs are flashable. Raw image compatibility cannot be established from its
bytes; identity and target checks still apply. Malformed recognized containers
are refused, and RWD checksum verification is not implemented.

The vehicle definition's `system_voltage` selects a 12 V or 24 V battery
system; omission defaults to 12 V. Flashing checks battery voltage before
confirmation and again before programming begins. See the
[vehicle definition guide](vehicles/README.md#vehicle-composition) for voltage
limits and configuration. Use a stable supply that meets the manufacturer's
programming requirements.

Preflight checks finish before confirmation, and programming uses the same
bytes and options that were checked. Untested destructive operations require
`--allow-untested`; `--yes` skips only the confirmation question. Cancellation
during erase/programming is deferred until transfer and dependency checks
finish. Hardware validation is required for each supported ECU and procedure.

## Code documentation

The [crate overview](src/lib.rs) describes the project's purpose, main
components, request flow, and technologies. It is also the landing page of
the searchable HTML API reference generated by `rustdoc` through Cargo:

```sh
cargo doc --locked --no-deps --lib
```

Open `target/doc/opendiag/index.html`, or add `--open` to the command to launch
it in your browser. Keep the entire `target/doc/` directory when sharing the
HTML so that styles, search, source pages, and module links remain available.
These generated files are already excluded from version control.

The default build includes J2534 documentation and does not need an adapter or
vendor driver installed. To document the library without the J2534 feature in
a separate output directory:

```sh
cargo doc --locked --no-deps --lib --no-default-features --target-dir target/core-docs
```

That version opens at `target/core-docs/doc/opendiag/index.html`. To check
documentation links and examples when editing:

```sh
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --lib
cargo test --locked --doc
```

## Output and development

Results go to stdout; progress, warnings and `-v` protocol details go to stderr.
Firmware transfers and counted operations such as `did scan` use a single
updating progress bar with percentage, amount, average rate, and estimated time
remaining. A transfer's count advances after each acknowledged block;
finalization and dependency checks follow the transfer. Redirected output uses plain-text progress summaries every five seconds
and at completion. `TERM=dumb` also selects plain text.
Status lines are marked and colored rather than tagged: a check or finished
operation carries `✔`, a note `•`, a warning `▲` and a failure `✖`.
`--color auto` colorizes only when stdout and stderr are both terminals, so
redirecting either one never writes escape sequences into a file. `--color
always` and `--color never` override that, and `NO_COLOR` disables color.
Exit codes are 0 for success, 1 for failure and 2 for invalid CLI arguments.
Generate shell completions with `opendiag completions zsh` (also Bash, Fish,
PowerShell and Elvish).

```sh
cargo test
cargo test --no-default-features --lib
cargo check --no-default-features --lib
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps
```

J2534 hardware tests are ignored by default. See [AGENTS.md](AGENTS.md) for
architecture, development commands, and hardware test setup, and
[vehicles/README.md](vehicles/README.md) for the catalog contribution format.
External `--profile` files and JSON output are not implemented.
The project is licensed under GPL-3.0-only.
