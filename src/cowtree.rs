//! New threads' worktrees made with [cowtree](https://github.com/markjaquith/cowtree), which
//! clones the files a worktree shares with the repository's other checkouts instead of
//! writing them again, so they take next to no room on the disk.
//!
//! The server makes a thread's worktree itself when asked for one. Where its disk is this
//! one and cowtree is installed, tria makes it instead, where the server would have put
//! it, and hands the server the finished checkout. Anything that keeps that from working
//! leaves the worktree to the server, as before.

use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
};

use tokio::process::Command;

/// What became of trying.
#[derive(Debug)]
pub enum Made {
    /// The worktree is at this path, on the branch it was asked for.
    At(String),
    /// cowtree is not installed, which is no news to anyone.
    NotInstalled,
    /// cowtree could not make it, for the reason given. Whatever it left behind is gone,
    /// so the server can make the same worktree on the same branch.
    Failed(String),
}

/// What a worktree is made from, as the server would make it.
pub struct Request<'a> {
    /// The project's checkout, which the worktree is added to.
    pub project_cwd: &'a str,
    /// The server's `worktreesDirectory` setting; empty for its default.
    pub worktrees_directory: &'a str,
    pub base_branch: &'a str,
    pub branch: &'a str,
    pub start_from_origin: bool,
}

/// Make the worktree with cowtree.
pub async fn add(request: Request<'_>) -> Made {
    add_with("cowtree", request).await
}

async fn add_with(program: &str, request: Request<'_>) -> Made {
    let Some(path) = worktree_path(&request) else {
        return Made::Failed("no directory to put worktrees in".into());
    };
    let cwd = request.project_cwd;
    // Something already there is not ours to clean up after, so it is the server's to
    // deal with.
    if path.exists() {
        return Made::Failed(format!("{} is already there", path.display()));
    }
    if git(
        cwd,
        &["rev-parse", "--verify", "--quiet", &heads(request.branch)],
    )
    .await
    .is_ok()
    {
        return Made::Failed(format!("branch {} is already there", request.branch));
    }
    let start = start_point(&request).await;
    let path_arg = path.to_string_lossy();
    let output = Command::new(program)
        .args(["add", "-b", request.branch, &path_arg, &start])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await;
    let why = match output {
        Ok(output) if output.status.success() => return Made::At(path_arg.into_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Made::NotInstalled,
        Err(error) => error.to_string(),
        Ok(output) => last_line(&output.stderr)
            .unwrap_or_else(|| format!("cowtree exited with {}", output.status)),
    };
    clean_up(cwd, &path, request.branch).await;
    Made::Failed(why)
}

/// Where the server puts a worktree it makes: the repository's directory under the
/// worktrees directory, named for the branch.
fn worktree_path(request: &Request<'_>) -> Option<PathBuf> {
    let repo = Path::new(request.project_cwd).file_name()?;
    let directory = match request.worktrees_directory {
        "" => crate::discovery::t3_home().ok()?.join("worktrees"),
        setting => {
            let expanded = match setting.strip_prefix('~') {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                    dirs::home_dir()?.join(rest.trim_start_matches('/'))
                }
                _ => PathBuf::from(setting),
            };
            // The server refuses one that is not absolute, and a filesystem root.
            if !expanded.is_absolute() || expanded.parent().is_none() {
                return None;
            }
            expanded
        }
    };
    Some(directory.join(repo).join(request.branch.replace('/', "-")))
}

/// What the worktree starts from: the base branch, or the commit its copy on `origin` is
/// at once fetched, where it is to start from there and there is one. A commit rather
/// than the remote branch, as the server does, so the new branch does not track it.
async fn start_point(request: &Request<'_>) -> String {
    let base = request.base_branch;
    let cwd = request.project_cwd;
    if request.start_from_origin && git(cwd, &["fetch", "origin", base]).await.is_ok() {
        let remote = format!("refs/remotes/origin/{base}^{{commit}}");
        if let Ok(commit) = git_output(cwd, &["rev-parse", "--verify", "--quiet", &remote]).await {
            return commit;
        }
    }
    base.to_string()
}

/// Take away what a failed `cowtree add` made, which is all new: the path and the branch
/// were both checked to be free before it ran.
async fn clean_up(cwd: &str, path: &Path, branch: &str) {
    let path_arg = path.to_string_lossy();
    let _ = git(
        cwd,
        &["worktree", "remove", "--force", "--force", &path_arg],
    )
    .await;
    if path.exists() {
        let _ = tokio::fs::remove_dir_all(path).await;
    }
    let _ = git(cwd, &["worktree", "prune"]).await;
    let _ = git(cwd, &["branch", "-D", branch]).await;
}

