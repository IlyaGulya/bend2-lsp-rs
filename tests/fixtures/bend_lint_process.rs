use std::{
    env, fs,
    io::{self, Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

fn hold_socket(path: &Path) -> io::Result<TcpListener> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    // Readers treat this name as a ready signal; never expose an empty or partial address.
    let pending = path.with_extension(format!("{}.pending", std::process::id()));
    fs::write(&pending, listener.local_addr()?.to_string())?;
    fs::rename(pending, path)?;
    Ok(listener)
}

// The report a successful `prepare` leaves: the 0.2.1 identity and shape the
// server validates, with no source text.
fn prepare_report() -> &'static str {
    concat!(
        "{\"bendLint\":\"0.2.1\",\"notes\":[],\"runtime\":\"bun 1.4.4\",",
        "\"platform\":\"fixture\",\"backend\":\"native\",",
        "\"program\":\"/fixture/rules.js\",",
        "\"cache\":{\"dir\":\"/fixture/cache\",\"built\":true,",
        "\"checks\":{\"cached\":0,\"checked\":0}},",
        "\"timings\":{\"setupMs\":1,\"checkerMs\":0,\"runMs\":2},",
        "\"exitCode\":0}\n"
    )
}

fn prepare(arguments: &[String], control: &Path) -> io::Result<ExitCode> {
    let mut attempts = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(control.join("prepares"))?;
    writeln!(attempts, "{}", arguments.join("\t"))?;
    fs::write(control.join("prepare-arguments"), arguments.join("\n"))?;
    fs::write(
        control.join("prepare-cwd"),
        env::current_dir()?.to_string_lossy().as_bytes(),
    )?;
    if control.join("prepare-block").exists() {
        let listener = hold_socket(&control.join("prepare-address"))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while !control.join("prepare-release").exists() {
            if Instant::now() >= deadline {
                return Err(io::Error::other("prepare fixture gate deadline exceeded"));
            }
            thread::sleep(Duration::from_millis(10));
        }
        drop(listener);
    }
    if control.join("prepare-fail").exists() {
        eprintln!("bend-lint: fixture prepare failure");
        return Ok(ExitCode::FAILURE);
    }
    if let Ok(error) = fs::read(control.join("prepare-error")) {
        io::stdout().write_all(&error)?;
        eprintln!("bend-lint: fixture prepare rejected the project");
        return Ok(ExitCode::from(2));
    }
    if control.join("prepare-no-report").exists() {
        eprintln!("bend-lint: prepared /fixture/rules.js");
        return Ok(ExitCode::SUCCESS);
    }
    let report = arguments
        .iter()
        .position(|argument| argument == "--report")
        .and_then(|position| arguments.get(position + 1))
        .ok_or_else(|| io::Error::other("missing --report for prepare"))?;
    let body = fs::read(control.join("prepare-report.json"))
        .unwrap_or_else(|_| prepare_report().as_bytes().to_vec());
    fs::write(report, body)?;
    if control.join("prepare-stdout").exists() {
        println!("bend-lint: fixture prepare wrote stdout");
    }
    // Successful prepare writes nothing to stdout; stderr carries the identity.
    eprintln!("bend-lint: prepared /fixture/rules.js");
    Ok(ExitCode::SUCCESS)
}

fn response_file(control: &Path, arguments: &[String]) -> std::path::PathBuf {
    // A test can control each target's canned report independently, so a check
    // of document B never fabricates findings for document A.
    if let Some(target) = arguments
        .iter()
        .find(|argument| argument.ends_with(".bend"))
        && let Some(name) = Path::new(target).file_name()
    {
        let candidate = control
            .join("responses")
            .join(name)
            .with_extension("bend.json");
        if candidate.is_file() {
            return candidate;
        }
    }
    control.join("response.json")
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
        println!("bend 2.0.39");
        return Ok(ExitCode::SUCCESS);
    }
    if arguments.iter().any(|arg| arg == "--help") {
        println!("bend <file.bend> --check-only; bend base");
        return Ok(ExitCode::SUCCESS);
    }
    if arguments.iter().any(|arg| arg == "base") {
        return Ok(ExitCode::SUCCESS);
    }
    if arguments.iter().any(|arg| arg == "prepare") {
        return prepare(&arguments, control);
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
    // The server writes its overlay object and then closes stdin. Reading to EOF
    // both records the real transport and proves the write side was closed.
    let mut overlays = Vec::new();
    io::stdin().read_to_end(&mut overlays)?;
    fs::write(control.join("stdin"), &overlays)?;
    fs::write(control.join("stdin-eof"), b"eof")?;
    // Capture the response when this invocation starts: a test that changes the
    // response while a check is blocked must affect the next invocation only.
    let output = fs::read(response_file(control, &arguments))?;
    // The start barrier certifies this invocation's report has been captured.
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
    io::stdout().write_all(&output)?;
    eprintln!("bend-lint: fixture analyzer uses its own Bend 2.0.39");
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
