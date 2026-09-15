//! Vehicle diagnostics and ECU programming through SAE J2534 pass-thru adapters.
//!
//! OpenDIAG provides an `opendiag` command-line application and a reusable Rust
//! library. It reads on-board diagnostics (OBD-II), identifies vehicles and
//! electronic control units (ECUs), reads and clears diagnostic trouble codes
//! (DTCs), displays live signals, records CSV logs, and runs vehicle-specific
//! service and firmware-programming workflows.
//!
//! The CLI uses the J2534 v05.00 API to communicate with OpenDIAG and other
//! compatible pass-thru adapters. Its implemented vehicle protocols are ISO-TP
//! over 11-bit and 29-bit CAN, ISO 9141-2 K-Line, KWP2000 with five-baud or fast
//! initialization, and J1850 PWM and VPW. KWP2000 and J1850 support covers
//! standard OBD diagnostics; manufacturer-specific services and programming
//! on those buses are not implemented. KWP validation currently uses an
//! emulator; real-vehicle testing remains outstanding. Manufacturer diagnostics
//! currently support Unified Diagnostic Services (UDS); programming support is
//! specific to the Honda CAN strategy and still requires hardware validation.
//!
//! # Main components
//!
//! | Component | Responsibility |
//! | --- | --- |
//! | `src/main.rs`, `src/cli/` | Parse commands, select the adapter and vehicle, dispatch operations, and render terminal output. These belong to the binary; this reference documents the library. |
//! | `src/config.rs`, [`j2534`] | Read adapter configuration, select and load a vendor driver, and implement bus sessions using J2534 v05.00. Configuration and driver loading require the `j2534` feature. |
//! | [`link`] | Define the adapter-independent contracts: [`Adapter`](link::Adapter), [`DiagLink`](link::DiagLink), [`DeviceOps`](link::DeviceOps), and [`BusSpec`](link::BusSpec). |
//! | [`obd`], [`diag`] | Manage diagnostic exchanges, ECU addressing, OBD parameter identifiers (PIDs), trouble codes, and typed manufacturer services. [`ObdProtocol`] is the central session type. |
//! | [`identify`], [`scan`] | Detect the bus, read the vehicle identification number (VIN), match vehicle definitions, discover responding ECUs, and scan data identifiers (DIDs). |
//! | [`vehicle`] | Load the embedded catalog, resolve ECU inheritance and variants, and expose each vehicle's signals, service procedures, and programming rules. |
//! | [`sampler`], [`datalog`] | Poll OBD signals and DID fields through a shared sampling engine, then deliver samples to terminal or CSV output. |
//! | [`flash`] | Inspect firmware images, prepare immutable programming input, and run the selected programming strategy. The current strategy handles Honda RWD containers and explicitly configured raw images. |
//! | [`preflight`], [`events`] | Share operation checks, confirmation prompts, progress reporting, and cooperative cancellation across frontends. |
//! | [`errors`], [`value`], [`utils`] | Provide common error types, decoded values, and byte/number conversion helpers. |
//!
//! # How the components interact
//!
//! A typical diagnostic request follows this path; responses return through
//! the same layers for decoding and presentation:
//!
//! ```text
//! CLI command + configuration + vehicle definition
//!                       |
//!                       v
//! Diagnostic workflow (identify / scan / sample / service / flash)
//!                       |
//!                       v
//! ObdProtocol and diagnostic services
//!                       |
//!                       v
//! Adapter / DiagLink interfaces
//!                       |
//!                       v
//! J2534 v05.00 backend -> vendor driver -> adapter -> vehicle ECU
//! ```
//!
//! 1. The CLI resolves its configuration and selects a driver and device. An
//!    adapter table defines a choice; `--adapter` or `default_adapter` selects
//!    it. `--driver` and `OPENDIAG_J2534_LIB` can override the library selection.
//! 2. The frontend opens an [`Adapter`](link::Adapter), chooses a [`BusSpec`](link::BusSpec)
//!    from command options, a vehicle definition, or bus detection, and creates
//!    an [`ObdProtocol`] around the connected [`DiagLink`](link::DiagLink).
//! 3. Workflows use that session to exchange complete diagnostic messages.
//!    The backend handles bus-specific framing and J2534 calls; the diagnostic
//!    layer interprets replies without depending on a particular adapter vendor.
//! 4. Results return as values or samples. [`Events`] carries progress and
//!    warnings, while [`Interaction`] supplies prompts and confirmations. The
//!    [`sampler::SampleSink`] interface lets live tables and [`datalog::CsvSink`]
//!    consume the same sampling engine.
//!
//! A session retains its adapter and active link on one thread; adapter handles
//! use [`std::rc::Rc`]. [`Cancel`] is a thread-safe request that workflows observe
//! at appropriate stopping points. Programming may defer cancellation during a
//! critical transfer. Destructive workflows use [`preflight::Preflight`] to
//! report checks and obtain confirmation before proceeding; [`flash::PreparedFlash`]
//! retains the bytes and options that were checked.
//!
//! # Vehicle data
//!
//! `vehicles/` contains TOML vehicle profiles, and `ecus/` contains reusable ECU
//! definitions. The Rust `build.rs` script embeds these files and generates
//! catalog indexes at build time. At runtime, [`vehicle::catalog`] parses and
//! validates definitions, resolves inheritance, and connects named procedures
//! to Rust implementations in [`vehicle::hooks`] or declarative [`vehicle::steps`].
//! [`identify`] caches selections per VIN and invalidates them when the catalog
//! changes. Adding supported signals or procedures can often be done in TOML;
//! new transport backends or specialized algorithms require Rust code.
//!
//! Catalog inspection does not require a connected adapter:
//!
//! ```
//! let catalog = opendiag::vehicle::catalog::embedded();
//! for id in catalog.vehicle_ids() {
//!     println!("{id}");
//! }
//! ```
//!
//! # Technologies and build options
//!
//! The application, library, and build script use **Rust 2021** and Cargo.
//! `clap` derives the command-line parser, `clap_complete` generates shell
//! completions, and `colored` formats terminal output. `serde` and `toml` handle
//! configuration, definitions, and cached selections. `libloading` loads native
//! J2534 libraries through a foreign-function interface (FFI), with `winreg`
//! providing Windows driver discovery. `dirs` locates configuration and cache
//! directories, and `ctrlc` connects terminal interrupts to cancellation.
//!
//! The default **`j2534`** Cargo feature enables configuration, driver discovery,
//! and native adapter access; the CLI requires it. With `--no-default-features`,
//! the library retains protocol logic, catalogs, sampling, and adapter traits
//! for use with another implementation or test doubles. Native drivers are
//! loaded at runtime and are not needed to generate these HTML pages. Windows
//! builds with J2534 support require the `i686-pc-windows-gnu` target to match
//! 32-bit driver DLLs; Linux and macOS use shared libraries.

#[cfg(feature = "j2534")]
pub mod config;
pub mod datalog;
pub mod diag;
/// Shared diagnostic errors and their application-facing classifications.
pub mod errors;
/// Progress events, frontend interaction, and cooperative cancellation.
pub mod events;
pub mod flash;
pub mod identify;
pub mod j2534;
pub mod link;
/// OBD-II sessions, addressing, PID decoding, trouble codes, and UDS exchanges.
pub mod obd;
/// Checks and authorization shared by destructive diagnostic workflows.
pub mod preflight;
pub mod sampler;
/// Discovery of responding ECUs and scanning of diagnostic data identifiers.
pub mod scan;
#[cfg(test)]
pub(crate) mod test_support;
/// Helpers for hexadecimal data and numeric argument conversion.
pub mod utils;
/// Decoded diagnostic values shared by terminal output and sampling.
pub mod value;
pub mod vehicle;

pub use errors::DiagError;
pub use events::{Cancel, Event, Events, Interaction};
pub use obd::ObdProtocol;
pub use utils::parse_hex_u32;
