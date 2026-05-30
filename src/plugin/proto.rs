//! the termie plugin wire protocol (v1): newline-delimited json over the child
//! process stdin/stdout. this is a public contract once a marketplace exists —
//! bump API_VERSION and document compat when it changes
//!
//! direction names are from termie's point of view:
//!   HostEvent  = termie -> plugin   (things that happened)
//!   PluginCmd  = plugin -> termie   (things the plugin wants done)

use super::json::Json;

pub const API_VERSION: u32 = 1;

/// a Tier-1 declarative widget the renderer draws in the instrument aesthetic.
/// plugins describe; termie draws. richer fields (sprites, meters) extend this
/// without breaking older plugins because unknown fields are ignored
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Widget {
    pub id: String,
    pub title: String,
    pub lines: Vec<String>,
}

impl Widget {
    fn from_json(v: &Json) -> Option<Widget> {
        let id = v.get_str("id")?.to_string();
        let title = v.get_str("title").unwrap_or("").to_string();
        let lines = v
            .get("lines")
            .and_then(Json::as_array)
            .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Some(Widget { id, title, lines })
    }

    #[cfg(test)]
    fn to_json(&self) -> Json {
        Json::obj([
            ("id", Json::Str(self.id.clone())),
            ("title", Json::Str(self.title.clone())),
            (
                "lines",
                Json::Arr(self.lines.iter().cloned().map(Json::Str).collect()),
            ),
        ])
    }
}

/// termie -> plugin
#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    /// handshake: termie's api version + the granted permission list
    Hello { api_version: u32, permissions: Vec<String> },
    FocusChanged { pane: u64 },
    TabChanged { tab: usize },
    CwdChanged { cwd: String },
    Bell { pane: u64 },
    WidgetClicked { id: String },
    /// a message published by another plugin on the in-process bus
    Message { from: String, topic: String, body: Json },
    Shutdown,
}

impl HostEvent {
    pub fn to_line(&self) -> String {
        let v = match self {
            HostEvent::Hello { api_version, permissions } => Json::obj([
                ("t", Json::Str("hello".into())),
                ("api_version", Json::Num(*api_version as f64)),
                (
                    "permissions",
                    Json::Arr(permissions.iter().cloned().map(Json::Str).collect()),
                ),
            ]),
            HostEvent::FocusChanged { pane } => Json::obj([
                ("t", Json::Str("focus_changed".into())),
                ("pane", Json::Num(*pane as f64)),
            ]),
            HostEvent::TabChanged { tab } => Json::obj([
                ("t", Json::Str("tab_changed".into())),
                ("tab", Json::Num(*tab as f64)),
            ]),
            HostEvent::CwdChanged { cwd } => Json::obj([
                ("t", Json::Str("cwd_changed".into())),
                ("cwd", Json::Str(cwd.clone())),
            ]),
            HostEvent::Bell { pane } => Json::obj([
                ("t", Json::Str("bell".into())),
                ("pane", Json::Num(*pane as f64)),
            ]),
            HostEvent::WidgetClicked { id } => Json::obj([
                ("t", Json::Str("widget_clicked".into())),
                ("id", Json::Str(id.clone())),
            ]),
            HostEvent::Message { from, topic, body } => Json::obj([
                ("t", Json::Str("message".into())),
                ("from", Json::Str(from.clone())),
                ("topic", Json::Str(topic.clone())),
                ("body", body.clone()),
            ]),
            HostEvent::Shutdown => Json::obj([("t", Json::Str("shutdown".into()))]),
        };
        v.to_string()
    }
}

/// plugin -> termie. unknown command types parse to `Unknown` so a newer plugin
/// talking to an older termie degrades instead of breaking the stream
#[derive(Clone, Debug, PartialEq)]
pub enum PluginCmd {
    /// plugin announces itself + the api version it was built against
    Ready { name: String, api_version: u32 },
    DeclareWidget(Widget),
    UpdateWidget(Widget),
    /// a transient status/toast message
    Notify { text: String },
    /// send input bytes to the focused pane (requires the write_pty permission)
    WritePty { data: String },
    /// publish to the in-process bus for other plugins
    Publish { topic: String, body: Json },
    /// subscribe to a bus topic ("*" = all)
    Subscribe { topic: String },
    Unknown(String),
}

