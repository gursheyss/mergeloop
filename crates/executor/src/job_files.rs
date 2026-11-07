//! Files the orchestrator gives the jobs of one repo, such as an `.npmrc` with a registry token.
//! `DockerExecutor` streams them into the container between `docker create` and `docker start`,
//! owned by the job's user and readable only by it, and masks their values in everything the
//! job leaves behind: its output files, the end of its log, and its errors.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::protocol::BUNDLE_FILE;

/// The job image's `node` user and group.
const JOB_UID: u64 = 1000;
/// Read-only, and only for the job's user.
const JOB_FILE_MODE: u64 = 0o400;
/// A value shorter than this is too common to mask.
const MIN_SECRET: usize = 8;
pub const MASK: &str = "***";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobFile {
    /// `owner/name`: the repo whose jobs get the file.
    pub repo: String,
    /// A file the orchestrator reads, again for each job, so a rotated file needs no restart.
    pub source: PathBuf,
    /// An absolute path in the job, in a directory the image has.
    pub target: String,
}

impl JobFile {
    /// `MERGEPATROL_JOB_FILES`: `owner/name:<source>:<target>` entries, separated by commas.
    pub fn parse_list(s: &str) -> Result<Vec<Self>> {
        s.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let mut parts = entry.splitn(3, ':');
                let (Some(repo), Some(source), Some(target)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    bail!("{entry:?} is not owner/name:<source>:<target>");
                };
                if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
                    bail!("{entry:?}: {repo:?} is not owner/name");
                }
                for path in [source, target] {
                    if !path.starts_with('/') || path.ends_with('/') {
                        bail!("{entry:?}: {path:?} is not an absolute file path");
                    }
                }
                if target.len() > 100 {
                    bail!("{entry:?}: the target path is longer than 100 bytes");
                }
                Ok(Self {
                    repo: repo.into(),
                    source: source.into(),
                    target: target.into(),
                })
            })
            .collect()
    }
}

/// The files of `repo`'s jobs, read now: each target with its contents.
pub(crate) async fn read_for(files: &[JobFile], repo: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let mut read = Vec::new();
    for file in files.iter().filter(|f| f.repo.eq_ignore_ascii_case(repo)) {
        let contents = tokio::fs::read(&file.source)
            .await
            .with_context(|| format!("read the job file {}", file.source.display()))?;
        read.push((file.target.clone(), contents));
    }
    Ok(read)
}

/// A tar archive of `files`, to extract at `/`: each owned by the job's user, mode 0400.
pub(crate) fn archive(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut tar = Vec::new();
    for (target, contents) in files {
        let mut header = [0u8; 512];
        let name = target.trim_start_matches('/').as_bytes();
        header[..name.len()].copy_from_slice(name);
        octal(&mut header[100..108], JOB_FILE_MODE);
        octal(&mut header[108..116], JOB_UID);
        octal(&mut header[116..124], JOB_UID);
        octal(&mut header[124..136], contents.len() as u64);
        octal(&mut header[136..148], 0);
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].fill(b' ');
        let sum: u64 = header.iter().map(|&b| u64::from(b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        tar.extend_from_slice(&header);
        tar.extend_from_slice(contents);
        tar.resize(tar.len().next_multiple_of(512), 0);
    }
    tar.resize(tar.len() + 1024, 0);
    tar
}

fn octal(field: &mut [u8], n: u64) {
    let digits = field.len() - 1;
    field.copy_from_slice(format!("{n:0digits$o}\0").as_bytes());
}

/// What must not leave a job: each line's value after its first `=`, as in
/// `//npm.pkg.github.com/:_authToken=<token>`, or the whole line when it has none. Also each
/// value as a JSON string spells it, for transcripts. Longest first, so no mask splits another.
pub(crate) fn secrets(files: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut secrets = Vec::new();
    for (_, contents) in files {
        for line in String::from_utf8_lossy(contents).lines() {
            let line = line.trim();
            if line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let value = line.split_once('=').map_or(line, |(_, v)| v);
            let value = value.trim().trim_matches(['"', '\'']);
            if value.chars().count() < MIN_SECRET {
                continue;
            }
            let json = serde_json::Value::from(value).to_string();
            secrets.push(json[1..json.len() - 1].to_owned());
            secrets.push(value.to_owned());
        }
    }
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.dedup();
    secrets
}

pub(crate) fn mask(text: &str, secrets: &[String]) -> String {
    secrets.iter().fold(text.to_owned(), |text, secret| {
        text.replace(secret.as_str(), MASK)
    })
}

/// Masks `secrets` in every regular file under `dir`, except the bundle, whose packed objects
/// cannot be edited. A symlink the job left is skipped, never followed.
pub(crate) async fn mask_dir(dir: &Path, secrets: &[String]) -> Result<()> {
    if secrets.is_empty() {
        return Ok(());
    }
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                dirs.push(path);
                continue;
            }
            if !kind.is_file() || entry.file_name() == BUNDLE_FILE {
                continue;
            }
            let bytes = tokio::fs::read(&path).await?;
            if let Some(masked) = mask_bytes(&bytes, secrets) {
                tokio::fs::write(&path, masked).await?;
            }
        }
    }
    Ok(())
}

