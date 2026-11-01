use std::{
    env, fs,
    io::{self, Write},
    net::TcpListener,
    path::Path,
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

fn hold_socket(path: &Path) -> io::Result<TcpListener> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    // File visibility is the parent's readiness signal; never expose partial text.
    let pending = path.with_extension("pending");
    fs::write(&pending, listener.local_addr()?.to_string())?;
    fs::rename(pending, path)?;
    Ok(listener)
}

fn run() -> io::Result<ExitCode> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    if arguments.first().is_some_and(|arg| arg == "--descendant") {
        let path = arguments
            .get(1)
            .ok_or_else(|| io::Error::other("missing descendant address path"))?;
        let listener = hold_socket(Path::new(path))?;
        thread::sleep(Duration::from_secs(30));
        drop(listener);
        return Ok(ExitCode::SUCCESS);
    }
    let control = env::var_os("BEND_LINT_FIXTURE_CONTROL")
        .ok_or_else(|| io::Error::other("missing fixture control directory"))?;
    let control = Path::new(&control);
    if arguments.iter().any(|arg| arg == "version") {
        println!("bend 2.0.37");
        return Ok(ExitCode::SUCCESS);
    }
    if arguments.iter().any(|arg| arg == "--help") {
        println!("bend <file.bend> --check-only; bend base");
        return Ok(ExitCode::SUCCESS);
    }
    if arguments.iter().any(|arg| arg == "base") {
        return Ok(ExitCode::SUCCESS);
    }
    if arguments.iter().any(|arg| arg == "--check-only") {
        if control.join("compiler-block").exists() {
            let listener = hold_socket(&control.join("compiler-address"))?;
            let deadline = Instant::now() + Duration::from_secs(30);
            while !control.join("compiler-release").exists() {
                if Instant::now() >= deadline {
                    return Err(io::Error::other("compiler fixture gate deadline exceeded"));
                }
                thread::sleep(Duration::from_millis(10));
            }
            drop(listener);
        }
        if let Ok(output) = fs::read(control.join("compiler.stderr")) {
            io::stderr().write_all(&output)?;
            return Ok(ExitCode::FAILURE);
        }
        return Ok(ExitCode::SUCCESS);
    }
    let mut calls = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(control.join("calls"))?;
    writeln!(calls, "{}", arguments.join("\t"))?;
    fs::write(control.join("cwd"), env::current_dir()?.to_string_lossy().as_bytes())?;
    fs::write(control.join("arguments"), arguments.join("\n"))?;
    let listener = hold_socket(&control.join("address"))?;
    // The child owns a distinct socket: killing only the immediate analyzer
    // process must not look like successful process-tree cleanup.
    let mut descendant = if control.join("spawn-descendant").exists() {
        Some(
            Command::new(env::current_exe()?)
                .arg("--descendant")
                .arg(control.join("descendant-address"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        )
    } else {
        None
    };
    fs::write(control.join("started"), b"ready")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while control.join("block").exists() && !control.join("release").exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::other("fixture gate deadline exceeded"));
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = fs::read(control.join("response.json"))?;
    io::stdout().write_all(&output)?;
    eprintln!("bend-lint: fixture analyzer uses its own Bend 2.0.37");
    drop(listener);
    if let Some(child) = descendant.as_mut() {
        child.kill()?;
        child.wait()?;
    }
    let status = fs::read_to_string(control.join("exit-code"))?
        .trim()
        .parse::<u8>()
        .map_err(io::Error::other)?;
    Ok(ExitCode::from(status))
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("bend-lint fixture: {error}");
            ExitCode::FAILURE
        }
    }
}
