//! Backup and restore of an ECU's stored data.
//!
//! An ECU definition opts in with a `[backup]` table naming a strategy. The
//! strategy decides what "the ECU's data" is (an EEPROM, a flash area, ...) and
//! how it is unlocked, read and written, one 16-bit value at a time. This module
//! owns everything shared: the JSON backup file, identity and
//! compatibility checks, confirmation, the safety copy, writing only the values
//! that differ, read-back and final verification.
//!
//! Identity comes from the ECU definition's `match`: backup and restore read the
//! live ECU's identity first and refuse an ECU that matches no variant with a
//! `[backup]` table.
pub mod honda_ba;

use crate::{
    diag::{self, DataId},
    events::{Cancel, Event, Interaction, ProgressUnit},
    preflight::{CheckStatus, Preflight},
    vehicle::{ecu_def::Status, SelectedVehicle},
    DiagError, ObdProtocol,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The `format` value of every backup file.
pub const FORMAT: &str = "opendiag-ecu-backup";
/// The backup file version this build reads and writes.
pub const VERSION: u32 = 1;

/// One named area of an ECU's data and its size in 16-bit values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockLayout {
    pub id: &'static str,
    pub words: usize,
}

/// An ECU-specific way of reaching its stored data.
///
/// A strategy only implements what differs between ECUs. Callers never write
/// through a strategy directly; [`restore`] authorizes and verifies every write.
pub trait BackupStrategy: Sync {
    /// The name ECU definitions use in `backup.strategy`.
    fn name(&self) -> &'static str;
    /// Blocks in the order they are read, written and stored.
    fn layout(&self) -> &'static [BlockLayout];
    /// What the data holds, in a mechanic's words.
    fn contents(&self) -> &'static str;
    /// The identity a backup must share with the ECU it is restored to.
    fn software(&self, protocol: &mut ObdProtocol) -> Result<String, DiagError>;
    /// Whether a backup made on `backup` software may be written to `ecu` software.
    fn compatible(&self, backup: &str, ecu: &str) -> bool {
        backup == ecu
    }
    /// Gains access to the data. May ask the user to act (e.g. power-cycle the ECU).
    fn unlock(
        &self,
        protocol: &mut ObdProtocol,
        ui: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<(), DiagError>;
    fn read_word(
        &self,
        protocol: &mut ObdProtocol,
        block: usize,
        word: usize,
    ) -> Result<u16, DiagError>;
    fn write_word(
        &self,
        protocol: &mut ObdProtocol,
        block: usize,
        word: usize,
        value: u16,
    ) -> Result<(), DiagError>;
    /// What can go wrong when a restore is interrupted, shown before confirmation.
    fn risk(&self) -> &'static str;
    /// What the mechanic should do after a successful restore.
    fn next_steps(&self) -> &'static str;
}

static STRATEGIES: &[&dyn BackupStrategy] = &[&honda_ba::HondaBa];

/// The registered strategy with this name.
pub fn strategy(name: &str) -> Option<&'static dyn BackupStrategy> {
    STRATEGIES.iter().copied().find(|s| s.name() == name)
}

pub fn strategy_names() -> Vec<&'static str> {
    STRATEGIES.iter().map(|s| s.name()).collect()
}

