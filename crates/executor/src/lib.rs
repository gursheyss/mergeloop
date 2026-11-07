//! Runs rebase jobs. A job runs the repo's code and an agent with a shell, so it is untrusted:
//! in production each job gets its own gVisor container (`DockerExecutor`). `LocalExecutor`
//! runs the job as a subprocess, for development. `RemoteExecutor` runs each job on another
//! machine: a GKE sandbox, through `sandbox::SandboxLaunch`.

pub mod job_files;
pub mod protocol;
pub mod proxy;
pub mod remote;
pub mod sandbox;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::job_files::JobFile;
use crate::protocol::{JobResult, JobSpec, RESULT_FILE};

#[async_trait]
pub trait Executor: Send + Sync {
    /// Runs one job and leaves its output (`result.json`, maybe `result.bundle`) in `out`.
    async fn run(&self, job_id: &str, spec: &JobSpec, out: &Path) -> Result<JobResult>;
}

/// Runs `<program> job --out <dir>` on this machine. No isolation.
pub struct LocalExecutor {
    pub program: PathBuf,
    pub timeout: Duration,
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn run(&self, _job_id: &str, spec: &JobSpec, out: &Path) -> Result<JobResult> {
        tokio::fs::create_dir_all(out).await?;
        let mut cmd = Command::new(&self.program);
        cmd.arg("job").arg("--out").arg(out);
        let logs = run_with_spec(cmd, spec, self.timeout).await?;
        read_result(out, &logs).await
    }
}

/// One container per job: `docker create`, `docker cp` its repo's job files in, `docker start`
/// with the spec on stdin, then `docker cp` the output out. The only bind mount is `resolv_conf`,
/// a host file, so it also works when the orchestrator itself runs in a container and talks to
/// the host's Docker.
pub struct DockerExecutor {
    pub image: String,
    /// `runsc` for gVisor. `None` uses Docker's default runtime.
    pub runtime: Option<String>,
    /// A network whose host firewall blocks the metadata server and private ranges.
    pub network: Option<String>,
    /// A file on the Docker host, mounted as the job's `/etc/resolv.conf`. Needed with gVisor on
    /// a user-defined network: there Docker's embedded DNS is out of the sandbox's reach, and
    /// `--dns` does not replace it (gVisor FAQ). `None` uses Docker's default.
    pub resolv_conf: Option<String>,
    pub memory: String,
    pub cpus: String,
    pub timeout: Duration,
    /// Streamed into the jobs of their repos, and masked in what those jobs leave behind.
    pub files: Vec<JobFile>,
}

#[async_trait]
impl Executor for DockerExecutor {
    async fn run(&self, job_id: &str, spec: &JobSpec, out: &Path) -> Result<JobResult> {
        let files = job_files::read_for(&self.files, &spec.repo).await?;
        let secrets = job_files::secrets(&files);
        let ran = self.run_container(job_id, spec, out, &files).await;
        job_files::mask_dir(out, &secrets).await?;
        match ran {
            Ok(logs) => read_result(out, &job_files::mask(&logs, &secrets)).await,
            Err(e) => Err(anyhow!("{}", job_files::mask(&format!("{e:#}"), &secrets))),
        }
    }
}

impl DockerExecutor {
    /// Returns the end of the job's log.
    async fn run_container(
        &self,
        job_id: &str,
        spec: &JobSpec,
        out: &Path,
        files: &[(String, Vec<u8>)],
    ) -> Result<String> {
        tokio::fs::create_dir_all(out).await?;
        let name = format!("mergepatrol-job-{job_id}");
        // A container left over from a crash would block the name.
        let mut rm = Command::new("docker");
        rm.args(["rm", "-f", "-v", &name]);
        let _ = checked(rm, "docker rm").await;
        // From here on, the container may exist: a job cut off during `docker create` too.
        let _container = Removed(name.clone());
        let mut create = Command::new("docker");
        // `/out` is a volume: under gVisor, writes to the container's own filesystem stay in
        // the sandbox's memory, where `docker cp` cannot see them.
        create.args(["create", "-i", "--name", &name, "-v", "/out"]);
        if let Some(runtime) = &self.runtime {
            create.args(["--runtime", runtime]);
        }
        if let Some(network) = &self.network {
            create.args(["--network", network]);
        }
        if let Some(file) = &self.resolv_conf {
            create.args(["-v", &format!("{file}:/etc/resolv.conf:ro")]);
        }
        create
            .args([
                "--memory",
                &self.memory,
                "--cpus",
                &self.cpus,
                "--pids-limit",
                "4096",
            ])
            .args(["--cap-drop", "ALL", "--security-opt", "no-new-privileges"])
            // The Docker host, where the orchestrator's Anthropic proxy listens, by default.
            .args(["--add-host", "host.docker.internal:host-gateway"])
            .arg(&self.image)
            .args(["job", "--out", "/out"]);
        let id = String::from_utf8(checked(create, "docker create").await?)?
            .trim()
            .to_owned();

        if !files.is_empty() {
            let mut cp = Command::new("docker");
            // `-a` keeps the archive's owner, the job's user.
            cp.args(["cp", "-a", "-", &format!("{id}:/")]);
            checked_with_input(cp, "docker cp the job files", job_files::archive(files)).await?;
        }

        let mut start = Command::new("docker");
        start.args(["start", "-a", "-i", &id]);
        let logs = run_with_spec(start, spec, self.timeout).await?;
        let mut cp = Command::new("docker");
        cp.args(["cp", &format!("{id}:/out/."), path_str(out)?]);
        checked(cp, "docker cp").await?;
        Ok(logs)
    }
}

