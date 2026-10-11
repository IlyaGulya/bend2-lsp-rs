use std::process::Command;

#[test]
fn collection_requires_ci_before_accessing_measurement_inputs()
-> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args([
            "native",
            "latency",
            "--baseline-binary",
            "missing-baseline",
            "--candidate-binary",
            "missing-candidate",
            "--baseline-output",
            "baseline.json",
            "--candidate-output",
            "candidate.json",
        ])
        .env_remove("GITHUB_ACTIONS")
        .output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("hosted CI"),
        "collection did not fail at CI boundary: {stderr}"
    );
    assert!(!stderr.contains("No such file"));
    Ok(())
}

#[test]
fn zero_samples_and_unknown_flags_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
    for extra in [
        ["--samples", "0"],
        ["--rounds", "0"],
        ["--warmup", "-1"],
        ["--unknown", "1"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
            .args([
                "native",
                "latency",
                "--baseline-binary",
                "missing-baseline",
                "--candidate-binary",
                "missing-candidate",
                "--baseline-output",
                "baseline.json",
                "--candidate-output",
                "candidate.json",
            ])
            .args(extra)
            .env_remove("GITHUB_ACTIONS")
            .output()?;
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            !stderr.contains("hosted CI"),
            "invalid CLI accepted before collection guard: {stderr}"
        );
    }
    Ok(())
}