// ---------------------------------------------------------------- the file

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcuBackup {
    pub format: String,
    pub version: u32,
    /// UTC time the data was read, `YYYY-MM-DDTHH:MM:SSZ`.
    pub created: String,
    /// The OpenDIAG version that made the backup.
    pub opendiag: String,
    pub vehicle: BackupVehicle,
    pub ecu: BackupEcu,
    pub blocks: Vec<Block>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupVehicle {
    /// Vehicle definition ID.
    pub id: String,
    /// The VIN the ECU reported, if it reported a valid one.
    pub vin: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupEcu {
    /// Vehicle module name, e.g. `engine`.
    pub slot: String,
    /// The matched ECU definition ID.
    pub definition: String,
    /// The strategy's identity value, e.g. the software number.
    pub software: String,
    pub strategy: String,
}
/// One block of 16-bit values, stored as 4-digit hex strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawBlock", into = "RawBlock")]
pub struct Block {
    pub id: String,
    pub words: Vec<u16>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBlock {
    id: String,
    words: Vec<String>,
}
impl TryFrom<RawBlock> for Block {
    type Error = String;
    fn try_from(raw: RawBlock) -> Result<Self, Self::Error> {
        let words = raw
            .words
            .iter()
            .map(|text| {
                if text.len() == 4 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
                    Ok(u16::from_str_radix(text, 16).unwrap())
                } else {
                    Err(format!(
                        "block {}: value {text:?} is not 4 hex digits",
                        raw.id
                    ))
                }
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { id: raw.id, words })
    }
}
impl From<Block> for RawBlock {
    fn from(block: Block) -> Self {
        Self {
            id: block.id,
            words: block.words.iter().map(|w| format!("{w:04X}")).collect(),
        }
    }
}

impl EcuBackup {
    /// Reads and fully validates a backup file.
    pub fn load(path: &Path) -> Result<Self, DiagError> {
        let file =
            |e: &dyn std::fmt::Display| DiagError::InvalidFile(format!("{}: {e}", path.display()));
        let text = std::fs::read_to_string(path).map_err(|e| file(&e))?;
        let backup: Self = serde_json::from_str(&text).map_err(|e| file(&e))?;
        backup.validate().map_err(|e| file(&e))?;
        Ok(backup)
    }
    /// Writes the backup to a new file; an existing file is never replaced.
    pub fn save_new(&self, path: &Path) -> Result<(), DiagError> {
        use std::io::Write;
        self.validate()?;
        let file = |e: std::io::Error| DiagError::InvalidFile(format!("{}: {e}", path.display()));
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(file)?;
        }
        let mut text = serde_json::to_string_pretty(self)
            .map_err(|e| DiagError::InvalidFile(e.to_string()))?;
        text.push('\n');
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(file)?;
        out.write_all(text.as_bytes()).map_err(file)?;
        out.sync_all().map_err(file)
    }
    /// Checks everything a restore relies on and returns the file's strategy.
    pub fn validate(&self) -> Result<&'static dyn BackupStrategy, DiagError> {
        let invalid = |message: String| Err(DiagError::InvalidFile(message));
        if self.format != FORMAT {
            return invalid(format!(
                "not an OpenDIAG ECU backup (format {:?})",
                self.format
            ));
        }
        if self.version != VERSION {
            return invalid(format!(
                "backup version {} is not supported by this OpenDIAG (expected {VERSION})",
                self.version
            ));
        }
        if file_stamp(&self.created).is_none() {
            return invalid(format!(
                "created {:?} is not YYYY-MM-DDTHH:MM:SSZ",
                self.created
            ));
        }
        if self.vehicle.vin.as_deref().is_some_and(|vin| !is_vin(vin)) {
            return invalid("vehicle.vin is not a 17-character VIN".into());
        }
        if !is_name(&self.ecu.slot)
            || self.ecu.definition.is_empty()
            || self.ecu.software.is_empty()
        {
            return invalid("ecu.slot, ecu.definition and ecu.software must be valid names".into());
        }
        let Some(strategy) = strategy(&self.ecu.strategy) else {
            return invalid(format!(
                "backup strategy {} is not supported by this OpenDIAG",
                self.ecu.strategy
            ));
        };
        let layout = strategy.layout();
        let matches = self.blocks.len() == layout.len()
            && self.blocks.iter().zip(layout).all(|(block, expected)| {
                block.id == expected.id && block.words.len() == expected.words
            });
        if !matches {
            return invalid(format!(
                "blocks do not match the {} layout ({})",
                strategy.name(),
                layout
                    .iter()
                    .map(|b| format!("{} × {}", b.id, b.words))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(strategy)
    }
}

/// `YYYYMMDD-HHMM` from a `created` timestamp, or `None` if it is malformed.
fn file_stamp(created: &str) -> Option<String> {
    let bytes = created.as_bytes();
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let shape = bytes.len() == 20
        && digits.iter().all(|&i| bytes[i].is_ascii_digit())
        && [
            (4, b'-'),
            (7, b'-'),
            (10, b'T'),
            (13, b':'),
            (16, b':'),
            (19, b'Z'),
        ]
        .iter()
        .all(|&(i, c)| bytes[i] == c);
    shape.then(|| {
        format!(
            "{}{}{}-{}{}",
            &created[0..4],
            &created[5..7],
            &created[8..10],
            &created[11..13],
            &created[14..16]
        )
    })
}
/// Safety-copy file names are built from the slot, so it must stay plain.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn is_vin(text: &str) -> bool {
    text.len() == 17
        && text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}
/// The current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    utc_timestamp(seconds)
}
fn utc_timestamp(unix_seconds: u64) -> String {
    // Civil-from-days (Howard Hinnant), valid for every date after 1970.
    let days = (unix_seconds / 86_400) as i64;
    let time = unix_seconds % 86_400;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3_600,
        time % 3_600 / 60,
        time % 60
    )
}

// ---------------------------------------------------------------- workflows

/// The live ECU in `slot`, its matched definition and its strategy.
struct Target {
    definition: String,
    diag: String,
    status: Status,
    strategy: &'static dyn BackupStrategy,
}

/// Re-reads the live ECU's identity (never a cached match) and requires a
/// matched variant with a `[backup]` table.
fn target(
    protocol: &mut ObdProtocol,
    vehicle: &mut SelectedVehicle<'_>,
    slot: &str,
    cancel: &Cancel,
) -> Result<Target, DiagError> {
    let ecu = vehicle.resolve_for_write(slot, protocol, cancel)?;
    let definition = ecu.selection.definition.clone().unwrap_or_default();
    if !ecu.selection.matched {
        return Err(DiagError::Unsupported(format!(
            "The {slot} ECU does not match a known variant{}; backup and restore are refused",
            if definition.is_empty() {
                String::new()
            } else {
                format!(" of {definition}")
            }
        )));
    }
    let backup = ecu.definition.backup.ok_or_else(|| {
        DiagError::Unsupported(format!(
            "Backup isn't available for this ECU ({definition})"
        ))
    })?;
    let strategy = strategy(&backup.strategy).ok_or_else(|| {
        DiagError::Unsupported(format!(
            "Backup strategy {} needs a newer OpenDIAG",
            backup.strategy
        ))
    })?;
    Ok(Target {
        definition,
        diag: ecu.definition.diag.unwrap_or_else(|| "obd".into()),
        status: backup.status,
        strategy,
    })
}

/// The VIN the ECU reports, when it reports a valid one.
fn read_vin(protocol: &mut ObdProtocol, diag_name: &str) -> Option<String> {
    let bytes = diag::services(diag_name, protocol)
        .ok()?
        .read_data(DataId::Common(0xF190))
        .ok()?;
    let vin = diag::vin_text(&bytes).trim().to_string();
    is_vin(&vin).then_some(vin)
}

/// Reads every value once, reporting progress `done_before..` of `total`.
fn read_all(
    protocol: &mut ObdProtocol,
    strategy: &dyn BackupStrategy,
    cancel: &Cancel,
    done_before: u64,
    total: u64,
) -> Result<Vec<Vec<u16>>, DiagError> {
    let mut done = done_before;
    let mut blocks = Vec::new();
    for (index, layout) in strategy.layout().iter().enumerate() {
        let mut words = Vec::with_capacity(layout.words);
        for word in 0..layout.words {
            cancel.check()?;
            words.push(strategy.read_word(protocol, index, word)?);
            done += 1;
            protocol.emit(Event::Progress {
                done,
                total,
                unit: ProgressUnit::Items,
            });
        }
        blocks.push(words);
    }
    Ok(blocks)
}

/// Reads everything twice; a value that changes between passes means the data
/// (or the ECU) cannot be trusted.
fn read_twice(
    protocol: &mut ObdProtocol,
    strategy: &dyn BackupStrategy,
    cancel: &Cancel,
) -> Result<Vec<Vec<u16>>, DiagError> {
    let per_pass: u64 = strategy.layout().iter().map(|b| b.words as u64).sum();
    let first = read_all(protocol, strategy, cancel, 0, per_pass * 2)?;
    let second = read_all(protocol, strategy, cancel, per_pass, per_pass * 2)?;
    let unstable = differences(strategy, &first, &second);
    if !unstable.is_empty() {
        return Err(DiagError::InvalidResponse(format!(
            "ECU data changed between two reads at {}; the ECU may be faulty or the data is still being updated",
            describe(&unstable)
        )));
    }
    Ok(first)
}

/// Positions where two copies of the data differ, as (block, word).
fn differences(
    strategy: &dyn BackupStrategy,
    a: &[Vec<u16>],
    b: &[Vec<u16>],
) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    for (index, layout) in strategy.layout().iter().enumerate() {
        for word in 0..layout.words {
            if a[index][word] != b[index][word] {
                positions.push((index, word));
            }
        }
    }
    positions
}
fn describe(positions: &[(usize, usize)]) -> String {
    let shown: Vec<_> = positions
        .iter()
        .take(8)
        .map(|(block, word)| format!("block {block} value 0x{word:02X}"))
        .collect();
    let more = positions.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

fn new_backup(
    vehicle: &SelectedVehicle<'_>,
    slot: &str,
    target: &Target,
    software: String,
    vin: Option<String>,
    data: Vec<Vec<u16>>,
) -> EcuBackup {
    EcuBackup {
        format: FORMAT.into(),
        version: VERSION,
        created: utc_now(),
        opendiag: env!("CARGO_PKG_VERSION").into(),
        vehicle: BackupVehicle {
            id: vehicle.id.clone(),
            vin,
        },
        ecu: BackupEcu {
            slot: slot.into(),
            definition: target.definition.clone(),
            software,
            strategy: target.strategy.name().into(),
        },
        blocks: target
            .strategy
            .layout()
            .iter()
            .zip(data)
            .map(|(layout, words)| Block {
                id: layout.id.into(),
                words,
            })
            .collect(),
    }
}

/// Reads the data of the ECU in `slot`; nothing is written. Gaining access
/// can restart the ECU, so an untested procedure needs `allow_untested`, as
/// for [`restore`].
pub fn backup(
    protocol: &mut ObdProtocol,
    vehicle: &mut SelectedVehicle<'_>,
    slot: &str,
    allow_untested: bool,
    ui: &mut dyn Interaction,
    cancel: &Cancel,
) -> Result<EcuBackup, DiagError> {
    let target = target(protocol, vehicle, slot, cancel)?;
    if target.status == Status::Untested && !allow_untested {
        return Err(DiagError::PreflightFailed(format!(
            "status: backup and restore are untested for this ECU ({}); use --allow-untested to back it up",
            target.definition
        )));
    }
    let software = target.strategy.software(protocol)?;
    let vin = read_vin(protocol, &target.diag);
    protocol.emit(Event::Step(&format!(
        "Found {software} \u{b7} VIN {}",
        vin.as_deref().unwrap_or("not reported")
    )));
    protocol.emit(Event::Step(&format!(
        "Contains: {}",
        target.strategy.contents()
    )));
    if target.status == Status::Untested {
        protocol.emit(Event::Warning(&format!(
            "Backup and restore are untested for this ECU ({}, --allow-untested); keep the old ECU until a restore from this backup has been verified",
            target.definition
        )));
    }
    target.strategy.unlock(protocol, ui, cancel)?;
    protocol.emit(Event::Step("Reading ECU data (twice, to check it)"));
    let data = read_twice(protocol, target.strategy, cancel)?;
    Ok(new_backup(vehicle, slot, &target, software, vin, data))
}

/// Saves the ECU's data as it was before a restore, next to the backup being
/// restored: `<dir>/<slot>-before-restore-<YYYYMMDD-HHMM>.json`, adding a
/// counter rather than replacing an existing file.
fn save_safety_copy(dir: &Path, copy: &EcuBackup) -> Result<PathBuf, DiagError> {
    let stem = format!(
        "{}-before-restore-{}",
        copy.ecu.slot,
        file_stamp(&copy.created).expect("generated timestamp")
    );
    for counter in 1..100 {
        let name = if counter == 1 {
            format!("{stem}.json")
        } else {
            format!("{stem}-{counter}.json")
        };
        let path = dir.join(name);
        match copy.save_new(&path) {
            Err(DiagError::InvalidFile(_)) if path.exists() => continue,
            result => return result.map(|()| path),
        }
    }
    Err(DiagError::InvalidFile(format!(
        "{}: too many safety copies named {stem}",
        dir.display()
    )))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreOptions {
    /// Stop after the checks and the summary of changes.
    pub dry_run: bool,
    pub allow_untested: bool,
    /// Report a supply outside the battery limits as a warning.
    pub skip_battery_check: bool,
}

#[derive(Debug)]
pub struct RestoreReport {
    pub preflight: Preflight,
    /// Values written; zero for a dry run or an ECU that already matched.
    pub written: usize,
    /// The ECU's data as it was before the restore.
    pub safety_copy: Option<PathBuf>,
    pub next_steps: &'static str,
}

/// Writes `backup` to the ECU in `slot`.
///
/// Every check runs before the ECU is unlocked. The ECU's current data is then
/// read twice and saved in `safety_copy_dir` before confirmation; only
/// values that differ are written, each is read back, and everything is
/// verified at the end. Cancellation is not observed while writing.
#[allow(clippy::too_many_arguments)]
pub fn restore(
    protocol: &mut ObdProtocol,
    vehicle: &mut SelectedVehicle<'_>,
    slot: &str,
    backup: &EcuBackup,
    safety_copy_dir: &Path,
    options: &RestoreOptions,
    ui: &mut dyn Interaction,
    cancel: &Cancel,
) -> Result<RestoreReport, DiagError> {
    backup.validate()?;
    let target = target(protocol, vehicle, slot, cancel)?;
    let strategy = target.strategy;
    let software = strategy.software(protocol)?;
    let mut preflight = Preflight::new("Restore ECU backup", strategy.risk());
    preflight.add(
        "ECU identity",
        format!("{software} matches {}", target.definition),
        CheckStatus::Ok,
    );
    let same_ecu_type =
        backup.ecu.definition == target.definition && backup.ecu.strategy == strategy.name();
    preflight.add(
        "backup file",
        if same_ecu_type {
            format!(
                "{} from {}, VIN {}",
                backup.created,
                backup.vehicle.id,
                backup.vehicle.vin.as_deref().unwrap_or("unknown")
            )
        } else {
            format!(
                "made for {} ({}), not {}",
                backup.ecu.definition, backup.ecu.strategy, target.definition
            )
        },
        if same_ecu_type {
            CheckStatus::Ok
        } else {
            CheckStatus::Fail
        },
    );
    if strategy.compatible(&backup.ecu.software, &software) {
        preflight.add(
            "compatibility",
            format!("software {software} matches the backup"),
            CheckStatus::Ok,
        );
    } else {
        preflight.add(
            "compatibility",
            format!(
                "the backup is from {}, this ECU runs {software}; program {} first or use a backup from a matching ECU",
                backup.ecu.software, backup.ecu.software
            ),
            CheckStatus::Fail,
        );
    }
    let (battery, status) = crate::preflight::battery_check(
        protocol,
        vehicle.vehicle.system_voltage,
        options.skip_battery_check,
    )?;
    preflight.add("battery", battery, status);
    match target.status {
        Status::Verified => preflight.add("status", "verified", CheckStatus::Ok),
        Status::Untested if options.allow_untested => {
            preflight.add("status", "untested (--allow-untested)", CheckStatus::Warn)
        }
        Status::Untested => preflight.add(
            "status",
            "untested; use --allow-untested to restore",
            CheckStatus::Fail,
        ),
    }
    // Refuse before asking anyone to power-cycle an ECU.
    if let Err(error) = preflight.validate() {
        ui.report(&preflight);
        return Err(error);
    }

    let vin = read_vin(protocol, &target.diag);
    strategy.unlock(protocol, ui, cancel)?;
    protocol.emit(Event::Step(
        "Reading the ECU's current data (twice, to check it)",
    ));
    let current = read_twice(protocol, strategy, cancel)?;
    let wanted: Vec<Vec<u16>> = backup.blocks.iter().map(|b| b.words.clone()).collect();
    let changes = differences(strategy, &current, &wanted);
    let total: usize = strategy.layout().iter().map(|b| b.words).sum();
    if changes.is_empty() {
        preflight.add(
            "will change",
            "nothing: the ECU already holds this backup",
            CheckStatus::Ok,
        );
        return Ok(RestoreReport {
            preflight,
            written: 0,
            safety_copy: None,
            next_steps: strategy.next_steps(),
        });
    }
    if let Some(after) = &backup.vehicle.vin {
        if vin.as_ref() != Some(after) {
            preflight.add(
                "VIN",
                format!("{} \u{2192} {after}", vin.as_deref().unwrap_or("blank")),
                CheckStatus::Info,
            );
        }
    }
    preflight.add(
        "will change",
        format!(
            "{} of {total} values; contains {}",
            changes.len(),
            strategy.contents()
        ),
        CheckStatus::Info,
    );
    if options.dry_run {
        preflight.add("safety copy", "not saved (dry run)", CheckStatus::Info);
        return Ok(RestoreReport {
            preflight,
            written: 0,
            safety_copy: None,
            next_steps: strategy.next_steps(),
        });
    }
    let copy = new_backup(vehicle, slot, &target, software, vin, current.clone());
    let safety_copy = save_safety_copy(safety_copy_dir, &copy)?;
    preflight.add(
        "safety copy",
        safety_copy.display().to_string(),
        CheckStatus::Ok,
    );
    preflight.authorize(ui, cancel)?;

    // Access may lapse while the question is open. Confirm it with a read and
    // check that the ECU still holds what was read before anything is written.
    let (block, word) = changes[0];
    let still = match strategy.read_word(protocol, block, word) {
        Ok(value) => value,
        Err(_) => {
            protocol.emit(Event::Warning(
                "ECU access lapsed while waiting for confirmation; unlocking again",
            ));
            strategy.unlock(protocol, ui, cancel)?;
            strategy.read_word(protocol, block, word)?
        }
    };
    if still != current[block][word] {
        return Err(DiagError::InvalidResponse(format!(
            "The ECU's data changed after it was read (block {block} value 0x{word:02X}); nothing was written. Was the ECU swapped?"
        )));
    }

    protocol.emit(Event::Step("Writing (Ctrl-C waits until this finishes)"));
    let mut mismatched = Vec::new();
    for (done, &(block, word)) in changes.iter().enumerate() {
        let value = wanted[block][word];
        let result = strategy
            .write_word(protocol, block, word, value)
            .and_then(|()| strategy.read_word(protocol, block, word));
        match result {
            Ok(read_back) if read_back == value => {}
            Ok(_) => mismatched.push((block, word)),
            Err(error) => {
                protocol.emit(Event::Warning(&format!(
                    "Restore stopped after {done} of {} values were written and checked. The ECU's previous data is saved in {}. Running the same restore again is safe: values that already match are skipped.",
                    changes.len(),
                    safety_copy.display()
                )));
                return Err(error);
            }
        }
        protocol.emit(Event::Progress {
            done: done as u64 + 1,
            total: changes.len() as u64,
            unit: ProgressUnit::Items,
        });
    }
    cancel.check()?;

    protocol.emit(Event::Step("Verifying"));
    let total_words = total as u64;
    let result = read_all(protocol, strategy, cancel, 0, total_words)?;
    for position in differences(strategy, &result, &wanted) {
        if !mismatched.contains(&position) {
            mismatched.push(position);
        }
    }
    if !mismatched.is_empty() {
        mismatched.sort_unstable();
        return Err(DiagError::InvalidResponse(format!(
            "{} values did not verify after writing: {}. The ECU's previous data is saved in {}",
            mismatched.len(),
            describe(&mismatched),
            safety_copy.display()
        )));
    }
    Ok(RestoreReport {
        preflight,
        written: changes.len(),
        safety_copy: Some(safety_copy),
        next_steps: strategy.next_steps(),
    })
}

#[cfg(test)]
mod tests;
