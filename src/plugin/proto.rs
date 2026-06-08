//! the termie plugin wire protocol (v2): newline-delimited json over the child
//! process stdin/stdout. this is a public contract once a marketplace exists —
//! bump API_VERSION and document compat when it changes. v2 adds the Tier-2
//! immediate-mode draw list (see `DrawCmd`) as an optional widget field; v1
//! plugins that never send it are unaffected
//!
//! direction names are from termie's point of view:
//!   HostEvent  = termie -> plugin   (things that happened)
//!   PluginCmd  = plugin -> termie   (things the plugin wants done)

use super::json::Json;

pub const API_VERSION: u32 = 2;

/// an immediate-mode draw primitive (Tier-2, `api_version` >= 2). coordinates are
/// normalized 0..1 within the widget's canvas box, so a plugin is scale- and
/// resolution-independent and can never address outside its own widget. `color`
/// is a palette role ("paper", "text", "mute", "rule", "ink", "accent") or a
/// "#rrggbb" / "#rgb" hex string; an unknown spec falls back to the dock body color
#[derive(Clone, Debug, PartialEq)]
pub enum DrawCmd {
    Rect { x: f32, y: f32, w: f32, h: f32, color: String },
    Text { x: f32, y: f32, text: String, color: String },
}

impl DrawCmd {
    fn from_json(v: &Json) -> Option<DrawCmd> {
        // clamp coords into the unit square here so the renderer can trust them
        let unit = |k: &str| (v.get(k).and_then(Json::as_f64).unwrap_or(0.0) as f32).clamp(0.0, 1.0);
        let color = v.get_str("color").unwrap_or("").to_string();
        Some(match v.get_str("t")? {
            "rect" => DrawCmd::Rect { x: unit("x"), y: unit("y"), w: unit("w"), h: unit("h"), color },
            "text" => DrawCmd::Text {
                x: unit("x"),
                y: unit("y"),
                text: v.get_str("text").unwrap_or("").chars().take(240).collect(),
                color,
            },
            _ => return None,
        })
    }

    #[cfg(test)]
    fn to_json(&self) -> Json {
        match self {
            DrawCmd::Rect { x, y, w, h, color } => Json::obj([
                ("t", Json::Str("rect".into())),
                ("x", Json::Num(*x as f64)),
                ("y", Json::Num(*y as f64)),
                ("w", Json::Num(*w as f64)),
                ("h", Json::Num(*h as f64)),
                ("color", Json::Str(color.clone())),
            ]),
            DrawCmd::Text { x, y, text, color } => Json::obj([
                ("t", Json::Str("text".into())),
                ("x", Json::Num(*x as f64)),
                ("y", Json::Num(*y as f64)),
                ("text", Json::Str(text.clone())),
                ("color", Json::Str(color.clone())),
            ]),
        }
    }
}

/// a Tier-1 declarative widget the renderer draws in the instrument aesthetic.
/// plugins describe; termie draws. richer fields (sprites, meters) extend this
/// without breaking older plugins because unknown fields are ignored. a Tier-2
/// plugin (`api_version` >= 2) may also set `draw` — an immediate-mode primitive
/// list painted in a `canvas_h`-tall box under the title; Tier-1 plugins leave it
/// empty and are unaffected
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Widget {
    pub id: String,
    pub title: String,
    pub lines: Vec<String>,
    pub draw: Vec<DrawCmd>,
    pub canvas_h: Option<f32>,
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
        // cap the primitive count so one widget can't flood the instance buffer
        let draw = v
            .get("draw")
            .and_then(Json::as_array)
            .map(|a| a.iter().filter_map(DrawCmd::from_json).take(256).collect::<Vec<_>>())
            .unwrap_or_default();
        let canvas_h = v
            .get("canvas_h")
            .and_then(Json::as_f64)
            .map(|n| (n as f32).clamp(8.0, 360.0));
        Some(Widget { id, title, lines, draw, canvas_h })
    }

    #[cfg(test)]
    fn to_json(&self) -> Json {
        let mut fields = vec![
            ("id", Json::Str(self.id.clone())),
            ("title", Json::Str(self.title.clone())),
            ("lines", Json::Arr(self.lines.iter().cloned().map(Json::Str).collect())),
        ];
        if !self.draw.is_empty() {
            fields.push(("draw", Json::Arr(self.draw.iter().map(DrawCmd::to_json).collect())));
        }
        if let Some(h) = self.canvas_h {
            fields.push(("canvas_h", Json::Num(h as f64)));
        }
        Json::obj(fields)
    }
}

