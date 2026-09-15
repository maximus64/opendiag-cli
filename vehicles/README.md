# Contributing vehicle definitions

Add TOML data under `vehicles/<make>/<model>.toml` and reuse ECU families under
`ecus/<make>/<family>.toml`. Paths define IDs automatically; adding a vehicle
that reuses existing families needs no Rust or frontend changes. `--vehicle`
accepts vehicle IDs only. `uses` and `extends` accept ECU definition IDs only.
Vehicle slots use `[module.NAME]`; `uses` links a diagnostic definition.
Without `uses`, a module requires a `label` and supplies display metadata only.

## Vehicle composition

```toml
make = "Example"
model = "Model"
years = [{ from = 2004, to = 2008 }]
markets = ["US"]
vin = []  # empty means never automatically matched
system_voltage = 12  # optional: 12 (default) or 24

[bus.main]
protocol = "can11"
# bitrate = 500000
# pins = [6, 14]

[module.engine]
uses = "example/pcm"
bus = "main"
request = 0x7E0
# response = 0x7E8
```

`system_voltage` is the nominal battery voltage, set at the top level before
any TOML tables. Only integer values `12` and `24` are accepted; omission
defaults to `12`. `opendiag vehicle show ID` displays the selected value.
Definitions are embedded at build time, so rebuild after editing them.

Flash preflight checks the measured battery voltage against these application
defaults for lead-acid systems:

| System | Refuse programming | Warn about low margin | Pass voltage check |
|---|---|---|---|
| 12 V | Below 12.0 V or above 15.0 V | 12.0 V to below 12.4 V | 12.4–15.0 V |
| 24 V | Below 24.0 V or above 30.0 V | 24.0 V to below 24.8 V | 24.8–30.0 V |

The same check runs again after confirmation, before programming voltage or
write requests. An unsupported voltage measurement produces a warning;
other read errors stop programming. Without a selected vehicle, the system
defaults to 12 V. Nominal voltage is never inferred from the measured voltage.

These limits are application policy, not validated limits for every ECU.
Follow the vehicle manufacturer's supply requirements; a passing reading
does not establish battery capacity or voltage stability throughout a flash.
Selecting 24 V does not establish adapter or ECU programming compatibility
and does not change a strategy's separate programming-voltage output.

A CAN slot has `request` and optional `response`; a legacy slot has `address`.
An explicit response permits a custom CAN pair. The standard pair is derived
when omitted. Header values must fit the bus width. Pins must be J1962 pins
1–16. Buses support `can11`, `can29`, `iso9141`, `kwp-fast`, `kwp-5baud`,
`j1850vpw`, and `j1850pwm`.

K-Line buses can set `init_address` (default 0x33). The ISO 9141 and KWP backend accepts
pins `[7]` (K only) or `[7, 15]` (K and L during initialization), and optional
`timing` overrides `P1_MAX`, `P2_MAX`, `P3_MIN`, `P4_MIN` in half-millisecond
units (`P4_MIN = 10` is 5 ms). Explicit unsupported settings fail rather than
being ignored. Other timing names in the schema are reserved for future
backends. A module needing a distinct init gets its own bus. Selecting another
bus closes the old connection and opens the new one.

For ISO 14230-4 OBD, use `protocol = "kwp-5baud"` or `protocol = "kwp-fast"`, `bitrate = 10400`,
`init_address = 0x33` and an ECU `address` matching its response source byte.
Use `diag = "obd"`; manufacturer KWP services are not implemented.

J1850 PWM uses `protocol = "j1850pwm"`, `bitrate = 41600` and `pins = [2, 10]`.
The backend rejects K-Line timing/init settings and other PWM rates or pins.
ECU `address` is the reply-source byte; standard OBD requests remain functional.
The `ford/f150-1997` profile is selected manually with no VIN prefixes or
electronic ML2-355 identity claim. Its definition contains only standard OBD,
with no manufacturer services or flash strategy.

