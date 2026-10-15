//! Simulators and emulators an agent drives through T3 Code's `device_*` tools.
//!
//! The web app streams such a device live beside the thread. A terminal does not play
//! video, so tria shows what can be shown still: the screenshots the agent took, where it
//! took them, and which device the thread has open, with `:device` handing the live view
//! to the web app.
//!
//! A screenshot's bytes never come with the thread. The server leaves them out of the
//! timeline and serves each as a `tool-output-image` asset, by the item that returned it
//! and its place among that item's images.

use serde::Deserialize;
use serde_json::Value;

use crate::model::Activity;

/// A screenshot a call returned, by the item that returned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screenshot {
    /// The turn item, which the asset is asked for by.
    pub item_id: String,
    /// The thread the item is stored in: a subagent's own, for one it took.
    pub thread_id: String,
    /// The device it was asked of, where the call named one.
    pub device_id: Option<String>,
}

/// The screenshot a finished call returned, if it is a `device_screenshot` that worked.
pub fn screenshot(activity: &Activity, thread_id: &str) -> Option<Screenshot> {
    if activity.kind != "tool.completed" {
        return None;
    }
    let data = activity.payload.get("data")?;
    let tool = data.get("toolName").and_then(Value::as_str)?;
    if !tool.ends_with("device_screenshot") {
        return None;
    }
    if data
        .pointer("/result/content/isError")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return None;
    }
    Some(Screenshot {
        item_id: activity.id.clone(),
        thread_id: data
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or(thread_id)
            .to_string(),
        device_id: data
            .pointer("/input/deviceId")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The server's devices and which threads have them open, from `subscribeDeviceState`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceState {
    #[serde(default)]
    pub devices: Vec<Device>,
    #[serde(default)]
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub host_id: String,
    pub id: String,
    pub name: String,
}

/// A device open in a thread. One device can be open in several.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub thread_id: String,
    pub host_id: String,
    pub device_id: String,
}

impl DeviceState {
    /// The names of the devices a thread has open, oldest first.
    pub fn open_in(&self, thread_id: &str) -> Vec<&str> {
        self.sessions
            .iter()
            .filter(|session| session.thread_id == thread_id)
            .map(|session| {
                self.devices
                    .iter()
                    .find(|device| {
                        device.host_id == session.host_id && device.id == session.device_id
                    })
                    .map_or(session.device_id.as_str(), |device| device.name.as_str())
            })
            .collect()
    }

    /// A device's name, by its id on whichever host has it.
    pub fn name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|device| device.id == device_id)
            .map(|device| device.name.as_str())
    }
}

/// Where the web app shows a thread, its open devices with it.
pub fn web_url(origin: &str, environment_id: &str, thread_id: &str) -> Option<String> {
    let mut url = url::Url::parse(origin).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .push(environment_id)
        .push(thread_id);
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn activity(kind: &str, data: Value) -> Activity {
        serde_json::from_value(json!({
            "id": "item-1", "kind": kind, "payload": { "data": data },
        }))
        .unwrap()
    }

    #[test]
    fn a_screenshot_is_known_by_its_item_and_thread() {
        let found = screenshot(
            &activity(
                "tool.completed",
                json!({
                    "toolName": "mcp__t3-code__device_screenshot",
                    "threadId": "child",
                    "input": { "deviceId": "0BA5", "hostId": "local" },
                    "result": { "content": null },
                }),
            ),
            "open",
        );
        assert_eq!(
            found,
            Some(Screenshot {
                item_id: "item-1".into(),
                thread_id: "child".into(),
                device_id: Some("0BA5".into()),
            })
        );
    }

    #[test]
    fn only_a_finished_screenshot_that_worked_is_one() {
        let data = json!({ "toolName": "mcp__t3_code__device_screenshot" });
        assert_eq!(
            screenshot(&activity("tool.started", data.clone()), "t"),
            None
        );
        let failed = json!({
            "toolName": "mcp__t3-code__device_screenshot",
            "result": { "content": { "isError": true } },
        });
        assert_eq!(screenshot(&activity("tool.completed", failed), "t"), None);
        let open = json!({ "toolName": "mcp__t3-code__device_open" });
        assert_eq!(screenshot(&activity("tool.completed", open), "t"), None);
        let found = screenshot(&activity("tool.completed", data), "t").unwrap();
        assert_eq!(found.thread_id, "t");
        assert_eq!(found.device_id, None);
    }

    #[test]
    fn a_thread_names_the_devices_it_has_open() {
        let state: DeviceState = serde_json::from_value(json!({
            "hosts": [], "hostStatus": "ready",
            "devices": [
                { "hostId": "local", "id": "a", "platform": "ios", "name": "iPhone 18 Pro",
                  "version": "iOS 27.0", "booted": true, "physical": false },
            ],
            "sessions": [
                { "threadId": "t", "hostId": "local", "deviceId": "a", "platform": "ios",
                  "openedAt": "2026-10-15T16:30:30Z" },
                { "threadId": "t", "hostId": "ssh", "deviceId": "b", "platform": "android",
                  "openedAt": "2026-10-15T16:31:30Z" },
                { "threadId": "u", "hostId": "local", "deviceId": "a", "platform": "ios",
                  "openedAt": "2026-10-15T16:32:30Z" },
            ],
        }))
        .unwrap();
        assert_eq!(state.open_in("t"), vec!["iPhone 18 Pro", "b"]);
        assert!(state.open_in("v").is_empty());
        assert_eq!(state.name("a"), Some("iPhone 18 Pro"));
    }

    #[test]
    fn the_web_app_shows_a_thread_under_its_environment() {
        assert_eq!(
            web_url("http://127.0.0.1:3773", "env", "thread:a b").as_deref(),
            Some("http://127.0.0.1:3773/env/thread:a%20b")
        );
        assert_eq!(
            web_url("https://host.example/t3/", "env", "t").as_deref(),
            Some("https://host.example/t3/env/t")
        );
    }
}
