use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    let directory = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_opendiag"))
        .args([
            "--config",
            directory.path().join("absent.toml").to_str().unwrap(),
            "--driver",
            "/missing/opendiag-driver",
        ])
        .args(args)
        .env_remove("OPENDIAG_J2534_LIB")
        .output()
        .unwrap()
}

#[test]
fn usage_errors_exit_two_before_loading_the_adapter() {
    for args in [
        vec!["--vehicle", "unknown", "service", "list"],
        vec!["--vehicle", "honda/37820-RGW", "vehicle", "show"],
        vec![
            "--vehicle",
            "honda/odyssey-rl3",
            "service",
            "run",
            "unknown",
        ],
        vec!["--protocol", "unknown", "ecu", "scan"],
        vec!["did", "scan", "--start", "2", "--end", "1"],
    ] {
        let output = run(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("Failed to load"));
    }
}

#[test]
fn adapter_failures_exit_one_and_offer_setup_help() {
    for command in [
        ["adapter", "voltage"],
        ["adapter", "reset"],
        ["ecu", "scan"],
    ] {
        let output = run(&command);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("config init"));
    }
}

#[test]
fn iso9141_and_the_accord_profile_are_accepted_before_adapter_loading() {
    for args in [
        vec!["--protocol", "iso9141", "ecu", "scan"],
        vec!["--protocol", "iso9141", "ecu", "info", "0x09"],
        vec!["--vehicle", "honda/accord-2005", "info"],
    ] {
        let output = run(&args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("config init"));
    }
    let output = run(&["--protocol", "iso9141", "ecu", "info", "0x100"]);
    assert_eq!(output.status.code(), Some(2));
    let output = run(&["vehicle", "show", "honda/accord-2005"]);
    assert!(output.status.success());
}

#[test]
fn kwp_modes_are_accepted_and_validate_legacy_ecu_addresses() {
    for protocol in ["kwp-5baud", "kwp-fast"] {
        for command in [
            vec!["ecu", "scan"],
            vec!["ecu", "info", "0x11"],
            vec!["live", "--once"],
        ] {
            let mut args = vec!["--protocol", protocol];
            args.extend(command);
            let output = run(&args);
            assert_eq!(output.status.code(), Some(1));
            assert!(String::from_utf8_lossy(&output.stderr).contains("config init"));
        }
        let output = run(&["--protocol", protocol, "ecu", "info", "0x100"]);
        assert_eq!(output.status.code(), Some(2));
    }
}

#[test]
fn pwm_and_the_manual_ford_profile_are_accepted_without_a_vin() {
    for args in [
        vec!["--protocol", "j1850pwm", "ecu", "scan"],
        vec!["--protocol", "j1850pwm", "ecu", "info", "0x10"],
        vec![
            "--vehicle",
            "ford/f150-1997",
            "live",
            "--once",
            "ENGINE_RPM",
        ],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("config init"));
    }
    let output = run(&["--protocol", "j1850pwm", "ecu", "info", "0x100"]);
    assert_eq!(output.status.code(), Some(2));
    let output = run(&["vehicle", "show", "ford/f150-1997"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("ML2-355"));
}

#[test]
fn vpw_and_the_suburban_profile_are_available() {
    for args in [
        vec!["--protocol", "j1850vpw", "identify"],
        vec!["--protocol", "j1850vpw", "ecu", "info", "0x10"],
        vec!["--vehicle", "chevrolet/suburban-2002", "info"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("config init"));
    }
    let output = run(&["vehicle", "show", "chevrolet/suburban-2002"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("2002") && text.contains("L59") && text.contains("12200411"));
    let output = run(&["--protocol", "j1850vpw", "ecu", "info", "0x100"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn vehicle_and_service_lists_are_offline_and_come_from_embedded_data() {
    let output = run(&["vehicle", "list"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("honda/odyssey-rl3"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("honda/37820-RGW"));
    let output = run(&["--vehicle", "honda/odyssey-rl3", "service", "list"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Unidentified ECU"));
    assert!(!text.contains("engine.idle-relearn"));
    assert!(!text.contains("abs-bleed") && !text.contains("pair-immobilizer"));
    let output = run(&[
        "--vehicle",
        "honda/odyssey-rl3",
        "service",
        "run",
        "idle-relearn",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not implemented"));
}

#[test]
fn vehicle_year_filters_include_boundaries_and_show_named_ranges() {
    for year in ["2006", "2007", "2009", "2010"] {
        let output = run(&["vehicle", "list", "--year", year]);
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            text.contains("honda/crv-re3"),
            matches!(year, "2007" | "2009")
        );
        assert!(text.contains("honda/odyssey-rl3"));
    }
    let output = run(&["vehicle", "show", "honda/crv-re3"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("(2007–2009)"));
    assert!(text.contains("engine: Engine (PCM, 37820-RZA)"));
    assert!(text.contains("transmission: Transmission (TCU)"));
}

#[test]
fn raw_firmware_needs_explicit_vehicle_before_loading_a_driver() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), [1; 32]).unwrap();
    let output = run(&[
        "flash",
        file.path().to_str().unwrap(),
        "--ecu",
        "0x18DA11F1",
        "--start-address",
        "0x10000",
        "--security-key",
        "11111111",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--vehicle"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Failed to load"));
    let output = run(&[
        "--vehicle",
        "honda/odyssey-rl3",
        "flash",
        file.path().to_str().unwrap(),
        "--start-address",
        "0x10000",
        "--security-key",
        "11111111",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("config init"));
}
