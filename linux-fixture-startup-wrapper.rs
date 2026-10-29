use std::{
    env, fs, io,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::Command,
    thread,
    time::Duration,
};

fn main() -> io::Result<()> {
    let control = PathBuf::from(env::var_os("BEND_LINT_FIXTURE_CONTROL")
        .ok_or_else(|| io::Error::other("missing fixture control"))?);
    let original = env::var_os("BEND_LINT_FIXTURE_ORIGINAL")
        .ok_or_else(|| io::Error::other("missing original fixture"))?;
    let delay = env::var("BEND_LINT_FIXTURE_DELAY_MS")
        .map_err(io::Error::other)?
        .parse::<u64>()
        .map_err(io::Error::other)?;
    fs::write(control.join("wrapper-started"), std::process::id().to_string())?;
    thread::sleep(Duration::from_millis(delay));
    Err(Command::new(original).args(env::args_os().skip(1)).exec())
}
