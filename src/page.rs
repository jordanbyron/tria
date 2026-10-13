//! Pages an agent showed the reader with T3 Code's `html_render`, drawn as pictures.
//!
//! The web app shows such a page live, in a frame above the reply. A terminal cannot run
//! a page, so it is drawn once into a picture by a headless Chromium-family browser on
//! this machine, the way the web app would show it at its usual width, and the picture is
//! what the chat shows. The page itself is kept beside it, for `gx` to open in a browser
//! that can run it.
//!
//! The call names the page by the attachment the server stored it as, and the stored
//! page carries the theme the web app gives it, so it is fetched rather than rebuilt from
//! what the agent wrote. A page never changes once stored, so what was drawn of it is kept
//! on disk and drawn again from there.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use base64::Engine;
use serde_json::Value;

use crate::model::Activity;

/// The width the web app gives a page in its reply column, in CSS pixels, and so the
/// width the agent made and measured it at.
const WIDTH: u32 = 728;

/// How tall a picture of a page may be, in CSS pixels. A page as tall as this is not
/// one anybody reads in a terminal, and the rest of it is a `gx` away.
const MOST_HEIGHT: u32 = 4000;

/// Pixels per CSS pixel. A terminal's cells are drawn on a screen that is likely to be
/// dense, and a picture drawn at one pixel per CSS pixel comes out soft.
const SCALE: u32 = 2;

/// How long a browser is given to draw a page.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A page a call showed: the attachment it is stored as, and what is known of its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Render {
    pub attachment_id: String,
    pub title: String,
    /// How tall the page is at `WIDTH`, in CSS pixels.
    pub height: u32,
}

/// A page drawn: the picture, and where the page is to open it.
#[derive(Debug, Clone)]
pub struct Shot {
    /// The picture, PNG as base64.
    pub png: String,
    /// The page on this machine's disk.
    pub page: String,
}

/// The page a finished call showed, if it is an `html_render` that showed one.
pub fn of(activity: &Activity) -> Option<Render> {
    if activity.kind != "tool.completed" {
        return None;
    }
    let data = activity.payload.get("data")?;
    let tool = data.get("toolName").and_then(Value::as_str)?;
    if !tool.ends_with("html_render") {
        return None;
    }
    // The result is the tool's text, which is the JSON of what it rendered, or that JSON
    // already read where a provider kept the structured form.
    let content = data.pointer("/result/content")?;
    let parsed;
    let result = match content {
        Value::String(text) => {
            parsed = serde_json::from_str::<Value>(text).ok()?;
            &parsed
        }
        Value::Object(fields) => fields.get("structuredContent").unwrap_or(content),
        _ => return None,
    };
    let render = result.get("htmlRender")?;
    let attachment_id = render.get("attachmentId").and_then(Value::as_str)?;
    // What the page measured at each width it was tried at, and failing that the frame
    // the agent asked for, which may cut it short.
    let measured = render
        .get("heights")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|pair| {
            let pair = pair.as_array()?;
            (pair.first()?.as_u64()? == u64::from(WIDTH)).then(|| pair.get(1)?.as_u64())?
        });
    let height = measured
        .or_else(|| render.get("height").and_then(Value::as_u64))
        .unwrap_or(600)
        .clamp(1, u64::from(MOST_HEIGHT)) as u32;
    Some(Render {
        attachment_id: attachment_id.to_string(),
        title: render
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("page")
            .to_string(),
        height,
    })
}

/// Where a page and its picture are kept, named after the attachment.
fn files(attachment_id: &str) -> (PathBuf, PathBuf) {
    let name: String = attachment_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = folder();
    (
        dir.join(format!("{name}.html")),
        dir.join(format!("{name}.png")),
    )
}

fn folder() -> PathBuf {
    std::env::temp_dir().join("tria-pages")
}

/// A page drawn before, by this run or an earlier one.
pub fn kept(attachment_id: &str) -> Option<Shot> {
    let (page, png) = files(attachment_id);
    if !page.exists() {
        return None;
    }
    shot(&page, &png)
}

fn shot(page: &Path, png: &Path) -> Option<Shot> {
    let bytes = std::fs::read(png).ok()?;
    Some(Shot {
        png: base64::engine::general_purpose::STANDARD.encode(bytes),
        page: page.to_string_lossy().into_owned(),
    })
}

/// Why a page is not drawn.
pub const NO_BROWSER: &str = "needs Chrome, Chromium, Edge or Brave to be drawn";
pub const NOT_FETCHED: &str = "could not be fetched";
pub const NOT_DRAWN: &str = "could not be drawn";

