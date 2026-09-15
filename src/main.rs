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

mod cli;
use clap::Parser;
fn main() {
    if let Err(error) = cli::run(cli::Cli::parse()) {
        eprintln!("{}", cli::error_line(error.as_ref()));
        std::process::exit(if error.is::<cli::UsageError>() { 2 } else { 1 });
    }
}
