// OpenDIAG - a command-line OBD-II diagnostic and ECU flashing tool.
// Copyright (C) 2026 Khoa Hoang
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, version 3.
//
// This program is distributed in the hope that it will be useful, but WITHOUT
// ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with
// this program. If not, see <https://www.gnu.org/licenses/>.

mod ports;
mod errors;
mod utils;
mod obd;
mod flash;
mod services;
#[cfg(test)]
mod test_support;

use colored::Colorize;
use ports::serial::SerialPort;
use ports::tcp::TcpPort;
use ports::OpenDiagPort;
use utils::{hex_to_bytes, hexdump, bytes_to_hex};
use obd::{ObdProtocol};
use obd::addressing::{describe_address, Addressing};
use obd::protocol::ResponseFrame;
use clap::{Parser, Subcommand};
use std::path::PathBuf; // added for flash file path
use std::io::{self, Write}; // for confirmation prompt
use flash::{get_strategy, get_supported_ecus, is_supported_ecu, FlashOptions};
use obd::uds::read_data_by_identifier; // for UDS reading
use services::{get_supported_cars, get_services_for_car, car_to_ecu};


/// Parses a number written either as `0x`-prefixed hex or as plain decimal.
/// Every numeric flag goes through this so they all accept the same syntax.
fn parse_hex_u32(s: &str) -> Result<u32, String> {
    // Check if the string starts with "0x" for hexadecimal
    if let Some(hex_val) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex_val, 16).map_err(|e| e.to_string())
    } else {
        // Otherwise, parse as a regular decimal number
        s.parse::<u32>().map_err(|e| e.to_string())
    }
}

/// Same syntax as [`parse_hex_u32`], narrowed to a single byte.
fn parse_hex_u8(s: &str) -> Result<u8, String> {
    let value = parse_hex_u32(s)?;
    u8::try_from(value).map_err(|_| format!("{} does not fit in a byte", value))
}

/// Parses a hex byte string (optionally 0x-prefixed) into raw bytes.
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let cleaned = s.strip_prefix("0x").unwrap_or(s);
    hex_to_bytes(cleaned).map_err(|e| e.to_string())
}

#[derive(Parser, Debug)]
#[command(name = "opendiag", version = "1.0", about = "A diagnostic tool for embedded devices")]
struct Cli {
    // Connection Info (Global Flags)
    #[arg(short, long, global = true, help = "Device path (e.g., /dev/ttyUSB0 or 192.168.1.1)")]
    port: Option<String>,

    #[arg(short, long, global = true, help = "Baud rate for serial connection")]
    baud: Option<u32>,

    // ECU ID
    #[arg(short = 'i', long = "id", global = true, help = "ECU ID to talk to", value_parser = parse_hex_u32)]
    ecu_id: Option<u32>,

    // Car model
    #[arg(short, long, global = true, help = "Car model, e.g., honda_odyssey_rl3")]
    car: Option<String>,

    // The command to execute (e.g., "reset").
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug, PartialEq)]
enum Commands {
    /// Resets the connected device
    Reset,
    /// Reads the device's voltage
    Voltage,
    /// Transfer diagnostic frame
    Transfer {
        #[arg(name = "HEX_DATA")]
        data: String,
    },
    /// Reads all supported PIDs from the ECU
    Pids {
        #[arg(long,
              default_value_t = 0x01,
              value_parser = parse_hex_u8,
              help = "Service ID (SID) to use, e.g., 0x01 for current data, 0x09 for vehicle info")]
        sid: u8,
    },
    /// Read stored Diagnostic Trouble Codes (DTCs)
    ReadDtc,
    /// Read pending Diagnostic Trouble Codes (DTCs)
    ReadPendingDtc,
    /// Clear stored Diagnostic Trouble Codes (DTCs)
    ClearDtc,
    /// List supported ECU types for flashing
    ListEcus,
    /// Show basic information / identification for a specific supported ECU type (no flashing)
    FlashInfo,
    /// Perform a firmware flash
    Flash {
        #[arg(name = "FILE")]
        file: PathBuf,
        #[arg(long, help = "Skip interactive confirmation prompt")]
        force: bool,
        #[arg(long = "start-address", value_parser = parse_hex_u32,
              help = "Download start address (required for raw .bin; overrides .rwd embedded value)")]
        start_address: Option<u32>,
        #[arg(long = "security-key", value_parser = parse_hex_bytes,
              help = "SecurityAccess secret as hex (4 or 6 bytes); highest priority")]
        security_key: Option<Vec<u8>>,
        #[arg(long = "tokens",
              help = "Companion .rwd file to source the security token from, matched by live calibration id")]
        tokens: Option<PathBuf>,
    },
    /// Read all UDS identifiers
    ReadAllUdsIds,
    /// Scan ECUs on the bus
    ScanEcu,
    /// Perform a car-specific service
    Service {
        #[arg(name = "SERVICE")]
        service: String,
    },
    /// List supported car models
    ListCars,
    /// List available services
    ListServices,
}

