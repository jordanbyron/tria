//! Route `tria open` to a live client on the same tmux or Embers server.
//!
//! Each client advertises a private Unix socket on its pane. The socket's reply
//! confirms that the UI accepted the directory before the caller switches panes.

#[cfg(unix)]
pub use unix::{listen, reuse};

#[cfg(not(unix))]
pub async fn reuse(_root: &str, _origin: Option<&str>) -> anyhow::Result<bool> {
    Ok(false)
}

#[cfg(not(unix))]
pub async fn listen(
    _events: tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
    _origin: &str,
) -> anyhow::Result<Option<()>> {
    Ok(None)
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        process::Stdio,
        time::Duration,
    };

    use anyhow::{Context as _, Result, bail};
    use serde::{Deserialize, Serialize, de::DeserializeOwned};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        process::Command,
        sync::{mpsc, oneshot},
        task::JoinHandle,
        time::timeout,
    };

    use crate::{app::AppEvent, mux::Mux};

    const OPTION: &str = "@tria-open-socket";
    const LIMIT: u64 = 16 * 1024;
    const WAIT: Duration = Duration::from_secs(5);

    /// A pane of a tmux or Embers server, and the server's socket. The two take the same
    /// commands; they differ in how a command names the server and the pane it runs in.
    #[derive(Clone)]
    struct Pane {
        mux: Mux,
        socket: PathBuf,
        pane: String,
    }

    impl Pane {
        fn from_env() -> Option<Self> {
            let mux = Mux::detect()?;
            let socket = match mux {
                Mux::Tmux => {
                    let tmux = std::env::var("TMUX").ok()?;
                    let mut parts = tmux.rsplitn(3, ',');
                    parts.next()?;
                    parts.next()?;
                    PathBuf::from(parts.next()?)
                }
                Mux::Embers => PathBuf::from(std::env::var_os("EMBERS_SOCKET")?),
            };
            let pane = mux.pane()?;
            Some(Self { mux, socket, pane })
        }

        /// The multiplexer's command line, aimed at this server as though run in this pane.
        fn std_command(&self) -> std::process::Command {
            let mut command = self.mux.command();
            match self.mux {
                Mux::Tmux => {
                    command.arg("-S").arg(&self.socket);
                    command.env("TMUX_PANE", &self.pane);
                }
                Mux::Embers => {
                    command.arg("--socket").arg(&self.socket);
                    command.env("EMBERS_SOCKET", &self.socket);
                    command.env("EMBERS_BUFFER", self.pane.trim_start_matches('%'));
                }
            }
            command
        }

        fn command(&self) -> Command {
            let mut command = Command::from(self.std_command());
            command.stdin(Stdio::inherit()).kill_on_drop(true);
            command
        }

        async fn call(&self, args: &[&str]) -> Result<String> {
            let name = self.mux.name();
            let output = timeout(WAIT, self.command().args(args).output())
                .await
                .with_context(|| format!("{name} command timed out"))??;
            if !output.status.success() {
                bail!("{name}: {}", String::from_utf8_lossy(&output.stderr).trim());
            }
            Ok(String::from_utf8(output.stdout)?.trim_end().to_string())
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Open {
        root: String,
        origin: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "status", rename_all = "kebab-case")]
    enum Reply {
        Opened,
        OtherServer,
        Rejected { error: String },
    }

    async fn read<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
        let mut bytes = Vec::new();
        BufReader::new(stream)
            .take(LIMIT)
            .read_until(b'\n', &mut bytes)
            .await?;
        anyhow::ensure!(bytes.last() == Some(&b'\n'), "incomplete tria handoff");
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn write(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        anyhow::ensure!(bytes.len() <= LIMIT as usize, "tria handoff is too large");
        stream.write_all(&bytes).await?;
        Ok(())
    }

    async fn receive(
        mut stream: UnixStream,
        events: mpsc::UnboundedSender<AppEvent>,
        origin: String,
    ) -> Result<()> {
        let open: Open = read(&mut stream).await?;
        let reply = if open
            .origin
            .as_ref()
            .is_some_and(|wanted| wanted.trim_end_matches('/') != origin.trim_end_matches('/'))
        {
            Reply::OtherServer
        } else if !Path::new(&open.root).is_absolute() {
            Reply::Rejected {
                error: "tria open needs an absolute directory".into(),
            }
        } else {
            let (reply, answer) = oneshot::channel();
            events
                .send(AppEvent::OpenDirectory {
                    root: open.root,
                    reply,
                })
                .map_err(|_| anyhow::anyhow!("tria is shutting down"))?;
            match answer
                .await
                .context("tria closed before opening the directory")?
            {
                Ok(()) => Reply::Opened,
                Err(error) => Reply::Rejected { error },
            }
        };
        write(&mut stream, &reply).await
    }

    /// Owned by the event loop. Normal exit removes both the advertisement and socket.
    pub struct Listener {
        task: JoinHandle<()>,
        socket: PathBuf,
        pane: Pane,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.task.abort();
            let _ = fs::remove_file(&self.socket);
            // Another client in the pane may have replaced our advertisement.
            let option = self
                .pane
                .std_command()
                .args(["show-options", "-pqv", "-t", &self.pane.pane, OPTION])
                .output();
            if option.is_ok_and(|out| {
                String::from_utf8_lossy(&out.stdout).trim() == self.socket.to_string_lossy()
            }) {
                let _ = self
                    .pane
                    .std_command()
                    .args(["set-option", "-pu", "-t", &self.pane.pane, OPTION])
                    .output();
            }
        }
    }

    fn runtime_directory() -> Result<PathBuf> {
        let uid = unsafe { libc::geteuid() };
        // macOS's long per-user temporary path can exceed the Unix socket path limit.
        let directory = dirs::runtime_dir()
            .map(|path| path.join("tria"))
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/tria-{uid}")));
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        anyhow::ensure!(
            metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
            "tria's socket directory must be private and owned by this user"
        );
        Ok(directory)
    }

    pub async fn listen(
        events: mpsc::UnboundedSender<AppEvent>,
        origin: &str,
    ) -> Result<Option<Listener>> {
        let Some(pane) = Pane::from_env() else {
            return Ok(None);
        };
        let directory = runtime_directory()?;
        listen_at(pane, &directory, events, origin).await.map(Some)
    }

    async fn listen_at(
        pane: Pane,
        directory: &Path,
        events: mpsc::UnboundedSender<AppEvent>,
        origin: &str,
    ) -> Result<Listener> {
        let socket = directory.join(format!("{}.sock", crate::commands::new_id()));
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let origin = origin.to_string();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let events = events.clone();
                let origin = origin.clone();
                tokio::spawn(async move {
                    let _ = timeout(WAIT, receive(stream, events, origin)).await;
                });
            }
        });
        let guard = Listener { task, socket, pane };
        guard
            .pane
            .call(&[
                "set-option",
                "-p",
                "-t",
                &guard.pane.pane,
                OPTION,
                &guard.socket.to_string_lossy(),
            ])
            .await?;
        Ok(guard)
    }

    pub async fn reuse(root: &str, origin: Option<&str>) -> Result<bool> {
        let Some(pane) = Pane::from_env() else {
            return Ok(false);
        };
        reuse_at(&pane, root, origin).await
    }

    async fn reuse_at(here: &Pane, root: &str, origin: Option<&str>) -> Result<bool> {
        // Fields are split at spaces, which ids never have, with the one that may have them
        // last. Not tabs: tmux 3.7 prints a tab in a format as `_`.
        let Ok(panes) = here
            .call(&[
                "list-panes",
                "-a",
                "-F",
                "#{session_id} #{pane_id} #{@tria-open-socket}",
            ])
            .await
        else {
            return Ok(false);
        };
        let caller = here
            .call(&[
                "display-message",
                "-p",
                "-t",
                &here.pane,
                "#{session_id} #{client_name}",
            ])
            .await?;
        let (session, client) = caller.split_once(' ').unwrap_or((&caller, ""));
        let mut candidates: Vec<_> = panes
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, ' ');
                let session = parts.next()?;
                let pane = parts.next()?;
                let socket = parts.next()?.trim();
                (!socket.is_empty()).then_some((session, pane, socket))
            })
            .collect();
        candidates.sort_by_key(|(candidate, _, _)| *candidate != session);
        for (_, pane, socket) in candidates {
            let Ok(metadata) = fs::metadata(socket) else {
                continue;
            };
            if !metadata.file_type().is_socket()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                continue;
            }
            let Ok(Ok(mut stream)) =
                timeout(Duration::from_secs(1), UnixStream::connect(socket)).await
            else {
                continue;
            };
            let request = Open {
                root: root.into(),
                origin: origin.map(str::to_string),
            };
            let reply: Reply = timeout(WAIT, async {
                write(&mut stream, &request).await?;
                read(&mut stream).await
            })
            .await
            .context(
                "existing tria did not acknowledge the directory; it may still open there",
            )??;
            match reply {
                Reply::OtherServer => continue,
                Reply::Rejected { error } => bail!("{error}"),
                Reply::Opened => {
                    let mut args = vec!["switch-client"];
                    if !client.is_empty() {
                        args.extend(["-c", client]);
                    }
                    args.extend(["-t", pane]);
                    here.call(&args).await.with_context(|| {
                        format!(
                            "directory opened in tria, but switching to its {} pane failed",
                            here.mux.name()
                        )
                    })?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A tmux or Embers server of the test's own, with a client attached to the
        /// session `calling`, whose pane is the one `tria open` runs in.
        struct Server {
            pane: Pane,
            directory: PathBuf,
            client: Option<Client>,
        }

        enum Client {
            /// tmux's control mode, which needs no terminal.
            Control(std::process::Child),
            Terminal(Box<dyn portable_pty::Child + Send + Sync>),
        }

        impl Drop for Server {
            fn drop(&mut self) {
                match &mut self.client {
                    Some(Client::Control(client)) => {
                        let _ = client.kill();
                        let _ = client.wait();
                    }
                    Some(Client::Terminal(client)) => {
                        let _ = client.kill();
                        let _ = client.wait();
                    }
                    None => {}
                }
                let _ = self.pane.std_command().arg("kill-server").output();
                let _ = fs::remove_dir_all(&self.directory);
            }
        }

        impl Server {
            async fn new(mux: Mux) -> Option<Self> {
                // Embers takes tmux's option commands since it took set-option.
                let probe = match mux {
                    Mux::Tmux => ["-V"].as_slice(),
                    Mux::Embers => ["set-option", "--help"].as_slice(),
                };
                let name = mux.name();
                if !mux
                    .command()
                    .args(probe)
                    .output()
                    .is_ok_and(|out| out.status.success())
                {
                    eprintln!("{name} is unavailable; skipping isolated {name} test");
                    return None;
                }
                let directory =
                    PathBuf::from(format!("/tmp/tria-test-{}", crate::commands::new_id()));
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&directory)
                    .unwrap();
                let mut server = Self {
                    pane: Pane {
                        mux,
                        socket: directory.join(format!("{name}.sock")),
                        pane: String::new(),
                    },
                    directory,
                    client: None,
                };
                let args = server.with_program(vec![
                    "new-session",
                    "-d",
                    "-s",
                    "calling",
                    "-P",
                    "-F",
                    "#{pane_id}",
                ]);
                server.pane.pane = server.pane.call(&args).await.unwrap();
                server.client = Some(server.attach());
                for _ in 0..500 {
                    if !server
                        .pane
                        .call(&["list-clients"])
                        .await
                        .unwrap()
                        .is_empty()
                    {
                        return Some(server);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("client did not attach");
            }

            /// A client showing `calling`: in control mode for tmux, in a terminal of its
            /// own for Embers, which has no control mode.
            fn attach(&self) -> Client {
                if self.pane.mux == Mux::Tmux {
                    let client = self
                        .pane
                        .std_command()
                        .args(["-C", "attach-session", "-t", "calling"])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap();
                    return Client::Control(client);
                }
                let pty = portable_pty::native_pty_system()
                    .openpty(portable_pty::PtySize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .unwrap();
                let command = self.pane.std_command();
                let mut builder = portable_pty::CommandBuilder::new(command.get_program());
                builder.args(command.get_args());
                builder.args(["attach", "-t", "calling"]);
                for name in ["TMUX", "TMUX_PANE", "EMBERS_SOCKET", "EMBERS_BUFFER"] {
                    builder.env_remove(name);
                }
                let child = pty.slave.spawn_command(builder).unwrap();
                // The client stops drawing, and so stops, if nothing reads what it draws.
                let mut reader = pty.master.try_clone_reader().unwrap();
                std::thread::spawn(move || {
                    let _master = pty.master;
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                });
                Client::Terminal(child)
            }

            /// `args` running a program that waits: tmux takes it as a line for its
            /// shell, Embers as the program and its arguments.
            fn with_program<'a>(&self, mut args: Vec<&'a str>) -> Vec<&'a str> {
                match self.pane.mux {
                    Mux::Tmux => args.push("sleep 60"),
                    Mux::Embers => args.extend(["--", "sleep", "60"]),
                }
                args
            }

            /// The session the client shows and its active pane.
            async fn shown(&self) -> String {
                const FORMAT: &str = "#{session_name} #{pane_id}";
                match self.pane.mux {
                    Mux::Tmux => self
                        .pane
                        .call(&["list-clients", "-F", FORMAT])
                        .await
                        .unwrap(),
                    Mux::Embers => {
                        let clients = self.pane.call(&["list-clients"]).await.unwrap();
                        let client = clients.split('\t').next().unwrap();
                        self.pane
                            .call(&["display-message", "-p", "-c", client, FORMAT])
                            .await
                            .unwrap()
                    }
                }
            }

            /// Another session, whose pane is in a window that is not its current one.
            async fn target(&self) -> Pane {
                let args = self.with_program(vec!["new-session", "-d", "-s", "target"]);
                self.pane.call(&args).await.unwrap();
                let args = self.with_program(vec![
                    "new-window",
                    "-d",
                    "-t",
                    "target",
                    "-P",
                    "-F",
                    "#{window_id}",
                ]);
                let window = self.pane.call(&args).await.unwrap();
                let args = self.with_program(vec![
                    "split-window",
                    "-d",
                    "-t",
                    &window,
                    "-P",
                    "-F",
                    "#{pane_id}",
                ]);
                let pane = self.pane.call(&args).await.unwrap();
                Pane {
                    pane,
                    ..self.pane.clone()
                }
            }
        }

        async fn a_live_client_receives_the_directory_before_switching_to_its_pane(mux: Mux) {
            let Some(server) = Server::new(mux).await else {
                return;
            };
            assert!(
                !reuse_at(&server.pane, "/tmp", Some("http://one"))
                    .await
                    .unwrap()
            );
            let target = server.target().await;
            let (events, mut requests) = mpsc::unbounded_channel();
            let listener = listen_at(target.clone(), &server.directory, events, "http://one")
                .await
                .unwrap();
            assert!(
                !reuse_at(&server.pane, "/tmp", Some("http://two"))
                    .await
                    .unwrap()
            );
            assert!(
                requests.try_recv().is_err(),
                "another server is not sent the directory"
            );

            let caller = server.pane.clone();
            let rejected =
                tokio::spawn(async move { reuse_at(&caller, "/tmp", Some("http://one")).await });
            let AppEvent::OpenDirectory { reply, .. } =
                timeout(WAIT, requests.recv()).await.unwrap().unwrap()
            else {
                panic!("expected directory event")
            };
            reply.send(Err("unsent draft".into())).unwrap();
            assert!(
                rejected
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("unsent draft")
            );
            let calling = format!("calling {}", server.pane.pane);
            assert_eq!(server.shown().await, calling);

            let caller = server.pane.clone();
            let opened = tokio::spawn(async move {
                reuse_at(
                    &caller,
                    "/tmp/project with spaces-你好",
                    Some("http://one/"),
                )
                .await
            });
            let AppEvent::OpenDirectory { root, reply } =
                timeout(WAIT, requests.recv()).await.unwrap().unwrap()
            else {
                panic!("expected directory event")
            };
            assert_eq!(root, "/tmp/project with spaces-你好");
            assert_eq!(server.shown().await, calling);
            reply.send(Ok(())).unwrap();
            assert!(opened.await.unwrap().unwrap());
            assert_eq!(server.shown().await, format!("target {}", target.pane));

            let socket = listener.socket.clone();
            drop(listener);
            assert!(!socket.exists());
            assert!(
                target
                    .call(&["show-options", "-pqv", "-t", &target.pane, OPTION])
                    .await
                    .unwrap()
                    .is_empty()
            );
            // A crashed instance can leave an advertisement and socket behind.
            let stale = server.directory.join("stale.sock");
            drop(UnixListener::bind(&stale).unwrap());
            fs::set_permissions(&stale, fs::Permissions::from_mode(0o600)).unwrap();
            target
                .call(&[
                    "set-option",
                    "-p",
                    "-t",
                    &target.pane,
                    OPTION,
                    stale.to_str().unwrap(),
                ])
                .await
                .unwrap();
            assert!(!reuse_at(&server.pane, "/tmp", None).await.unwrap());
        }

        #[tokio::test]
        async fn a_live_client_receives_the_directory_before_tmux_switches_to_its_pane() {
            a_live_client_receives_the_directory_before_switching_to_its_pane(Mux::Tmux).await;
        }

        #[tokio::test]
        async fn a_live_client_receives_the_directory_before_embers_switches_to_its_pane() {
            a_live_client_receives_the_directory_before_switching_to_its_pane(Mux::Embers).await;
        }

        async fn the_current_session_is_preferred_over_another_session(mux: Mux) {
            let Some(server) = Server::new(mux).await else {
                return;
            };
            let distant = server.target().await;
            let (remote_events, mut remote_requests) = mpsc::unbounded_channel();
            let _remote = listen_at(distant, &server.directory, remote_events, "http://one")
                .await
                .unwrap();
            let (events, mut requests) = mpsc::unbounded_channel();
            let _local = listen_at(server.pane.clone(), &server.directory, events, "http://one")
                .await
                .unwrap();
            let caller = server.pane.clone();
            let opened =
                tokio::spawn(async move { reuse_at(&caller, "/tmp", Some("http://one")).await });
            let AppEvent::OpenDirectory { reply, .. } =
                timeout(WAIT, requests.recv()).await.unwrap().unwrap()
            else {
                panic!("expected directory event")
            };
            reply.send(Ok(())).unwrap();
            assert!(opened.await.unwrap().unwrap());
            assert!(remote_requests.try_recv().is_err());
            assert_eq!(
                server.shown().await,
                format!("calling {}", server.pane.pane)
            );
        }

        #[tokio::test]
        async fn in_tmux_the_current_session_is_preferred_over_another_session() {
            the_current_session_is_preferred_over_another_session(Mux::Tmux).await;
        }

        #[tokio::test]
        async fn in_embers_the_current_session_is_preferred_over_another_session() {
            the_current_session_is_preferred_over_another_session(Mux::Embers).await;
        }

        #[tokio::test]
        async fn malformed_or_oversized_requests_do_not_reach_the_ui() {
            let (events, mut requests) = mpsc::unbounded_channel();
            for bytes in [b"not-json\n".to_vec(), vec![b'x'; LIMIT as usize + 1]] {
                let (mut caller, receiver) = UnixStream::pair().unwrap();
                let task = tokio::spawn(receive(receiver, events.clone(), "http://one".into()));
                // The receiver can close as soon as it reaches the size limit.
                let _ = caller.write_all(&bytes).await;
                let _ = caller.shutdown().await;
                assert!(timeout(WAIT, task).await.unwrap().unwrap().is_err());
            }
            assert!(requests.try_recv().is_err());
        }
    }
}
