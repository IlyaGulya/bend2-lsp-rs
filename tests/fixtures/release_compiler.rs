use std::{
    env, fs, io, net::TcpListener, path::PathBuf, process::ExitCode, thread, time::Duration,
};

fn run() -> io::Result<ExitCode> {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    if arguments.len() != 3 || arguments[2] != "--check-only" {
        return Err(io::Error::other(
            "expected control directory, source, --check-only",
        ));
    }
    let control = PathBuf::from(&arguments[0]);
    let entry = PathBuf::from(&arguments[1]);
    let source = fs::read_to_string(&entry)?;
    if source.contains("Dep.clamp(1)") {
        let parent = entry
            .parent()
            .ok_or_else(|| io::Error::other("source parent missing"))?;
        let dependency = fs::read_to_string(parent.join("dep.bend"))?;
        if !dependency.starts_with("def clamp(x: U32) -> U32:") {
            return Err(io::Error::other(
                "compiler did not receive unsaved dependency",
            ));
        }
    }
    fs::write(control.join("observed-source"), &source)?;
    fs::write(
        control.join("observed-path"),
        entry.to_string_lossy().as_bytes(),
    )?;
    if let Some(key) = source
        .lines()
        .find_map(|line| line.strip_prefix("# block "))
    {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let pending = control.join(format!("{key}.pending"));
        fs::write(&pending, listener.local_addr()?.to_string())?;
        fs::rename(pending, control.join(format!("{key}.started")))?;
        // Owning this listener provides a portable observable child lifetime,
        // including on Windows, without shells, signals, or process-list tools.
        loop {
            thread::sleep(Duration::from_secs(1));
            let _ = listener.local_addr()?;
        }
    }
    if source.lines().any(|line| line == "# error") {
        eprintln!("Error:\nportable compiler rejection\nLocation:");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("portable compiler fixture: {error}");
            ExitCode::FAILURE
        }
    }
}
