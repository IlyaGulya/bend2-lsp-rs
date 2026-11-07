use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

fn isolate_git(command: &mut Command, root: &Path) {
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-global-config"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file");
    for variable in [
        "GIT_CONFIG_COUNT",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(variable);
    }
}

fn git(root: &Path, arguments: &[&str]) -> Result<String, Box<dyn Error>> {
    let mut command = Command::new("git");
    isolate_git(&mut command, root);
    let output = command
        .current_dir(root)
        .args([
            "-c",
            "user.name=Source Test",
            "-c",
            "user.email=source-test@example.invalid",
        ])
        .args(arguments)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[derive(Clone, Copy)]
enum Candidate {
    Merge,
    NonUtf8Merge,
    NonMerge,
    Octopus,
}

impl Candidate {
    fn create(
        self,
        root: &Path,
        checkout: &Path,
        tree: &str,
        old_base: &str,
        base: &str,
        head: &str,
    ) -> Result<String, Box<dyn Error>> {
        let message = format!("merge candidate\n\nparent {old_base}\n");
        match self {
            Self::Merge => git(
                checkout,
                &["commit-tree", tree, "-p", base, "-p", head, "-m", &message],
            ),
            Self::NonUtf8Merge => {
                let message_path = root.join("latin1-message");
                fs::write(&message_path, b"caf\xe9\n")?;
                git(
                    checkout,
                    &[
                        "-c",
                        "i18n.commitEncoding=ISO-8859-1",
                        "commit-tree",
                        tree,
                        "-p",
                        base,
                        "-p",
                        head,
                        "-F",
                        message_path.to_str().ok_or("non-Unicode message path")?,
                    ],
                )
            }
            Self::NonMerge => Ok(head.to_owned()),
            Self::Octopus => {
                let third = git(
                    checkout,
                    &["commit-tree", tree, "-p", old_base, "-m", "third parent"],
                )?;
                git(
                    checkout,
                    &[
                        "commit-tree",
                        tree,
                        "-p",
                        base,
                        "-p",
                        head,
                        "-p",
                        &third,
                        "-m",
                        &message,
                    ],
                )
            }
        }
    }
}

struct SourceFixture {
    directory: TempDir,
    checkout: PathBuf,
    origin: String,
    server: String,
    old_base: String,
    base: String,
    head: String,
    candidate: String,
    output: PathBuf,
}

impl SourceFixture {
    fn new(kind: Candidate) -> Result<Self, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let checkout = root.join("checkout");
        fs::create_dir(&checkout)?;
        let path = root.to_str().ok_or("non-Unicode source fixture")?;
        let server = if cfg!(windows) {
            path.replace('\\', "/")
        } else {
            path.to_owned()
        };
        let origin = format!("{server}/published.git");
        git(root, &["init", "--bare", "--object-format=sha1", &origin])?;
        git(
            &checkout,
            &["init", "--object-format=sha1", "--initial-branch=tooling"],
        )?;
        let tree = git(&checkout, &["mktree"])?;
        let old_base = git(&checkout, &["commit-tree", &tree, "-m", "original main"])?;
        let base = git(
            &checkout,
            &["commit-tree", &tree, "-p", &old_base, "-m", "new main"],
        )?;
        let head = git(
            &checkout,
            &[
                "commit-tree",
                &tree,
                "-p",
                &old_base,
                "-m",
                "pull request head",
            ],
        )?;
        let candidate = kind.create(root, &checkout, &tree, &old_base, &base, &head)?;
        git(&checkout, &["update-ref", "refs/heads/main", &base])?;
        git(&checkout, &["update-ref", "refs/heads/topic", &head])?;
        git(&checkout, &["update-ref", "refs/heads/tooling", &candidate])?;
        git(&checkout, &["remote", "add", "origin", &origin])?;
        git(
            &checkout,
            &[
                "push",
                "origin",
                "refs/heads/main",
                "refs/heads/topic",
                "refs/heads/tooling",
                &format!("{candidate}:refs/pull/7/merge"),
            ],
        )?;
        let output = root.join("github-output");
        fs::write(&output, "")?;
        Ok(Self {
            directory,
            checkout,
            origin,
            server,
            old_base,
            base,
            head,
            candidate,
            output,
        })
    }

    fn validation(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-perf"));
        isolate_git(&mut command, self.directory.path());
        command
            .current_dir(&self.checkout)
            .args(["workflow", "validate"])
            // This invokes source validation only, never measurement collection.
            .env("GITHUB_ACTIONS", "true")
            .env("REQUEST_ID", "source-identity-regression")
            .env("MODE", "compare")
            .env("BASE_SHA", &self.base)
            .env("CANDIDATE_SHA", &self.candidate)
            .env("WORKFLOW_SHA", &self.candidate)
            .env("WORKFLOW_HEAD_SHA", &self.head)
            .env("SCENARIO", "discovery-1000")
            .env("SELECTED_TARGET", "x86_64-unknown-linux-gnu")
            .env("NATIVE_KIND", "cpu")
            .env_remove("MEASUREMENT_METHOD")
            .env("GITHUB_EVENT_NAME", "pull_request")
            .env("GITHUB_SERVER_URL", &self.server)
            .env("GITHUB_REPOSITORY", "published")
            .env("GITHUB_REF", "refs/heads/tooling")
            .env("PULL_NUMBER", "7")
            .env("GITHUB_OUTPUT", &self.output);
        command
    }
}