fn heads(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

async fn git(cwd: &str, args: &[&str]) -> Result<(), ()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|_| ())?;
    status.success().then_some(()).ok_or(())
}

/// What git printed, trimmed, where it succeeded.
async fn git_output(cwd: &str, args: &[&str]) -> Result<String, ()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The last line with anything on it, which is where a tool says what went wrong.
fn last_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(directory: &'a str) -> Request<'a> {
        Request {
            project_cwd: "/code/my-repo",
            worktrees_directory: directory,
            base_branch: "main",
            branch: "t3/0123abcd",
            start_from_origin: false,
        }
    }

    /// The server's own naming, so a worktree made here is one it counts as its own.
    #[test]
    fn a_worktree_goes_where_the_server_would_put_it() {
        assert_eq!(
            worktree_path(&request("/srv/worktrees")),
            Some(PathBuf::from("/srv/worktrees/my-repo/t3-0123abcd"))
        );
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            worktree_path(&request("~/wt")),
            Some(home.join("wt/my-repo/t3-0123abcd"))
        );
        assert_eq!(
            worktree_path(&request("")),
            Some(
                crate::discovery::t3_home()
                    .unwrap()
                    .join("worktrees/my-repo/t3-0123abcd")
            )
        );
    }

    #[test]
    fn a_setting_the_server_would_refuse_is_refused() {
        assert_eq!(worktree_path(&request("relative/dir")), None);
        assert_eq!(worktree_path(&request("/")), None);
    }

    /// A repository with one commit on `main`, and a directory for worktrees beside it.
    fn repository() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("tria-cowtree-{}", uuid::Uuid::new_v4()));
        let repo = root.join("my-repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "first",
            ],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(status.success());
        }
        (repo, root.join("worktrees"))
    }

    /// A stand-in for cowtree: `git worktree add` with what it is given, then `exit`.
    fn fake(root: &Path, exit: i32) -> String {
        let script = root.join(format!("cowtree-{exit}"));
        std::fs::write(
            &script,
            format!("#!/bin/sh\nshift\ngit worktree add -q \"$@\" || exit 9\necho 'error: no eligible clone' >&2\nexit {exit}\n"),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.to_string_lossy().into_owned()
    }

    fn has_branch(repo: &Path, branch: &str) -> bool {
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &heads(branch)])
            .current_dir(repo)
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    #[tokio::test]
    async fn a_worktree_cowtree_makes_is_handed_over_on_its_branch() {
        let (repo, worktrees) = repository();
        let cwd = repo.to_string_lossy();
        let directory = worktrees.to_string_lossy();
        let mut request = request(&directory);
        request.project_cwd = &cwd;
        let made = add_with(&fake(repo.parent().unwrap(), 0), request).await;
        let Made::At(path) = made else {
            panic!("{made:?}");
        };
        assert_eq!(Path::new(&path), worktrees.join("my-repo/t3-0123abcd"));
        assert!(Path::new(&path).join(".git").exists());
        assert!(has_branch(&repo, "t3/0123abcd"));
        let _ = std::fs::remove_dir_all(repo.parent().unwrap());
    }

    /// The server is asked for the same branch next, so a failure must leave none of it.
    #[tokio::test]
    async fn a_failed_cowtree_leaves_nothing_in_the_servers_way() {
        let (repo, worktrees) = repository();
        let cwd = repo.to_string_lossy();
        let directory = worktrees.to_string_lossy();
        let mut request = request(&directory);
        request.project_cwd = &cwd;
        let made = add_with(&fake(repo.parent().unwrap(), 1), request).await;
        assert!(
            matches!(&made, Made::Failed(why) if why == "error: no eligible clone"),
            "{made:?}"
        );
        assert!(!worktrees.join("my-repo/t3-0123abcd").exists());
        assert!(!has_branch(&repo, "t3/0123abcd"));
        let _ = std::fs::remove_dir_all(repo.parent().unwrap());
    }

    #[tokio::test]
    async fn without_cowtree_the_server_makes_it_and_hears_nothing() {
        let (repo, worktrees) = repository();
        let cwd = repo.to_string_lossy();
        let directory = worktrees.to_string_lossy();
        let mut request = request(&directory);
        request.project_cwd = &cwd;
        let made = add_with("tria-no-such-cowtree", request).await;
        assert!(matches!(made, Made::NotInstalled), "{made:?}");
        assert!(!has_branch(&repo, "t3/0123abcd"));
        let _ = std::fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn what_went_wrong_is_the_last_thing_said() {
        assert_eq!(
            last_line(b"cloning\nerror: no eligible clone\n\n").as_deref(),
            Some("error: no eligible clone")
        );
        assert_eq!(last_line(b"  \n"), None);
    }
}