/// Prints what answered a bus scan, in terms the user can act on.
///
/// A scan only ever sees *response* headers, while `--id` takes the *request*
/// header that addresses the same ECU — on 29-bit CAN a reply on `18DAF101`
/// means `--id 0x18DA01F1`. Printing the raw header alone leaves the user to do
/// that swap by hand, so the request header is derived here and both are shown.
///
/// The name attached to an address is a convention, never a fact the bus
/// reported, and is labelled as such. An address with no known convention gets
/// no name rather than a plausible-looking guess.
fn print_scan_results(addressing: Addressing, responses: &[ResponseFrame]) {
    let mut seen: Vec<u32> = Vec::new();

    for response in responses {
        if seen.contains(&response.id) {
            continue; // an ECU that sent several frames is still one ECU
        }
        seen.push(response.id);

        println!();
        match addressing.responder_address(response.id) {
            Some(address) => {
                let description = match describe_address(address) {
                    Some(name) => format!("{} {}", name, "[convention, not guaranteed]".dimmed()),
                    None => "no conventional name for this address".dimmed().to_string(),
                };
                println!("  ECU 0x{:02X} - {}", address, description);

                match addressing.physical_request(address) {
                    Some(request) => println!(
                        "      use with --id : {}",
                        format!("0x{}", addressing.format_header(request)).green()
                    ),
                    None => println!(
                        "      use with --id : {}",
                        "unavailable on this protocol".yellow()
                    ),
                }
            }
            None => {
                println!("  {}", "Unrecognized response header - cannot derive --id".yellow());
            }
        }

        println!("      responded on  : 0x{}", addressing.format_header(response.id));
    }
}

// ---------------- Flash scaffolding helpers ----------------

fn list_supported_ecus() {
    println!("Supported ECUs:");
    for ecu in get_supported_ecus() { 
        println!("  - {}", ecu); 
    }
}

fn list_supported_cars() {
    println!("Supported car models:");
    for car in get_supported_cars() {
        println!("  - {}", car);
    }
}