fn assert_admitted(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn exact_merge_parents_are_admitted_without_reading_message_parent_lines()
-> Result<(), Box<dyn Error>> {
    let fixture = SourceFixture::new(Candidate::Merge)?;
    assert_admitted(&fixture.validation().output()?);
    Ok(())
}

#[test]
fn merge_authority_does_not_require_utf8_commit_messages() -> Result<(), Box<dyn Error>> {
    let fixture = SourceFixture::new(Candidate::NonUtf8Merge)?;
    assert_admitted(&fixture.validation().output()?);
    Ok(())
}

#[test]
fn stale_event_base_is_rejected_before_authority_is_published() -> Result<(), Box<dyn Error>> {
    let fixture = SourceFixture::new(Candidate::Merge)?;
    let output = fixture
        .validation()
        .env("BASE_SHA", &fixture.old_base)
        .output()?;
    assert!(
        !output.status.success(),
        "a published but stale event base is not the merge candidate's baseline"
    );
    let error = String::from_utf8(output.stderr)?;
    assert!(
        error.contains(&fixture.base) && error.contains(&fixture.old_base),
        "{error}"
    );
    assert_eq!(fs::read(&fixture.output)?, b"");
    Ok(())
}

#[test]
fn mismatched_event_head_is_rejected_before_authority_is_published() -> Result<(), Box<dyn Error>> {
    let fixture = SourceFixture::new(Candidate::Merge)?;
    let output = fixture
        .validation()
        .env("WORKFLOW_HEAD_SHA", &fixture.old_base)
        .output()?;
    assert!(
        !output.status.success(),
        "the second merge parent must be the exact event head"
    );
    let error = String::from_utf8(output.stderr)?;
    assert!(
        error.contains(&fixture.head) && error.contains(&fixture.old_base),
        "{error}"
    );
    assert_eq!(fs::read(&fixture.output)?, b"");
    Ok(())
}

#[test]
fn pull_request_candidates_must_have_exactly_two_parents() -> Result<(), Box<dyn Error>> {
    for candidate in [Candidate::NonMerge, Candidate::Octopus] {
        let fixture = SourceFixture::new(candidate)?;
        let output = fixture.validation().output()?;
        assert!(
            !output.status.success(),
            "a published commit is not sufficient PR merge authority"
        );
        assert_eq!(fs::read(&fixture.output)?, b"");
    }
    Ok(())
}

#[test]
fn current_callgrind_request_is_admitted_without_loosening_other_scenarios()
-> Result<(), Box<dyn Error>> {
    let fixture = SourceFixture::new(Candidate::Merge)?;
    let mut command = fixture.validation();
    command
        .env("MODE", "callgrind")
        .env("SCENARIO", "analysis-hot-paths")
        .env("MEASUREMENT_METHOD", "paired-median-105-v1");
    assert_admitted(&command.output()?);
    let wrong_scenario = command.env("SCENARIO", "discovery-1000").output()?;
    assert!(!wrong_scenario.status.success());
    Ok(())
}

#[test]
fn a_shallow_merge_checkout_still_admits_the_exact_source_pair() -> Result<(), Box<dyn Error>> {
    let fixture = SourceFixture::new(Candidate::Merge)?;
    let shallow = fixture.directory.path().join("shallow");
    git(
        fixture.directory.path(),
        &[
            "clone",
            "--no-local",
            "--depth=1",
            "--branch=tooling",
            &fixture.origin,
            shallow.to_str().ok_or("shallow checkout path")?,
        ],
    )?;
    assert_eq!(
        git(&shallow, &["rev-parse", "--is-shallow-repository"])?,
        "true"
    );
    assert_admitted(&fixture.validation().current_dir(&shallow).output()?);
    Ok(())
}

#[test]
fn explicit_dispatch_preserves_named_non_merge_comparisons() -> Result<(), Box<dyn Error>> {
    let fixture = SourceFixture::new(Candidate::NonMerge)?;
    assert_admitted(
        &fixture
            .validation()
            .env("GITHUB_EVENT_NAME", "workflow_dispatch")
            .output()?,
    );
    Ok(())
}