/// termie -> plugin. this is the full host event surface (a versioned public
/// contract); some variants are emitted by the host already and others are wired
/// to their event sources incrementally, so the unconnected ones are allowed to
/// be unconstructed for now without churning the protocol definition
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
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
        assert_eq!(v.get("api_version").and_then(Json::as_f64), Some(API_VERSION as f64));
    }

    #[test]
    fn widget_clicked_serializes() {
        let line = HostEvent::WidgetClicked { id: "pet".into() }.to_line();
        let v = Json::parse(&line).unwrap();
        assert_eq!(v.get_str("t"), Some("widget_clicked"));
        assert_eq!(v.get_str("id"), Some("pet"));
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
                ..Default::default()
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

    #[test]
    fn tier2_draw_list_roundtrips() {
        let w = Widget {
            id: "gauge".into(),
            title: "Gauge".into(),
            lines: vec![],
            draw: vec![
                DrawCmd::Rect { x: 0.0, y: 0.0, w: 1.0, h: 0.25, color: "ink".into() },
                DrawCmd::Rect { x: 0.0, y: 0.0, w: 0.6, h: 0.25, color: "#6486a6".into() },
                DrawCmd::Text { x: 0.0, y: 0.3, text: "60%".into(), color: "paper".into() },
            ],
            canvas_h: Some(64.0),
        };
        let cmd = PluginCmd::DeclareWidget(w);
        assert_eq!(PluginCmd::from_line(&cmd.to_line()), Some(cmd));
    }

    #[test]
    fn tier2_coords_clamp_and_unknown_primitive_dropped() {
        let line = r#"{"t":"declare_widget","widget":{"id":"w","title":"T","draw":[{"t":"rect","x":-1,"y":2,"w":5,"h":0.5,"color":"rule"},{"t":"triangle","x":0,"y":0},{"t":"text","x":0.5,"y":0.5,"text":"hi","color":"text"}]}}"#;
        match PluginCmd::from_line(line).unwrap() {
            PluginCmd::DeclareWidget(w) => {
                assert_eq!(w.draw.len(), 2);
                match &w.draw[0] {
                    DrawCmd::Rect { x, y, w: rw, h, .. } => {
                        assert_eq!((*x, *y, *rw, *h), (0.0, 1.0, 1.0, 0.5));
                    }
                    other => panic!("expected rect, got {other:?}"),
                }
            }
            other => panic!("expected widget, got {other:?}"),
        }
    }

    #[test]
    fn tier2_draw_list_is_capped() {
        let rects = std::iter::repeat_n(r#"{"t":"rect","x":0,"y":0,"w":1,"h":1,"color":"ink"}"#, 300)
            .collect::<Vec<_>>()
            .join(",");
        let line = format!(r#"{{"t":"declare_widget","widget":{{"id":"w","title":"T","draw":[{rects}]}}}}"#);
        match PluginCmd::from_line(&line).unwrap() {
            PluginCmd::DeclareWidget(w) => assert_eq!(w.draw.len(), 256),
            other => panic!("expected widget, got {other:?}"),
        }
    }

    #[test]
    fn tier1_widget_has_no_draw_list() {
        // a v1-style widget (no draw field) stays Tier-1: empty draw, no canvas
        let line = r#"{"t":"declare_widget","widget":{"id":"pet","title":"Tama","lines":["happy"]}}"#;
        match PluginCmd::from_line(line).unwrap() {
            PluginCmd::DeclareWidget(w) => {
                assert!(w.draw.is_empty());
                assert!(w.canvas_h.is_none());
            }
            other => panic!("expected widget, got {other:?}"),
        }
    }

    #[test]
    fn tier2_text_truncates_to_240_chars() {
        let long = "x".repeat(500);
        let line = format!(
            r#"{{"t":"declare_widget","widget":{{"id":"w","title":"T","draw":[{{"t":"text","x":0,"y":0,"text":"{long}","color":"text"}}]}}}}"#
        );
        match PluginCmd::from_line(&line).unwrap() {
            PluginCmd::DeclareWidget(w) => match &w.draw[0] {
                DrawCmd::Text { text, .. } => assert_eq!(text.chars().count(), 240),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected widget, got {other:?}"),
        }
    }

    #[test]
    fn tier2_canvas_h_clamps_to_bounds() {
        let parse_h = |h: &str| -> Option<f32> {
            let line = format!(
                r#"{{"t":"declare_widget","widget":{{"id":"w","title":"T","canvas_h":{h},"draw":[{{"t":"rect","x":0,"y":0,"w":1,"h":1,"color":"ink"}}]}}}}"#
            );
            match PluginCmd::from_line(&line).unwrap() {
                PluginCmd::DeclareWidget(w) => w.canvas_h,
                _ => None,
            }
        };
        assert_eq!(parse_h("1"), Some(8.0)); // below the floor
        assert_eq!(parse_h("72"), Some(72.0)); // in range
        assert_eq!(parse_h("9000"), Some(360.0)); // above the ceiling
    }

    #[test]
    fn tier2_color_spec_is_preserved_verbatim() {
        // the protocol carries the color string as-is; the renderer resolves it
        let line = r##"{"t":"declare_widget","widget":{"id":"w","title":"T","draw":[{"t":"rect","x":0,"y":0,"w":1,"h":1,"color":"#6486a6"},{"t":"text","x":0,"y":0,"text":"hi","color":"accent"}]}}"##;
        match PluginCmd::from_line(line).unwrap() {
            PluginCmd::DeclareWidget(w) => {
                match &w.draw[0] {
                    DrawCmd::Rect { color, .. } => assert_eq!(color, "#6486a6"),
                    other => panic!("expected rect, got {other:?}"),
                }
                match &w.draw[1] {
                    DrawCmd::Text { color, .. } => assert_eq!(color, "accent"),
                    other => panic!("expected text, got {other:?}"),
                }
            }
            other => panic!("expected widget, got {other:?}"),
        }
    }
}