/// `None` when `bytes` holds no secret.
fn mask_bytes(bytes: &[u8], secrets: &[String]) -> Option<Vec<u8>> {
    let mut out = bytes.to_vec();
    let mut changed = false;
    for secret in secrets {
        let needle = secret.as_bytes();
        let mut masked = Vec::with_capacity(out.len());
        let mut rest = out.as_slice();
        while let Some(at) = rest.windows(needle.len()).position(|w| w == needle) {
            masked.extend_from_slice(&rest[..at]);
            masked.extend_from_slice(MASK.as_bytes());
            rest = &rest[at + needle.len()..];
            changed = true;
        }
        masked.extend_from_slice(rest);
        out = masked;
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ghp_0123456789abcdefTEST";

    fn npmrc() -> Vec<(String, Vec<u8>)> {
        vec![(
            "/home/node/.npmrc".into(),
            format!("# halo\n//npm.pkg.github.com/:_authToken={TOKEN}\nalways-auth=true\n")
                .into_bytes(),
        )]
    }

    #[test]
    fn a_list_names_a_repo_a_source_and_a_target() {
        let files = JobFile::parse_list(
            " productstudioinc/halo:/run/secrets/halo_npmrc:/home/node/.npmrc, a/b:/s:/t ",
        )
        .unwrap();
        assert_eq!(
            files[0],
            JobFile {
                repo: "productstudioinc/halo".into(),
                source: "/run/secrets/halo_npmrc".into(),
                target: "/home/node/.npmrc".into(),
            }
        );
        assert_eq!(files.len(), 2);
        assert!(JobFile::parse_list("").unwrap().is_empty());
        for bad in [
            "halo:/s:/t",
            "a/b:/s",
            "a/b:s:/t",
            "a/b:/s:t",
            "a/b:/s:/home/node/",
        ] {
            assert!(JobFile::parse_list(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn only_the_named_repos_jobs_get_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("npmrc");
        std::fs::write(&source, "x").unwrap();
        let files = vec![JobFile {
            repo: "productstudioinc/halo".into(),
            source,
            target: "/home/node/.npmrc".into(),
        }];
        assert_eq!(
            read_for(&files, "productstudioinc/Halo")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            read_for(&files, "productstudioinc/arnold")
                .await
                .unwrap()
                .is_empty()
        );
        let missing = vec![JobFile {
            source: dir.path().join("gone"),
            ..files[0].clone()
        }];
        assert!(read_for(&missing, "productstudioinc/halo").await.is_err());
    }

    #[test]
    fn the_archive_gives_the_job_user_a_read_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let tar = dir.path().join("files.tar");
        std::fs::write(&tar, archive(&npmrc())).unwrap();
        let list = std::process::Command::new("tar")
            .args(["--numeric-owner", "-tvf"])
            .arg(&tar)
            .output()
            .unwrap();
        let list = String::from_utf8(list.stdout).unwrap();
        assert!(list.starts_with("-r-------- 1000/1000"), "{list}");
        assert!(list.trim_end().ends_with("home/node/.npmrc"), "{list}");
        let contents = std::process::Command::new("tar")
            .arg("-xOf")
            .arg(&tar)
            .output()
            .unwrap();
        assert_eq!(contents.stdout, npmrc()[0].1);
    }

    #[test]
    fn a_files_values_are_masked_but_its_keys_and_short_values_are_not() {
        let secrets = secrets(&npmrc());
        assert_eq!(secrets, vec![TOKEN.to_owned()]);
        let line = format!("{{\"output\":\"//npm.pkg.github.com/:_authToken={TOKEN}\"}}");
        assert_eq!(
            mask(&line, &secrets),
            "{\"output\":\"//npm.pkg.github.com/:_authToken=***\"}"
        );
        let quoted = secrets_of_text("TOKEN=\"a\\b\\c-1234\"");
        assert!(
            quoted.contains(&"a\\\\b\\\\c-1234".to_owned()),
            "{quoted:?}"
        );
        assert!(quoted.contains(&"a\\b\\c-1234".to_owned()), "{quoted:?}");
    }

    fn secrets_of_text(text: &str) -> Vec<String> {
        secrets(&[("/t".into(), text.as_bytes().to_vec())])
    }

    #[tokio::test]
    async fn what_a_job_leaves_is_masked_except_its_bundle_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(out.join("sub")).unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, TOKEN).unwrap();
        std::fs::write(
            out.join("result.json"),
            format!("{{\"reason\":\"{TOKEN}\"}}"),
        )
        .unwrap();
        std::fs::write(out.join("sub/t.jsonl"), format!("a {TOKEN} b {TOKEN}\n")).unwrap();
        std::fs::write(out.join(BUNDLE_FILE), TOKEN).unwrap();
        std::os::unix::fs::symlink(&outside, out.join("link")).unwrap();
        mask_dir(&out, &secrets(&npmrc())).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("result.json")).unwrap(),
            "{\"reason\":\"***\"}"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("sub/t.jsonl")).unwrap(),
            "a *** b ***\n"
        );
        assert_eq!(
            std::fs::read_to_string(out.join(BUNDLE_FILE)).unwrap(),
            TOKEN
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), TOKEN);
    }
}
