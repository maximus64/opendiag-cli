# AGENTS.md

## Repository Overview

OpenDIAG is a Rust 2021 command-line application and reusable library for OBD-II diagnostics, vehicle identification, live data, CSV logging, and ECU programming through SAE J2534 v05.00 adapters.

| Path | Responsibility |
|---|---|
| `src/main.rs`, `src/cli/` | CLI arguments, command dispatch, terminal output, prompts, and exit codes |
| `src/lib.rs` | Public library modules and architecture overview |
| `src/link.rs` | Transport boundary: `Adapter`, `DeviceOps`, `DiagLink`, `BusSpec`, and complete diagnostic messages |
| `src/j2534/` | Driver discovery, native bindings, device ownership, CAN, K-Line, and J1850 backends |
| `src/obd/`, `src/diag.rs`, `src/scan.rs` | OBD/UDS requests, addressing, PID decoding, DTCs, and ECU/DID scanning |
| `src/identify.rs`, `src/vehicle/` | Identification, VIN cache, catalog validation, ECU selection, and service procedures |
| `src/flash/`, `src/flash.rs`, `src/preflight.rs` | Image parsing, programming strategies, compatibility checks, and authorization |
| `src/sampler.rs`, `src/datalog.rs`, `src/value.rs` | Shared sampling, CSV output, and typed readings |
| `src/events.rs`, `src/errors.rs` | Progress, interaction, cancellation, and diagnostic errors |
| `vehicles/`, `ecus/`, `build.rs` | TOML definitions embedded and indexed at build time |
| `src/test_support.rs`, `tests/` | Test doubles, CLI integration tests, and captured protocol fixtures |

Read `vehicles/README.md` before changing catalog data.

## Build, Test, and Lint Commands

Run from the repository root using stable Rust with `rustfmt` and Clippy. CI is defined in `.github/workflows/ci.yml`.

```sh
# Build and inspect CLI help
cargo build --locked --release
cargo run --locked -- --help

# Compile/type-check
cargo check --locked --all-targets
cargo check --locked --no-default-features --lib

# Tests
cargo test --locked
cargo test --locked --no-default-features --lib

# Formatting and linting
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings

# API documentation
cargo doc --locked --no-deps
```

Apply formatting with `cargo fmt --all`. For documentation changes, also run:

```sh
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --lib
cargo test --locked --doc
```

The default `j2534` feature enables native adapter access and is required by the CLI. Without default features, check and test the library using `--lib`. Vendor drivers load at runtime; ordinary builds and tests require no adapter.

Windows J2534 builds require the 32-bit target and its MinGW toolchain:

```sh
cargo build --locked --release --target i686-pc-windows-gnu
```

Strict Clippy is clean on Rust 1.97.1. Report any failure you find instead of weakening lint settings to conceal it.

## Coding Style & Conventions

- Follow rustfmt. Use `snake_case` for modules, functions, and fields; `UpperCamelCase` for types and traits; `SCREAMING_SNAKE_CASE` for constants.
- Extend the relevant domain module. CLI subcommands belong in `src/cli/<command>.rs`; specialized implementations belong beneath their owning module.
- Prefer readable declarations, explicit control flow, and small helpers. Comment invariants, protocol constraints, and safety reasoning rather than restating code.
- Use typed addresses, bus specifications, diagnostic values, and errors. Core operations generally return `Result<T, DiagError>` and propagate failures with `?`.
- Avoid panics for malformed input, unsupported settings, missing drivers, or unexpected ECU replies.
- Document public contracts with rustdoc. Explain each unsafe operation with a `SAFETY` comment.

## Architecture & Design Rules

While the project is in early development, do not preserve backward compatibility. Remove obsolete code paths instead of adding compatibility layers, fallbacks, or migrations. Update affected callers, tests, and documentation as part of the same change.

- **Keep the library independent of terminal presentation.** CLI parsing and rendering stay in `src/cli/`. Core workflows return structured data and use `Events`, `Interaction`, and `Cancel`.
- **Preserve the transport boundary.** Diagnostics operate on complete messages through `Adapter` and `DiagLink`. J2534 backends own framing and native API details; they must not know vehicle definitions.
- **Preserve session ownership.** Device handles use `Rc`; sessions retain the device and release resources through `Drop`. Keep adapter operations on the owning thread and close the previous link when switching buses.
- **Protect the native ABI.** Keep unsafe driver calls in the FFI layer. Preserve packed layouts, `extern "system"` signatures, buffer lifetimes, and driver error details.
- **Validate before opening hardware.** Invalid CLI arguments should fail before driver loading where possible. Preserve exit codes: `0` success, `1` operational failure, `2` usage error. Results go to stdout; progress, warnings, and verbose details go to stderr.
- **Keep sampling shared.** Live output and CSV use `sampler::run` and `SampleSink`. Preserve request deduplication, integer precision, empty cells for unavailable readings, and completed-row flushing.
- **Keep identification offline.** Cache entries depend on VIN, schema, and catalog fingerprint. Recheck cached ECU identities before destructive operations; cached matches do not authorize writes.
- **Keep authorization in the core.** Destructive workflows complete preflight before confirmation. `--yes` skips the question only; it must not bypass identity, compatibility, or trust checks. Program the bytes and options retained by `PreparedFlash`.
- **Preserve safe cancellation and cleanup.** Do not interrupt critical erase/programming transfers before their safe boundary. Release programming voltage and native resources on failure.
- **Validate protocol replies strictly.** Match ECU, service, identifier, lengths, and response echoes. Preserve bounded initialization retries and receive-only handling of pending responses; do not introduce automatic resends of accepted requests.

