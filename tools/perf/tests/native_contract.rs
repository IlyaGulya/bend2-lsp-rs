use std::{error::Error, process::Command};

const REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn discovery_args() -> Vec<&'static str> {
    vec![
        "native",
        "discovery",
        "--baseline-binary",
        "missing-baseline",
        "--candidate-binary",
        "missing-candidate",
        "--baseline-revision",
        REVISION,
        "--candidate-revision",
        REVISION,
        "--output-dir",
        "missing-output",
    ]
}

#[test]
fn discovery_and_memory_refuse_local_measurements_before_touching_inputs()
-> Result<(), Box<dyn Error>> {
    let commands = [
        discovery_args(),
        vec![
            "native",
            "memory",
            "collect",
            "--binary-dir",
            "missing-binaries",
            "--provenance",
            "missing-provenance",
            "--output-dir",
            "missing-output",
        ],
        vec![
            "native",
            "memory",
            "provenance",
            "--baseline-revision",
            REVISION,
            "--candidate-revision",
            REVISION,
            "--commands-file",
            "missing-commands",
            "--binary-dir",
            "missing-binaries",
            "--output",
            "missing-output",
        ],
    ];
    let directory = tempfile::tempdir()?;
    for args in commands {
        let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
            .args(args)
            .current_dir(directory.path())
            .env_remove("GITHUB_ACTIONS")
            .output()?;
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains("hosted CI"),
            "CI guard not reached: {stderr}"
        );
        assert!(
            !directory.path().join("missing-output").exists(),
            "Local command touched evidence outputs"
        );
    }
    Ok(())
}

#[test]
fn native_help_is_safe_locally_and_preserves_original_commands() -> Result<(), Box<dyn Error>> {
    for args in [
        vec!["native", "discovery", "--help"],
        vec!["native", "memory", "--help"],
        vec!["native", "memory", "collect", "--help"],
        vec!["native", "memory", "provenance", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
            .args(args)
            .env_remove("GITHUB_ACTIONS")
            .output()?;
        assert!(
            output.status.success(),
            "Help unexpectedly requires collection: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout)?;
        assert!(!stdout.contains("stamp"));
        assert!(!stdout.contains("verify"));
    }
    Ok(())
}

#[test]
fn discovery_cli_rejects_invalid_numbers_and_revision_before_ci_guard() -> Result<(), Box<dyn Error>>
{
    for extra in [
        ["--samples", "0"],
        ["--rounds", "0"],
        ["--warmup", "-1"],
        ["--discovery-timeout", "NaN"],
        ["--discovery-timeout", "inf"],
        ["--discovery-timeout", "0"],
        ["--unknown", "1"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
            .args(discovery_args())
            .args(extra)
            .env_remove("GITHUB_ACTIONS")
            .output()?;
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            !stderr.contains("hosted CI"),
            "Invalid CLI bypassed parser: {stderr}"
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args([
            "native",
            "memory",
            "provenance",
            "--baseline-revision",
            "main",
            "--candidate-revision",
            REVISION,
            "--commands-file",
            "missing",
            "--output",
            "missing",
        ])
        .env_remove("GITHUB_ACTIONS")
        .output()?;
    assert!(!output.status.success());
    assert!(!String::from_utf8(output.stderr)?.contains("hosted CI"));
    Ok(())
}
