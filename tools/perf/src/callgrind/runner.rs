//! Bounded original-runner execution within one sequentially collected variant.
use super::{collect::Environment, sha256_bytes};
use crate::{ToolResult, common::write_json};
use serde_json::json;
use std::{
    ffi::OsString,
    fs::{self, File},
    io::Write,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, MutexGuard},
    thread,
};

pub(crate) const MAX_PARALLELISM: usize = 4;

pub(crate) struct RunnerJob {
    pub command: Vec<OsString>,
    pub input: Vec<u8>,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

#[derive(Default)]
struct Queue {
    next: usize,
    error: Option<Box<dyn std::error::Error + Send + Sync>>,
}

fn lock(queue: &Mutex<Queue>) -> MutexGuard<'_, Queue> {
    // A worker panic must not prevent its peers from stopping or being reaped.
    queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn fail(queue: &Mutex<Queue>, error: Box<dyn std::error::Error + Send + Sync>) {
    lock(queue).error.get_or_insert(error);
}

struct Running<'a> {
    child: Child,
    queue: &'a Mutex<Queue>,
    reaped: bool,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            // Publish cancellation before waiting during unwinding, not afterward.
            fail(self.queue, "Callgrind runner worker panicked".into());
        }
        if !self.reaped {
            // Closing stdin is essential if unwinding interrupted descriptor delivery.
            drop(self.child.stdin.take());
            if let Err(error) = self.child.wait() {
                fail(self.queue, error.into());
            }
        }
    }
}

fn spawn(job: &RunnerJob, cwd: &Path, environment: &Environment) -> ToolResult<Child> {
    let (executable, arguments) = job.command.split_first().ok_or("empty runner command")?;
    let argv = job
        .command
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .ok_or("runner command argument is not UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    for output in [&job.stdout, &job.stderr] {
        if let Some(parent) = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
    }
    write_json(
        &job.stdout.with_extension("command.json"),
        &json!({"argv": argv, "cwd": cwd, "input_bytes": job.input.len(),
            "input_sha256": sha256_bytes(&job.input)}),
    )?;
    // Inherit the workflow supervisor's process group on every platform.
    Ok(Command::new(executable)
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::piped())
        .stdout(File::create(&job.stdout)?)
        .stderr(File::create(&job.stderr)?)
        .spawn()?)
}

fn worker(jobs: &[RunnerJob], cwd: &Path, environment: &Environment, queue: &Mutex<Queue>) {
    loop {
        let (job, mut running) = {
            let mut state = lock(queue);
            if state.error.is_some() || state.next == jobs.len() {
                return;
            }
            let job = &jobs[state.next];
            state.next += 1;
            // Serialize reservation and spawn with failure publication. No queued
            // reservation can turn into a late launch after cancellation.
            match spawn(job, cwd, environment) {
                Ok(child) => (
                    job,
                    Running {
                        child,
                        queue,
                        reaped: false,
                    },
                ),
                Err(error) => {
                    state.error = Some(error);
                    return;
                }
            }
        };
        let input = (|| -> ToolResult<()> {
            let mut stdin = running
                .child
                .stdin
                .take()
                .ok_or("runner stdin was not piped")?;
            stdin.write_all(&job.input)?;
            // EOF frames the descriptor; close before waiting, including on error.
            Ok(())
        })();
        if let Err(error) = input {
            fail(
                queue,
                format!(
                    "runner descriptor delivery failed: {error}; command {:?}; see {} and {}",
                    job.command,
                    job.stdout.display(),
                    job.stderr.display()
                )
                .into(),
            );
        }
        match running.child.wait() {
            Ok(status) => {
                running.reaped = true;
                if !status.success() {
                    fail(
                        queue,
                        format!(
                            "runner exited {status}: {:?}; see {} and {}",
                            job.command,
                            job.stdout.display(),
                            job.stderr.display()
                        )
                        .into(),
                    );
                }
            }
            Err(error) => fail(queue, error.into()),
        }
    }
}