J1850 VPW uses `protocol = "j1850vpw"`, `bitrate = 10400` and `pins = [2]`.
The backend rejects other rates/pins and K-Line timing/init settings. The
`chevrolet/suburban-2002` profile automatically matches the observed `1GNFK16Z`
VIN prefix and 2002 model year, with a Mode 09 VIN guard at source `0x10`.
Its description comes from NHTSA vPIC. The `gm/12200411` definition records
the owner's hardware label; the VIN guard establishes vehicle coverage only,
not electronic hardware identity or programming compatibility. It exposes
standard OBD diagnostics without manufacturer services or a flash strategy.

`years` is a nonempty list of inclusive ranges with named `from` and `to`
endpoints. For a production gap, list each supported range:

```toml
years = [
    { from = 2007, to = 2009 },
    { from = 2012, to = 2014 },
]
```

This excludes 2010–2011. A single model year uses equal endpoints:
`years = [{ from = 2008, to = 2008 }]`. Empty, reversed and overlapping ranges
are rejected, as is the old `years = [2007, 2009]` syntax. Ranges may be listed
in any order; adjacent ranges are allowed.

If a module changes by model year, use `[[module.engine]]` entries with the same
`years` format. Each version can cover several ranges. Its years must all be
covered by the vehicle's ranges; they cannot bridge a production gap or overlap
another version. Omitting slot years inherits all vehicle ranges. Without year
information, several versions of a diagnostic module require VIN identification.

For alternative ECUs at the same address, `uses` also accepts a list:

```toml
uses = ["honda/37820-RGL", "honda/37820-RGM", "honda/37820-RGW"]
```

Each candidate must declare disjoint `match` patterns and use the same diagnostic
protocol. The slot reads the required identity DIDs once and selects the matching
candidate. Until one matches, no candidate's signals or services are exposed.
Offline flash preparation requires agreement on the flash strategy and default
start address; programming still requires a matched definition and its guards.

## Module display labels

A module can declare `label = "Engine (ECU)"`. The CLI uses that label in
information, identification, scan and DTC output when the bus, address and
model year match the selected vehicle. Unknown or conflicting labels display
as `Module 0x…`; addresses and reply order do not imply a module's function.

A known responder without an ECU definition can have display metadata:

```toml
[module.transmission]
label = "Transmission (TCU)"
bus = "main"
request = 0x18DA1DF1
years = [{ from = 2008, to = 2008 }]
```

This example records the owner's CR-V transmission mapping. Modules with and
without `uses` share the same address fields. Optional `years` restricts the
label to supported vehicle years and requires an identified model year;
omitted years cover the whole vehicle definition. A module without `uses`
does not add an ECU candidate, require extra identity reads, provide a `--ecu`
name, or enable signals, services or flashing.

## ECU families and variants

```toml
title = "Engine (PCM)"
diag = "uds"
# extends = "example/base"
# match = [{ data = 0xF181, pattern = "CAL-FAMILY-*" }]

[[signal]]
field = "coolant_temp=0x1234:0"
unit = "°C"
scale = 1.0
offset = -40.0

[service.example]
title = "Example procedure"
description = "Document the real preconditions here."
safety = "changes-settings"
status = "untested"
steps = [{ prompt = "Ignition on, engine off" }, { session = "extended" }]

[dtc]
P1456 = "Documented manufacturer-specific description"
```

The example numbers are illustrative and are not shipped definitions.
`diag` is `obd`, `uds`, `kwp2000` or `j2190` and must fit the bus; only OBD and
UDS are available now. Inherited definitions can omit `diag`. Entries with the
same signal/service name replace the parent's entry; DTC texts and security
algorithms inherit with per-key overrides. Definition reuse via `extends` alone does not make an automatic variant.
Children with `match` are variants and retain their ancestor identity guards; literal prefixes before `*` must be disjoint between siblings.
Every pattern must match. An unmatched family remains readable but refuses
its destructive functions. A standalone definition's `match` acts as its own
identity guard.