/// Removes the job's container (by name), and its `/out` volume, when dropped: after the job, after an
/// error, and when the job is cut off because its queue stopped.
struct Removed(String);

impl Drop for Removed {
    fn drop(&mut self) {
        // Drop cannot wait, so `docker rm` finishes on its own; tokio reaps it.
        let removing = Command::new("docker")
            .args(["rm", "-f", "-v", &self.0])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Err(e) = removing {
            tracing::warn!(error = %e, container = %self.0, "could not remove the job container");
        }
    }
}

/// Runs `cmd` with `spec` as JSON on stdin. Returns the end of its output, for errors.
async fn run_with_spec(mut cmd: Command, spec: &JobSpec, timeout: Duration) -> Result<String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start the job")?;
    let mut stdin = child.stdin.take().context("job stdin")?;
    stdin.write_all(&serde_json::to_vec(spec)?).await?;
    drop(stdin);
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => out?,
        Err(_) => bail!("the job did not finish in {timeout:?}"),
    };
    let mut logs = String::from_utf8_lossy(&out.stdout).into_owned();
    logs.push_str(&String::from_utf8_lossy(&out.stderr));
    let tail = tail(&logs, 40);
    if !out.status.success() {
        bail!("the job crashed ({}):\n{tail}", out.status);
    }
    Ok(tail)
}

pub(crate) async fn read_result(out: &Path, logs: &str) -> Result<JobResult> {
    let json = tokio::fs::read(out.join(RESULT_FILE))
        .await
        .with_context(|| format!("the job wrote no result:\n{logs}"))?;
    serde_json::from_slice(&json).context("the job wrote a bad result")
}

/// The longest a Docker command around the job may take: a hung daemon would hold its queue.
const DOCKER_TIMEOUT: Duration = Duration::from_secs(5 * 60);

async fn checked(cmd: Command, what: &str) -> Result<Vec<u8>> {
    checked_within(cmd, what, DOCKER_TIMEOUT).await
}

async fn checked_within(mut cmd: Command, what: &str, timeout: Duration) -> Result<Vec<u8>> {
    // Dropping the command kills it.
    let output = cmd.stdin(Stdio::null()).kill_on_drop(true).output();
    let out = tokio::time::timeout(timeout, output)
        .await
        .with_context(|| format!("{what}: no result after {timeout:?}"))?
        .with_context(|| format!("{what}: spawn"))?;
    succeeded(out, what)
}

async fn checked_with_input(mut cmd: Command, what: &str, input: Vec<u8>) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("{what}: spawn"))?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let fed = async move {
        stdin.write_all(&input).await?;
        drop(stdin);
        child.wait_with_output().await
    };
    let out = tokio::time::timeout(DOCKER_TIMEOUT, fed)
        .await
        .with_context(|| format!("{what}: no result after {DOCKER_TIMEOUT:?}"))??;
    succeeded(out, what)
}

fn succeeded(out: std::process::Output, what: &str) -> Result<Vec<u8>> {
    if !out.status.success() {
        bail!("{what}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn tail(s: &str, lines: usize) -> String {
    let all: Vec<&str> = s.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn path_str(p: &Path) -> Result<&str> {
    p.to_str().context("non-UTF-8 path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hung_docker_command_is_cut_short() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = std::time::Instant::now();
        let err = checked_within(cmd, "docker cp", Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("docker cp: no result after"),
            "{err}"
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