pub(crate) fn run_jobs(
    jobs: &[RunnerJob],
    cwd: &Path,
    environment: &Environment,
    parallelism: usize,
) -> ToolResult<()> {
    if !(1..=MAX_PARALLELISM).contains(&parallelism) {
        return Err(format!("runner parallelism must be 1..={MAX_PARALLELISM}").into());
    }
    let queue = Mutex::new(Queue::default());
    thread::scope(|scope| {
        let mut workers = Vec::with_capacity(parallelism.min(jobs.len()));
        for _ in 0..parallelism.min(jobs.len()) {
            let queue = &queue;
            match thread::Builder::new().spawn_scoped(scope, move || {
                if catch_unwind(AssertUnwindSafe(|| worker(jobs, cwd, environment, queue))).is_err()
                {
                    fail(queue, "Callgrind runner worker panicked".into());
                }
            }) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    fail(queue, error.into());
                    break;
                }
            }
        }
        // Joining is unconditional: an error never leaves a runner or stdin writer
        // alive while the caller quarantines raw output or starts another variant.
        for worker in workers {
            if worker.join().is_err() {
                fail(&queue, "Callgrind runner worker panicked".into());
            }
        }
    });
    match queue
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .error
    {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::{
        collections::BTreeSet,
        sync::mpsc,
        time::{Duration, Instant},
    };

    const FIXTURE_ROOT: &str = "BEND2_PERF_RUNNER_TEST_ROOT";

    #[derive(Serialize)]
    struct Request {
        id: usize,
        fail: bool,
        payload: Vec<u8>,
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "real runner child fixture timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    // Real shell processes consume the binary descriptor and validate its exact
    // bytes independently, then expose controlled lifecycle events to the test.
    // This is fixture code, not an environment-gated/no-op test in the harness.
    const CHILD: &str = r#"
set -eu
cd "$BEND2_PERF_RUNNER_TEST_ROOT"
id=$1
fail=$2
/bin/cat > "received-$id"
/usr/bin/cmp "expected-$id" "received-$id"
printf '%s' "$$" > "pid-$id"
event() {
    attempts=0
    until /bin/mkdir events.lock 2>/dev/null; do
        attempts=$((attempts + 1))
        [ "$attempts" -lt 1000 ] || exit 92
        /bin/sleep 0.01
    done
    printf '%s %s\n' "$1" "$id" >> events
    /bin/rmdir events.lock
}
event start
printf ready > "started-$id"
printf 'partial raw stderr for %s\n' "$id" >&2
attempts=0
while [ ! -f "release-$id" ]; do
    attempts=$((attempts + 1))
    [ "$attempts" -lt 1000 ] || exit 93
    /bin/sleep 0.01
done
# A late writer makes premature return/quarantine observable.
/bin/sleep 0.04
printf 'completed raw stdout for %s\n' "$id"
printf 'completed raw stderr for %s\n' "$id" >&2
event end
printf finished > "done-$id"
if [ "$fail" = true ]; then
    exit 47
fi
"#;

    fn environment(root: &Path) -> Environment {
        let mut environment: Environment = std::env::vars_os().collect();
        environment.insert(FIXTURE_ROOT.into(), root.as_os_str().to_owned());
        environment
    }

    fn job(root: &Path, id: usize, fail: bool) -> ToolResult<RunnerJob> {
        let input = bincode::serialize(&Request {
            id,
            fail,
            payload: vec![0, 255, 17, 0, 128, 42],
        })?;
        fs::write(root.join(format!("expected-{id}")), &input)?;
        Ok(RunnerJob {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                CHILD.into(),
                "runner-child-fixture".into(),
                id.to_string().into(),
                fail.to_string().into(),
            ],
            input,
            stdout: root.join(format!("{id}.stdout")),
            stderr: root.join(format!("{id}.stderr")),
        })
    }

    struct ReleaseAll<'a> {
        root: &'a Path,
        count: usize,
    }

    impl Drop for ReleaseAll<'_> {
        fn drop(&mut self) {
            for id in 0..self.count {
                let _ = fs::write(self.root.join(format!("release-{id}")), b"go");
            }
        }
    }

    fn alive(pid: &str) -> bool {
        Command::new("/bin/kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("checking real child liveness")
            .success()
    }

    fn assert_reaped(root: &Path, ids: impl IntoIterator<Item = usize>) -> ToolResult<()> {
        for id in ids {
            let pid = fs::read_to_string(root.join(format!("pid-{id}")))?;
            assert!(
                !alive(&pid),
                "runner child {id} was alive or unreaped on return"
            );
        }
        Ok(())
    }

    #[test]
    fn real_children_overlap_with_a_bounded_dynamic_queue() -> ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let environment = environment(root);
        let jobs = (0..8)
            .map(|id| job(root, id, false))
            .collect::<ToolResult<Vec<_>>>()?;
        thread::scope(|scope| -> ToolResult<()> {
            let handle = scope.spawn(|| run_jobs(&jobs, root, &environment, MAX_PARALLELISM));
            let release = ReleaseAll {
                root,
                count: jobs.len(),
            };
            wait_until(|| (0..4).all(|id| root.join(format!("started-{id}")).exists()));
            assert!(
                !root.join("started-4").exists(),
                "more than four runners launched"
            );
            fs::write(root.join("release-1"), b"go")?;
            wait_until(|| root.join("started-4").exists());
            // Jobs 0, 2 and 3 are still blocked: job 4 must not wait for a wave.
            for id in [0, 2, 3] {
                assert!(!root.join(format!("done-{id}")).exists());
            }
            drop(release);
            handle.join().expect("runner coordinator panicked")
        })?;
        let mut active = BTreeSet::new();
        let mut maximum = 0;
        for record in fs::read_to_string(root.join("events"))?.lines() {
            let (kind, id) = record.split_once(' ').ok_or("bad fixture event")?;
            if kind == "start" {
                assert!(active.insert(id.to_owned()));
                maximum = maximum.max(active.len());
            } else {
                assert_eq!(kind, "end");
                assert!(active.remove(id));
            }
        }
        assert_eq!(maximum, MAX_PARALLELISM);
        assert!(active.is_empty());
        assert_reaped(root, 0..jobs.len())?;
        for job in &jobs {
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(job.stdout.with_extension("command.json"))?)?;
            let argv: Vec<_> = job
                .command
                .iter()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            assert_eq!(manifest["argv"], json!(argv));
            assert_eq!(manifest["cwd"], json!(root));
            assert_eq!(manifest["input_sha256"], sha256_bytes(&job.input));
            assert!(fs::read_to_string(&job.stdout)?.contains("completed raw stdout"));
        }
        Ok(())
    }

    #[test]
    fn child_failure_stops_dispatch_and_joins_every_late_writer() -> ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let environment = environment(root);
        let jobs = vec![
            job(root, 0, true)?,
            job(root, 1, false)?,
            job(root, 2, false)?,
        ];
        thread::scope(|scope| -> ToolResult<()> {
            let (sender, receiver) = mpsc::channel();
            let borrowed_jobs = &jobs;
            let borrowed_environment = &environment;
            let handle = scope.spawn(move || {
                sender
                    .send(run_jobs(borrowed_jobs, root, borrowed_environment, 2))
                    .unwrap();
            });
            let release = ReleaseAll {
                root,
                count: jobs.len(),
            };
            wait_until(|| (0..2).all(|id| root.join(format!("started-{id}")).exists()));
            fs::write(root.join("release-0"), b"go")?;
            let pid = fs::read_to_string(root.join("pid-0"))?;
            wait_until(|| !alive(&pid));
            assert!(
                matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "failure returned while another real child was still blocked"
            );
            fs::write(root.join("release-1"), b"go")?;
            let result = receiver.recv_timeout(Duration::from_secs(10))?;
            assert!(result.is_err());
            handle.join().expect("runner coordinator panicked");
            drop(release);
            Ok(())
        })?;
        assert!(!root.join("started-2").exists());
        assert!(!jobs[2].stdout.with_extension("command.json").exists());
        assert!(root.join("done-1").exists());
        assert_reaped(root, [0, 1])?;
        let raw = fs::read(&jobs[1].stderr)?;
        assert!(String::from_utf8_lossy(&raw).contains("completed raw stderr for 1"));
        assert!(fs::read_to_string(&jobs[0].stderr)?.contains("partial raw stderr for 0"));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            raw,
            fs::read(&jobs[1].stderr)?,
            "raw evidence changed after failure returned"
        );
        Ok(())
    }

    #[test]
    fn spawn_failure_drains_an_already_running_child() -> ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let environment = environment(root);
        let mut missing = job(root, 1, false)?;
        missing.command = vec![root.join("nonexistent-original-runner").into_os_string()];
        let jobs = vec![job(root, 0, false)?, missing, job(root, 2, false)?];
        thread::scope(|scope| -> ToolResult<()> {
            let (sender, receiver) = mpsc::channel();
            let borrowed_jobs = &jobs;
            let borrowed_environment = &environment;
            let handle = scope.spawn(move || {
                sender
                    .send(run_jobs(borrowed_jobs, root, borrowed_environment, 2))
                    .unwrap();
            });
            let release = ReleaseAll {
                root,
                count: jobs.len(),
            };
            wait_until(|| root.join("started-0").exists());
            wait_until(|| jobs[1].stdout.with_extension("command.json").exists());
            assert!(
                matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "spawn failure abandoned an already running child"
            );
            drop(release);
            assert!(receiver.recv_timeout(Duration::from_secs(10))?.is_err());
            handle.join().expect("runner coordinator panicked");
            Ok(())
        })?;
        assert!(root.join("done-0").exists());
        assert_reaped(root, [0])?;
        assert!(!jobs[2].stdout.with_extension("command.json").exists());
        assert!(fs::read_to_string(&jobs[0].stdout)?.contains("completed raw stdout for 0"));
        Ok(())
    }

    #[test]
    fn broken_stdin_is_an_error_and_still_reaps_the_original_child() -> ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let environment = environment(root);
        let mut first = job(root, 0, false)?;
        first.command = vec!["/bin/sh".into(), "-c".into(),
            "exec 0<&-; printf '%s' \"$$\" > pid-0; /bin/sleep 0.15; printf 'late raw stderr\\n' >&2; printf finished > done-0".into()];
        // Far larger than the pipe buffer, so the descriptor cannot be fully sent
        // before this real child closes stdin, even if the child is scheduled late.
        first.input = bincode::serialize(&vec![19_u8; 2 * 1024 * 1024])?;
        let jobs = vec![first, job(root, 1, false)?];
        let result = run_jobs(&jobs, root, &environment, 1);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("descriptor delivery failed")
        );
        assert!(
            root.join("done-0").exists(),
            "stdin failure returned before the child finished"
        );
        assert_reaped(root, [0])?;
        assert!(!jobs[1].stdout.with_extension("command.json").exists());
        assert_eq!(fs::read_to_string(&jobs[0].stderr)?, "late raw stderr\n");
        Ok(())
    }

    #[test]
    fn invalid_parallelism_cannot_launch_a_child() -> ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let jobs = vec![job(root, 0, false)?];
        for parallelism in [0, MAX_PARALLELISM + 1, usize::MAX] {
            assert!(run_jobs(&jobs, root, &environment(root), parallelism).is_err());
        }
        assert!(!jobs[0].stdout.exists());
        assert!(!jobs[0].stdout.with_extension("command.json").exists());
        Ok(())
    }
}