fn confirm_flash(ecu: &str, file: &PathBuf) -> bool {
    println!("Flash summary:");
    println!("  ECU type : {}", ecu);
    println!("  File     : {:?}", file);
    print!("Type 'y' to confirm: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() { return false; }
    let trimmed = line.trim();
    if trimmed.eq("y") { true } else { println!("Aborted."); false }
}

fn list_services(car: &str) {
    println!("Available services for car '{}':", car);
    if let Some(services) = get_services_for_car(car) {
        if services.is_empty() {
            println!("  - No services found for '{}'", car);
        } else {
            for service in services.keys() {
                println!("  - {}", service);
            }
        }
    } else {
        println!("  - No services found for '{}'", car);
    }
}
// ----------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Handle commands that don't require a port connection first
    match &cli.command {
        Commands::ListEcus => {
            list_supported_ecus();
            return Ok(());
        }
        Commands::ListCars => {
            list_supported_cars();
            return Ok(());
        }
        Commands::ListServices => {
            let car = cli.car.as_deref().ok_or("Error: --car is required")?;
            list_services(car);
            return Ok(());
        }
        _ => {} // Continue with port-requiring commands
    }

    let port_path = cli.port.ok_or("Error: --port is required")?;

    println!("Connecting to '{}'...", &port_path);

    let port: Box<dyn OpenDiagPort> = if let Some(addr) = port_path.strip_prefix("socket://") {
        // TCP connection
        let tcp_port = TcpPort::new(addr)?;
        Box::new(tcp_port)
    } else {
        // Assume Serial otherwise
        let baud_rate = cli.baud.unwrap_or(38400);
        let serial_port = SerialPort::new(&port_path, baud_rate)?;
        Box::new(serial_port)
    };

    let mut od = ObdProtocol::new(port);

    // Initialize the OBD interface
    od.reset()?;

    // Recover from a previous run that was killed mid-flash: SIGINT does not
    // unwind, so its programming-voltage guard never ran.
    od.clear_all_programming_voltage()?;

    let command = cli.command;

    if !matches!(command, Commands::ScanEcu | Commands::Service { .. }) {
        let ecu_id = cli.ecu_id.ok_or("Error: Please specify ECU ID with --id")?;
        od.set_header_id(ecu_id)?;
    }

    match command {
        Commands::Reset => {
            println!("Executing reset command...");
            od.reset()?;
        }
        Commands::Voltage => {
            println!("Executing voltage command...");
            od.read_voltage()?;
        }
        Commands::Transfer { data } => {
            println!("Executing transfer command...");
            println!("Parsed data payload: {}", data.green());
            let bytes = hex_to_bytes(&data).map_err(|e| format!("Invalid hex string: {}", e))?;
            println!("Decoded bytes: {:02X?}", bytes);
            let resp = od.transfer(&bytes)?;
            println!("Frame ID: 0x{:08x}", resp.id);
            println!("Data:");
            hexdump(&resp.data)
        }
        Commands::Pids { sid } => {
            println!("Reading PIDs...");
            od.read_all_vehicle_data(sid)?;
        }
        Commands::ReadDtc => {
            println!("Reading stored DTCs...");
            let dtcs = od.read_dtc(false)?;
            if dtcs.is_empty() { println!("{}", "No stored DTCs found".green()); } else {
                println!("\n{} stored DTC(s) found:", dtcs.len());
                for dtc in dtcs { println!("  {}: {}", dtc.code.red(), dtc.description); }
            }
        }
        Commands::ReadPendingDtc => {
            println!("Reading pending DTCs...");
            let dtcs = od.read_dtc(true)?;
            if dtcs.is_empty() { println!("{}", "No pending DTCs found".green()); } else {
                println!("\n{} pending DTC(s) found:", dtcs.len());
                for dtc in dtcs { println!("  {}: {}", dtc.code.yellow(), dtc.description); }
            }
        }
        Commands::ClearDtc => {
            println!("Clearing stored DTCs...");
            od.clear_dtc()?;
            println!("{}", "DTCs cleared successfully!".green());
        }
        Commands::ListEcus => {
            list_supported_ecus();
        }
        Commands::FlashInfo => {
            let car = cli.car.as_deref().ok_or("Error: Please specify car with --car")?;
            let ecu = car_to_ecu(car).ok_or_else(|| format!("Unsupported car: {}", car))?;
            if !is_supported_ecu(ecu) { eprintln!("Unsupported ECU type: {}", ecu); std::process::exit(2); }
            if let Some(mut strategy) = get_strategy(ecu, &mut od) {
                println!("{}", strategy.info()?);
            } else {
                println!("No strategy implementation found for {} (scaffold)", ecu);
            }
        }
        Commands::Flash { file, force, start_address, security_key, tokens } => {
            let car = cli.car.as_deref().ok_or("Error: Please specify car with --car")?;
            let ecu = car_to_ecu(car).ok_or_else(|| format!("Unsupported car: {}", car))?;
            if !is_supported_ecu(ecu) {
                eprintln!("Unsupported ECU type: {}", ecu);
                std::process::exit(2);
            }
            let mut strategy = match get_strategy(ecu, &mut od) {
                Some(s) => s,
                None => {
                    eprintln!("Strategy not implemented for {}", ecu);
                    return Ok(());
                }
            };

            if !force && !confirm_flash(ecu, &file) {
                return Ok(());
            }

            let opts = FlashOptions { start_address, security_key, tokens };

            println!("Starting flash for {}", car);
            if let Err(e) = strategy.flash(&file, &opts) {
                eprintln!("Flash failed: {}", e);
            } else {
                println!("Flash completed");
            }
        }
        Commands::ReadAllUdsIds => {
            println!("Reading all UDS identifiers...");

            for id in 0x0000..=0xffff {
                print!("Scanning ID 0x{:04X}\r", id);
                match read_data_by_identifier(&mut od, id) {
                    Ok(value) => {
                        let text = String::from_utf8_lossy(&value).to_string();
                        let sanitized_text: String = text.chars().map(|c| {
                            if c.is_ascii_graphic() {
                                c // Keep the character if it's printable ASCII
                            } else {
                                '.' // Otherwise, replace it with a period
                            }
                        }).collect();
                        println!("ID 0x{:04X}: {}", id, bytes_to_hex(&value).green());
                        println!(  "           {}", sanitized_text.cyan());
                    },
                    Err(e) => {}, //eprintln!("Failed to read ID 0x{:04X}: {}", id, e),
                }
            }
            println!("UDS ID scan complete.");
        }
        Commands::ScanEcu => {
            println!("Scanning for ECUs on the bus...");
            // Simple scan by sending a broadcast request and listening for responses
            let request = [0x01, 0x00]; // Service 0x01, PID 0x00 (supported PIDs)
            let addressing = od.addressing();
            match od.broadcast(&request, Some(std::time::Duration::from_secs(2))) {
                Ok(responses) => {
                    if responses.is_empty() {
                        println!("{}", "No ECUs responded.".red());
                    } else {
                        println!("{} ECU(s) responded:", responses.len());
                        print_scan_results(addressing, &responses);
                    }
                }
                Err(e) => eprintln!("Scan failed: {}", e),
            }
        }
        Commands::Service { service } => {
            let car = cli.car.as_deref().ok_or("Error: Please specify car with --car")?;
            let services = get_services_for_car(car).ok_or_else(|| format!("No services found for car: {}", car))?;
            let service_fn = services.get(service.as_str()).ok_or_else(|| format!("Service '{}' not supported for car '{}'", service, car))?;
            println!("Performing service '{}' for car '{}'...", service, car);
            if let Err(e) = service_fn(&mut od) {
                eprintln!("Service failed: {}", e);
            } else {
                println!("Service '{}' completed successfully", service);
            }
        }
        Commands::ListCars => {
            println!("Supported car models:");
            for car in get_supported_cars() {
                println!("  - {}", car);
            }
        }
        Commands::ListServices => {
            println!("Available services for this car:");
            let car = cli.car.as_deref().ok_or("Error: Please specify car with --car")?;
            let services = get_services_for_car(car);
            match services {
                Some(services) => {
                    for service in services.keys() {
                        println!("  - {}", service);
                    }
                }
                None => {
                    println!("  - No services found for '{}'", car);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--id`, `--start-address` and `--sid` all route through the same parser,
    /// so a value written one way on one flag works the same way on the others.
    #[test]
    fn numeric_flags_accept_hex_and_decimal_alike() {
        assert_eq!(parse_hex_u32("0x18DA01F1").unwrap(), 0x18DA01F1);
        assert_eq!(parse_hex_u32("0X18da01f1").unwrap(), 0x18DA01F1);
        assert_eq!(parse_hex_u32("2000").unwrap(), 2000);

        assert_eq!(parse_hex_u8("0x09").unwrap(), 9);
        assert_eq!(parse_hex_u8("0X0A").unwrap(), 10);
        assert_eq!(parse_hex_u8("9").unwrap(), 9);
        assert_eq!(parse_hex_u8("0xFF").unwrap(), 255);
    }

    #[test]
    fn numeric_flags_reject_garbage_and_overflow() {
        assert!(parse_hex_u32("zz").is_err());
        assert!(parse_hex_u32("0xZZ").is_err());
        assert!(parse_hex_u8("0x100").is_err(), "wider than a byte");
        assert!(parse_hex_u8("256").is_err(), "wider than a byte");
        assert!(parse_hex_u8("0x9z").is_err(), "a typo must not read as mode 1");
    }

    #[test]
    fn hex_byte_strings_tolerate_an_optional_prefix() {
        assert_eq!(parse_hex_bytes("0x112233").unwrap(), vec![0x11, 0x22, 0x33]);
        assert_eq!(parse_hex_bytes("112233").unwrap(), vec![0x11, 0x22, 0x33]);
        assert!(parse_hex_bytes("11223").is_err(), "odd length");
    }
}
