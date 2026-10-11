//! The terminal multiplexer tria runs inside, if any: tmux, or Embers, whose command line
//! takes tmux's commands and flags. Popups, panes, windows and sessions go through
//! whichever one this is, with the same arguments wherever the two agree.

use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mux {
    Tmux,
    Embers,
}

impl Mux {
    /// The multiplexer whose pane this is, from the variables each sets in its panes.
    pub fn detect() -> Option<Self> {
        Self::from_env(|name| std::env::var_os(name).is_some())
    }

    fn from_env(set: impl Fn(&str) -> bool) -> Option<Self> {
        if set("TMUX") {
            Some(Self::Tmux)
        } else if set("EMBERS_SOCKET") && set("EMBERS_BUFFER") {
            Some(Self::Embers)
        } else {
            None
        }
    }

    /// Its name, for what tria says about it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Embers => "embers",
        }
    }

    /// Its command line, which talks to the server this pane belongs to.
    pub fn command(self) -> Command {
        Command::new(self.name())
    }

    /// Tria's own pane, as a target. Embers names a pane after the buffer it shows,
    /// which is what it puts in the pane's environment.
    pub fn pane(self) -> Option<String> {
        match self {
            Self::Tmux => std::env::var("TMUX_PANE").ok(),
            Self::Embers => std::env::var("EMBERS_BUFFER")
                .ok()
                .map(|id| format!("%{id}")),
        }
    }

    /// The command that runs `command` in a popup over most of the screen, starting in
    /// `dir`, and returns when the popup closes. tmux takes the command as one line for
    /// its shell; Embers takes the program and its arguments, so the shell is named.
    pub fn popup_args(self, dir: &str, command: &str) -> Vec<String> {
        let mut args: Vec<String> = ["display-popup", "-E", "-d", dir, "-w", "90%", "-h", "90%"]
            .map(String::from)
            .into();
        if self == Self::Embers {
            args.extend(["--", "/bin/sh", "-c"].map(String::from));
        }
        args.push(command.to_string());
        args
    }
}

/// The command that opens the pane, as its arguments.
///
/// Side by side rather than one above the other: tria is a tall window and a shell
/// under it would have a dozen rows, where beside it both halves keep their height.
pub fn split_args(dir: &str, pane: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "split-window".to_string(),
        "-h".to_string(),
        "-c".to_string(),
        dir.to_string(),
    ];
    if let Some(pane) = pane {
        args.push("-t".to_string());
        args.push(pane.to_string());
    }
    args
}

/// The command that opens the window, as its arguments.
///
/// No target: tria was started inside the session it is in, so that is the one the
/// multiplexer resolves from the environment. No position either, so the window lands
/// where this session puts a new one — the point is that it is `prefix c` with a
/// directory, and anything else would be tria having an opinion about somebody's window
/// numbering.
pub fn window_args(dir: &str) -> Vec<String> {
    vec!["new-window".to_string(), "-c".to_string(), dir.to_string()]
}

/// Session name for a directory: its last path component, with the characters target
/// syntax reserves replaced.
pub fn session_name(dir: &str) -> String {
    let base = std::path::Path::new(dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| dir.to_string());
    base.chars()
        .map(|c| if c == '.' || c == ':' { '_' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_multiplexer_is_the_one_whose_pane_this_is() {
        let with = |names: &'static [&'static str]| Mux::from_env(|name| names.contains(&name));
        assert_eq!(with(&["TMUX", "TMUX_PANE"]), Some(Mux::Tmux));
        assert_eq!(with(&["EMBERS_SOCKET", "EMBERS_BUFFER"]), Some(Mux::Embers));
        // A socket alone is a server named for commands, not a pane of it.
        assert_eq!(with(&["EMBERS_SOCKET"]), None);
        assert_eq!(with(&[]), None);
    }

    #[test]
    fn session_name_is_the_directory_basename() {
        assert_eq!(session_name("/home/me/work/pnpm-hoisted"), "pnpm-hoisted");
        assert_eq!(session_name("/home/me/work/app.v2/"), "app_v2");
        assert_eq!(session_name("main"), "main");
    }

    /// The pane is a split of tria's own, named rather than left to whichever pane the
    /// multiplexer last called the active one, and it starts where the thread works.
    #[test]
    fn the_pane_is_a_split_of_this_one_in_the_threads_directory() {
        assert_eq!(
            split_args("/home/me/work/app", Some("%3")),
            ["split-window", "-h", "-c", "/home/me/work/app", "-t", "%3"]
        );
        // Outside a pane it names, the split is of whichever one is active — which,
        // with tria on screen, is tria's.
        assert_eq!(
            split_args("/home/me/work/app", None),
            ["split-window", "-h", "-c", "/home/me/work/app"]
        );
    }

    /// The window is `prefix c` with a directory: no target, since the session is the
    /// one tria was started in, and no position, since that is the session's to decide.
    #[test]
    fn the_window_is_a_new_one_in_this_session() {
        assert_eq!(
            window_args("/home/me/work/app"),
            ["new-window", "-c", "/home/me/work/app"]
        );
    }

    /// The same popup either way; Embers is handed the shell that tmux brings itself.
    #[test]
    fn the_popup_runs_the_command_through_a_shell() {
        let common = ["display-popup", "-E", "-d", "/w", "-w", "90%", "-h", "90%"];
        assert_eq!(
            Mux::Tmux.popup_args("/w", "nvim -R x"),
            [&common[..], &["nvim -R x"]].concat()
        );
        assert_eq!(
            Mux::Embers.popup_args("/w", "nvim -R x"),
            [&common[..], &["--", "/bin/sh", "-c", "nvim -R x"]].concat()
        );
    }
}