/// One browser at a time. Each run of one is a whole browser starting up, and they share
/// the profile they are pointed at, which a browser keeps to itself while it runs.
static DRAWING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Keep the page and draw it.
pub async fn draw(render: &Render, html: &[u8]) -> Result<Shot, &'static str> {
    let browser = browser().ok_or(NO_BROWSER)?;
    let (page, png) = files(&render.attachment_id);
    let _turn = DRAWING.lock().await;
    let dir = folder();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|_| NOT_DRAWN)?;
    tokio::fs::write(&page, html).await.map_err(|_| NOT_DRAWN)?;
    let url = url::Url::from_file_path(&page).map_err(|_| NOT_DRAWN)?;
    // Written aside and moved into place once whole, so a picture that is there at all
    // is a finished one.
    let part = png.with_extension("part.png");
    let _ = tokio::fs::remove_file(&part).await;
    let mut child = tokio::process::Command::new(&browser)
        .arg("--headless")
        .arg("--disable-gpu")
        .arg("--hide-scrollbars")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        // A browser of the same kind already running would otherwise be handed the page
        // to open as a window of its own.
        .arg(format!("--user-data-dir={}", dir.join("profile").display()))
        // The terminal is more likely dark than not, and the page has both themes.
        .arg("--force-dark-mode")
        .arg(format!("--force-device-scale-factor={SCALE}"))
        .arg(format!("--window-size={WIDTH},{}", render.height))
        .arg(format!("--screenshot={}", part.display()))
        .arg(url.as_str())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| NO_BROWSER)?;
    // Some browsers stay running once the picture is written. A picture that has stopped
    // growing is finished, and the browser is stopped.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut last = None;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        let size = tokio::fs::metadata(&part).await.ok().map(|meta| meta.len());
        if size.is_some_and(|size| size > 0) && size == last {
            break;
        }
        last = size;
        if tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = child.kill().await;
    tokio::fs::rename(&part, &png)
        .await
        .map_err(|_| NOT_DRAWN)?;
    shot(&page, &png).ok_or(NOT_DRAWN)
}

/// A Chromium-family browser to draw with: one installed the usual way, or one on the
/// `PATH`.
fn browser() -> Option<PathBuf> {
    let mut found = Vec::new();
    if cfg!(target_os = "macos") {
        let apps = [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
        ];
        let roots = std::iter::once(PathBuf::from("/Applications"))
            .chain(dirs::home_dir().map(|home| home.join("Applications")));
        for root in roots {
            found.extend(apps.iter().map(|app| root.join(app)));
        }
    }
    let names = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "microsoft-edge",
        "brave-browser",
    ];
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            found.extend(names.iter().map(|name| dir.join(name)));
        }
    }
    found.into_iter().find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn activity(kind: &str, data: Value) -> Activity {
        serde_json::from_value(json!({
            "id": "a1", "kind": kind, "payload": { "data": data },
        }))
        .unwrap()
    }

    #[test]
    fn a_render_is_read_from_the_text_the_tool_returned() {
        let text = json!({
            "htmlRender": {
                "attachmentId": "t-1-html", "title": "Options", "height": 667,
                "heights": [[375, 1327], [728, 690], [1000, 650]],
            },
            "message": "Shown to the reader above your reply.",
        })
        .to_string();
        let found = of(&activity(
            "tool.completed",
            json!({
                "toolName": "mcp__t3-code__html_render",
                "input": { "html": "<p>hi</p>" },
                "result": { "content": text },
            }),
        ));
        assert_eq!(
            found,
            Some(Render {
                attachment_id: "t-1-html".into(),
                title: "Options".into(),
                height: 690,
            })
        );
    }

    #[test]
    fn a_render_without_measurements_takes_the_frame_it_asked_for() {
        let found = of(&activity(
            "tool.completed",
            json!({
                "toolName": "mcp__t3_code__html_render",
                "result": { "content": { "structuredContent": {
                    "htmlRender": { "attachmentId": "t-2-html", "height": 400 },
                }}},
            }),
        ))
        .unwrap();
        assert_eq!(found.height, 400);
        assert_eq!(found.title, "page");
    }

    #[test]
    fn only_a_finished_render_is_a_page() {
        let data = json!({
            "toolName": "mcp__t3-code__html_render",
            "result": { "content": "{\"htmlRender\":{\"attachmentId\":\"x\"}}" },
        });
        assert_eq!(of(&activity("tool.started", data.clone())), None);
        let preview = json!({
            "toolName": "mcp__t3-code__html_preview",
            "result": { "content": "{\"htmlRender\":{\"attachmentId\":\"x\"}}" },
        });
        assert_eq!(of(&activity("tool.completed", preview)), None);
        assert!(of(&activity("tool.completed", data)).is_some());
    }

    #[test]
    fn a_page_is_kept_under_a_name_that_stays_in_its_folder() {
        let (page, png) = files("../../etc/passwd");
        assert_eq!(page.parent(), Some(folder().as_path()));
        assert_eq!(png.parent(), Some(folder().as_path()));
    }
}