Configuration precedence is part of the interface:

- Config path: `--config` → `OPENDIAG_CONFIG` → OS default.
- Driver selection: `--driver` → `OPENDIAG_J2534_LIB` → `--adapter` → `default_adapter` → platform default.
- An `[adapters.<name>]` entry defines an adapter without selecting it. Check `cargo run --locked -- config show` before changing discovery behavior.
- A selected adapter’s `driver_config` is passed through `OPENDIAG_DRIVER_CONFIG`.

Catalog changes must preserve these rules:

- Paths define IDs: `vehicles/<make>/<model>.toml` and `ecus/<make>/<family>.toml`.
- `--vehicle` accepts vehicle IDs; `uses` and `extends` reference ECU definitions.
- Vehicle slots use `[module.NAME]` or `[[module.NAME]]` for year-specific versions. `uses` links ECU definitions; without it, a required `label` provides display metadata only.
- Follow the strict schemas, inclusive named year ranges, inheritance rules, and identity guards.
- Put declarative procedures in TOML, registered algorithms in `src/vehicle/hooks/`, and programming sequences in `src/flash/`.
- Edit source TOML files, never generated `OUT_DIR/catalog.rs`.

## Testing & Verification

Unit tests live beside implementations under `#[cfg(test)]` or in module-local `tests.rs`. CLI integration tests live in `tests/cli.rs`; protocol captures live in `tests/fixtures/`.

```sh
# One unit test
cargo test --locked --lib vehicle::tests::every_embedded_file_is_valid_and_vehicle_ids_are_separate -- --exact

# One CLI integration test
cargo test --locked --test cli usage_errors_exit_two_before_loading_the_adapter -- --exact

# A module's tests
cargo test --locked --lib obd::protocol::tests
```

- Add regression coverage for changed behavior at the lowest useful layer.
- Reuse `MockLink`, `FakeAdapter`, `FakeEcu`, `RecordingEvents`, and `ScriptedInteraction` from `src/test_support.rs`.
- Test relevant failure paths: malformed replies, wrong ECU, timeout, cancellation, declined confirmation, and cleanup. Programming failures before authorization must produce no erase/write requests.
- Keep ordinary tests deterministic and independent of personal configuration or attached hardware. Use temporary directories and explicit environment overrides.
- Run both feature configurations when changing shared library code. Catalog edits must pass embedded catalog validation.
- Distinguish simulated, captured-message, emulator, and real-hardware evidence. Host tests do not establish electrical timing or programming compatibility.

Ignored J2534 tests require an explicitly configured adapter or firmware peer; the CAN test also requires its documented test ECU. Set `OPENDIAG_J2534_LIB` and, when needed, `OPENDIAG_DRIVER_CONFIG`, then run serially:

```sh
cargo test --locked --lib j2534::v0500:: -- --ignored --test-threads=1
```

## PR & Commit Etiquette

- Use descriptive branches such as `fix/kwp-timeout`, `feat/vehicle-catalog`, or `docs/adapter-setup`.
- Use Conventional Commits: `fix(obd): reject mismatched PID responses`. Suitable types include `feat`, `fix`, `refactor`, `test`, `docs`, and `chore`.
- Keep commits focused. Review the diff and stage explicit paths; preserve unrelated working-tree changes.
- Before a PR, run formatting, default and core-only tests, strict Clippy, core-only checking, and documentation generation.
- Run `git diff --check` and inspect staged changes for generated files, local settings, and unintended dependency changes.
- Describe the problem, resulting behavior, verification performed, and remaining limitations. Identify pre-existing failures and untested hardware/platform combinations.

## Do’s and Don’ts

- Keep changes within the requested scope. Present a concrete plan before broad architectural changes.
- Preserve the feature boundary so the library remains usable without J2534.
- Reject unsupported functionality explicitly; never silently ignore transport settings or report an unimplemented procedure as successful.
- Do not infer VIN coverage, ECU identity, DID meaning, programming compatibility, or verified status from filenames or hardware labels. Record supporting evidence.
- Do not broaden functional-address fallback to programming, control, or manufacturer requests.
- Do not bypass preflight, `--allow-untested`, identity guards, or explicit vehicle selection for raw firmware.
- Do not use live fault clearing, resets, programming, or programming-voltage operations as routine verification without task authorization.
- Do not commit generated `target/` output, firmware dumps, credentials, personal driver configuration, or incidental logs.
- Do not update dependencies, rewrite unrelated code, or modify sibling firmware/driver repositories incidentally. Preserve `Cargo.lock`; update it only when dependency changes require it.
- Preserve GPL-3.0-only licensing and existing attribution.