impl PluginCmd {
    /// parse one ndjson line; None only if the line isn't valid json at all
    pub fn from_line(line: &str) -> Option<PluginCmd> {
        let v = Json::parse(line.trim())?;
        let t = v.get_str("t").unwrap_or("");
        Some(match t {
            "ready" => PluginCmd::Ready {
                name: v.get_str("name").unwrap_or("").to_string(),
                api_version: v.get("api_version").and_then(Json::as_f64).unwrap_or(0.0) as u32,
            },
            "declare_widget" => match v.get("widget").and_then(Widget::from_json) {
                Some(w) => PluginCmd::DeclareWidget(w),
                None => PluginCmd::Unknown(t.to_string()),
            },
            "update_widget" => match v.get("widget").and_then(Widget::from_json) {
                Some(w) => PluginCmd::UpdateWidget(w),
                None => PluginCmd::Unknown(t.to_string()),
            },
            "notify" => PluginCmd::Notify {
                text: v.get_str("text").unwrap_or("").to_string(),
            },
            "write_pty" => PluginCmd::WritePty {
                data: v.get_str("data").unwrap_or("").to_string(),
            },
            "publish" => PluginCmd::Publish {
                topic: v.get_str("topic").unwrap_or("").to_string(),
                body: v.get("body").cloned().unwrap_or(Json::Null),
            },
            "subscribe" => PluginCmd::Subscribe {
                topic: v.get_str("topic").unwrap_or("*").to_string(),
            },
            other => PluginCmd::Unknown(other.to_string()),
        })
    }

    /// the serialized form (so a plugin written in rust against this crate, or a
    /// test, can emit valid lines)
    #[cfg(test)]
    pub fn to_line(&self) -> String {
        let v = match self {
            PluginCmd::Ready { name, api_version } => Json::obj([
                ("t", Json::Str("ready".into())),
                ("name", Json::Str(name.clone())),
                ("api_version", Json::Num(*api_version as f64)),
            ]),
            PluginCmd::DeclareWidget(w) => Json::obj([
                ("t", Json::Str("declare_widget".into())),
                ("widget", w.to_json()),
            ]),
            PluginCmd::UpdateWidget(w) => Json::obj([
                ("t", Json::Str("update_widget".into())),
                ("widget", w.to_json()),
            ]),
            PluginCmd::Notify { text } => Json::obj([
                ("t", Json::Str("notify".into())),
                ("text", Json::Str(text.clone())),
            ]),
            PluginCmd::WritePty { data } => Json::obj([
                ("t", Json::Str("write_pty".into())),
                ("data", Json::Str(data.clone())),
            ]),
            PluginCmd::Publish { topic, body } => Json::obj([
                ("t", Json::Str("publish".into())),
                ("topic", Json::Str(topic.clone())),
                ("body", body.clone()),
            ]),
            PluginCmd::Subscribe { topic } => Json::obj([
                ("t", Json::Str("subscribe".into())),
                ("topic", Json::Str(topic.clone())),
            ]),
            PluginCmd::Unknown(s) => Json::obj([("t", Json::Str(s.clone()))]),
        };
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_event_serializes() {
        let line = HostEvent::Hello {
            api_version: API_VERSION,
            permissions: vec!["read_output".into()],
        }
        .to_line();
        let v = Json::parse(&line).unwrap();
        assert_eq!(v.get_str("t"), Some("hello"));
        assert_eq!(v.get("api_version").and_then(Json::as_f64), Some(1.0));
    }

    #[test]
    fn message_event_carries_from_topic_body() {
        let line = HostEvent::Message {
            from: "chat".into(),
            topic: "say".into(),
            body: Json::Str("hi".into()),
        }
        .to_line();
        let v = Json::parse(&line).unwrap();
        assert_eq!(v.get_str("t"), Some("message"));
        assert_eq!(v.get_str("from"), Some("chat"));
        assert_eq!(v.get_str("topic"), Some("say"));
        assert_eq!(v.get("body").and_then(Json::as_str), Some("hi"));
    }

    #[test]
    fn plugin_cmd_roundtrips() {
        let cmds = [
            PluginCmd::Ready { name: "pet".into(), api_version: 1 },
            PluginCmd::DeclareWidget(Widget {
                id: "pet".into(),
                title: "Tama".into(),
                lines: vec!["happy".into(), "hunger 80%".into()],
            }),
            PluginCmd::Notify { text: "hi".into() },
            PluginCmd::WritePty { data: "ls\r".into() },
            PluginCmd::Publish { topic: "chat".into(), body: Json::Str("yo".into()) },
            PluginCmd::Subscribe { topic: "*".into() },
        ];
        for c in cmds {
            assert_eq!(PluginCmd::from_line(&c.to_line()), Some(c));
        }
    }

    #[test]
    fn unknown_command_degrades() {
        let c = PluginCmd::from_line(r#"{"t":"from_the_future","x":1}"#);
        assert_eq!(c, Some(PluginCmd::Unknown("from_the_future".into())));
    }

    #[test]
    fn garbage_line_is_none() {
        assert!(PluginCmd::from_line("not json").is_none());
    }

    #[test]
    fn widget_ignores_unknown_fields() {
        // forward-compat: a plugin sends a field this termie doesn't know yet
        let line = r#"{"t":"declare_widget","widget":{"id":"w","title":"T","lines":["a"],"sprite":"future"}}"#;
        match PluginCmd::from_line(line).unwrap() {
            PluginCmd::DeclareWidget(w) => {
                assert_eq!(w.id, "w");
                assert_eq!(w.lines, vec!["a".to_string()]);
            }
            other => panic!("expected widget, got {other:?}"),
        }
    }
}