Identity rules can use `data = 0xF181` for a manufacturer data identifier,
or `obd = 2` / `obd = 4` for Mode 09 VIN / calibration ID. Specify exactly
one source per rule. For example, an OBD-only RAD ECU uses:

```toml
diag = "obd"
match = [{ obd = 4, pattern = "37805-RAD-*" }]
```

OBD matching requires one complete identity record. Missing, malformed or
multiple records leave the ECU unmatched. The `data` and `obd` namespaces
are distinct; flash expectations continue to require manufacturer `data` IDs.

`flash` contains `strategy`, optional `start_address`, `expect` identity
patterns, `status` and `verified_on`. Raw bytes cannot establish image
compatibility: they require explicit `--vehicle`, retain a warning and still
check `flash.expect` against the ECU. Strategy names are case insensitive.

## Procedures, trust and translations

Services require `title`, `safety`, `status` and exactly one of `steps` or
`run`. Safety is `read-only`, `changes-settings` or `destructive`. Mark new
entries `untested`. `verified` requires nonempty `verified_on` evidence naming
the actual vehicle, calibration and adapter; mocked tests do not qualify.
Untested destructive functions need `--allow-untested`. Failed identity checks
and unmatched variants cannot be overridden by that flag or `--yes`.

A step has exactly one operation:

| Operation | Additional fields |
|---|---|
| `prompt = "text"` | optional `translations = { locale = "text" }` |
| `session = "default" / "programming" / "extended"` or numeric ID | |
| `security = LEVEL` | definition's `security` maps the odd seed level to a registered algorithm |
| `routine_start = ID` | optional `data = "hex"` |
| `routine_results = ID` | |
| `write_data = ID` | optional `data = "hex"`, `local = true` for legacy local IDs |
| `ecu_reset = "hard" / "key-off-on" / "soft"` | |
| `request = "hex"` | required nonempty `expect = "hex"` response prefix |
| `wait_ms = N` | |

Mutating steps cannot be declared read-only. All exchanges use `DiagServices`;
prompts and cancellation use the shared frontend interfaces. Real algorithms
and complex procedures belong in registered Rust hooks under
`src/vehicle/hooks/<make>.rs`; flash sequences belong under `src/flash/`.
Unknown hook or algorithm names fail validation. A declared stub is listed
with “not implemented” and running it returns an error.

ECU/service metadata may include `[translations.LOCALE]` with `title` and
`description` overrides. Service defaults stay in English; IDs and names stay
stable. Prompt translations contain text only. Locale selection is deferred.
Signal/service short names resolve only when unique across vehicle slots;
qualified names such as `engine.coolant_temp` always identify the slot.

## Validation and evidence

Run `cargo test` before submitting data. Validation covers every embedded
file, inheritance cycles, references, supported buses/protocols, addressing,
year ranges, signal syntax, steps, trust and translation keys. CI also checks
a core-only build. Add a make-specific reviewer to `CODEOWNERS` for both roots.
External profiles and separately distributed data are deferred.

The Honda seed contains the RWD calibration family and the 15 raw DIDs from
the former Odyssey datalog script. DID meanings, VIN prefixes and verification
evidence must come from real vehicles; do not infer them from filenames.
The model-year range is the plan's initial 2005–2010 range and needs supporting
vehicle evidence before publishing verified operations or VIN matching rules.

Offline WMI/year decoding uses Honda's [manufacturer VIN submission to NHTSA](https://vpic.nhtsa.dot.gov/mid/home/displayfile/e20b1fa6-8dbc-40ff-9a80-f0306c124149)
and the model-year codes in [49 CFR Part 565](https://www.govinfo.gov/content/pkg/CFR-2011-title49-vol6/pdf/CFR-2011-title49-vol6-part565-subpartB.pdf).
Year codes repeat after 30 years; the core retains possible years and uses
vehicle ranges to choose among them. The WMI table is intentionally limited;
unknown makes remain unknown and can still use generic OBD diagnostics.
