//! session persistence: the on-disk shape of a saved window — the tab/split
//! tree plus each pane's last cwd and shell kind. serialized with the in-house
//! json (no serde). the app builds a SessionFile from its live tree on a short
//! debounce and rebuilds the tree from it on a bare launch. always-new-window
//! means each process owns one shared session.json, last-writer-wins

use crate::plugin::json::Json;

const VERSION: u32 = 1;
const MAX_RESTORED_TABS: usize = 64;
const MAX_RESTORED_PANES: usize = 128;
const MAX_RESTORED_SHELL_BYTES: usize = 128;
const MAX_RESTORED_CWD_BYTES: usize = 32 * 1024;
const MAX_RESTORED_TITLE_BYTES: usize = 1024;

pub struct SessionFile {
    pub active_tab: usize,
    pub tabs: Vec<TabSnap>,
    /// the window's last outer position + inner size, restored on next launch;
    /// None in older files or when the window was minimized at save time
    pub window: Option<WindowBounds>,
}

/// saved outer position + inner size of the window (physical pixels), plus
/// whether it was maximized
#[derive(Clone)]
pub struct WindowBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

#[derive(Clone)]
pub struct TabSnap {
    /// in-order index of the focused leaf within this tab's tree, so focus can
    /// be re-keyed after restore renumbers pane ids
    pub focused_leaf: usize,
    pub root: NodeSnap,
    /// user-given tab name overriding the cwd label (None = use the cwd)
    pub title: Option<String>,
    /// user-picked tab tint, an ansi palette index 1-6
    pub color: Option<u8>,
}

#[derive(Clone)]
pub enum NodeSnap {
    Leaf {
        cwd: Option<String>,
        shell: String,
    },
    Split {
        vertical: bool,
        ratio: f32,
        a: Box<NodeSnap>,
        b: Box<NodeSnap>,
    },
}

impl SessionFile {
    pub fn to_json_string(&self) -> String {
        let mut pairs = vec![
            ("version".to_string(), Json::Num(VERSION as f64)),
            ("active_tab".to_string(), Json::Num(self.active_tab as f64)),
            ("tabs".to_string(), Json::Arr(self.tabs.iter().map(TabSnap::to_json).collect())),
        ];
        if let Some(w) = &self.window {
            pairs.push(("window".to_string(), w.to_json()));
        }
        Json::Obj(pairs.into_iter().collect()).to_string()
    }

    /// parse a saved session; None on absent/corrupt/empty so the caller falls
    /// back to a single fresh shell
    pub fn parse(text: &str) -> Option<SessionFile> {
        let v = Json::parse(text)?;
        let mut panes_left = MAX_RESTORED_PANES;
        let tabs: Vec<TabSnap> = v
            .get("tabs")
            .and_then(Json::as_array)?
            .iter()
            .take(MAX_RESTORED_TABS)
            .filter_map(|tab| TabSnap::from_json(tab, &mut panes_left))
            .collect();
        if tabs.is_empty() {
            return None;
        }
        let active_tab = v.get("active_tab").and_then(Json::as_f64).unwrap_or(0.0) as usize;
        let window = v.get("window").and_then(WindowBounds::from_json);
        Some(SessionFile { active_tab, tabs, window })
    }
}

impl WindowBounds {
    fn to_json(&self) -> Json {
        Json::obj([
            ("x", Json::Num(self.x as f64)),
            ("y", Json::Num(self.y as f64)),
            ("w", Json::Num(self.width as f64)),
            ("h", Json::Num(self.height as f64)),
            ("max", Json::Bool(self.maximized)),
        ])
    }

    fn from_json(v: &Json) -> Option<WindowBounds> {
        Some(WindowBounds {
            x: v.get("x").and_then(Json::as_f64)? as i32,
            y: v.get("y").and_then(Json::as_f64)? as i32,
            width: v.get("w").and_then(Json::as_f64)? as u32,
            height: v.get("h").and_then(Json::as_f64)? as u32,
            // absent in older files: default to not-maximized
            maximized: v.get("max").and_then(Json::as_bool).unwrap_or(false),
        })
    }
}

impl TabSnap {
    fn to_json(&self) -> Json {
        let mut pairs = vec![
            ("focused_leaf".to_string(), Json::Num(self.focused_leaf as f64)),
            ("root".to_string(), self.root.to_json()),
        ];
        if let Some(t) = &self.title {
            pairs.push(("title".to_string(), Json::Str(t.clone())));
        }
        if let Some(c) = self.color {
            pairs.push(("color".to_string(), Json::Num(c as f64)));
        }
        Json::Obj(pairs.into_iter().collect())
    }

    fn from_json(v: &Json, panes_left: &mut usize) -> Option<TabSnap> {
        let focused_leaf = v.get("focused_leaf").and_then(Json::as_f64).unwrap_or(0.0) as usize;
        let mut remaining = *panes_left;
        let root = NodeSnap::from_json(v.get("root")?, &mut remaining)?;
        *panes_left = remaining;
        let title = v
            .get("title")
            .and_then(Json::as_str)
            .filter(|title| title.len() <= MAX_RESTORED_TITLE_BYTES)
            .map(str::to_string);
        let color = v
            .get("color")
            .and_then(Json::as_f64)
            .map(|n| n as u8)
            .filter(|c| (1..=6).contains(c));
        Some(TabSnap { focused_leaf, root, title, color })
    }
}

impl NodeSnap {
    fn to_json(&self) -> Json {
        match self {
            NodeSnap::Leaf { cwd, shell } => {
                // cwd is optional: omit the key when unknown rather than null
                let mut pairs = vec![
                    ("kind".to_string(), Json::Str("leaf".to_string())),
                    ("shell".to_string(), Json::Str(shell.clone())),
                ];
                if let Some(c) = cwd {
                    pairs.push(("cwd".to_string(), Json::Str(c.clone())));
                }
                Json::Obj(pairs.into_iter().collect())
            }
            NodeSnap::Split { vertical, ratio, a, b } => Json::obj([
                ("kind", Json::Str("split".to_string())),
                ("dir", Json::Str(if *vertical { "v" } else { "h" }.to_string())),
                ("ratio", Json::Num(*ratio as f64)),
                ("a", a.to_json()),
                ("b", b.to_json()),
            ]),
        }
    }

    fn from_json(v: &Json, panes_left: &mut usize) -> Option<NodeSnap> {
        match v.get_str("kind")? {
            "leaf" => {
                *panes_left = panes_left.checked_sub(1)?;
                Some(NodeSnap::Leaf {
                    cwd: v
                        .get("cwd")
                        .and_then(Json::as_str)
                        .filter(|cwd| cwd.len() <= MAX_RESTORED_CWD_BYTES)
                        .map(str::to_string),
                    shell: v
                        .get_str("shell")
                        .filter(|shell| shell.len() <= MAX_RESTORED_SHELL_BYTES)
                        .unwrap_or("auto")
                        .to_string(),
                })
            }
            "split" => Some(NodeSnap::Split {
                vertical: v.get_str("dir").unwrap_or("v") == "v",
                ratio: (v.get("ratio").and_then(Json::as_f64).unwrap_or(0.5) as f32).clamp(0.05, 0.95),
                a: Box::new(NodeSnap::from_json(v.get("a")?, panes_left)?),
                b: Box::new(NodeSnap::from_json(v.get("b")?, panes_left)?),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_a_split_tree() {
        let sf = SessionFile {
            active_tab: 1,
            tabs: vec![
                TabSnap {
                    focused_leaf: 0,
                    root: NodeSnap::Leaf { cwd: Some("C:/a".into()), shell: "pwsh".into() },
                    title: None,
                    color: None,
                },
                TabSnap {
                    focused_leaf: 1,
                    root: NodeSnap::Split {
                        vertical: true,
                        ratio: 0.4,
                        a: Box::new(NodeSnap::Leaf { cwd: None, shell: "cmd".into() }),
                        b: Box::new(NodeSnap::Leaf { cwd: Some("C:/b".into()), shell: "wsl".into() }),
                    },
                    title: Some("build".into()),
                    color: Some(4),
                },
            ],
            window: Some(WindowBounds { x: -12, y: 40, width: 1200, height: 800, maximized: true }),
        };
        let back = SessionFile::parse(&sf.to_json_string()).expect("parse");
        assert_eq!(back.active_tab, 1);
        assert_eq!(back.tabs.len(), 2);
        let w = back.window.expect("window bounds round-trip");
        assert_eq!((w.x, w.y, w.width, w.height, w.maximized), (-12, 40, 1200, 800, true));
        match &back.tabs[0].root {
            NodeSnap::Leaf { cwd, shell } => {
                assert_eq!(cwd.as_deref(), Some("C:/a"));
                assert_eq!(shell, "pwsh");
            }
            _ => panic!("expected leaf"),
        }
        assert_eq!(back.tabs[1].focused_leaf, 1);
        assert_eq!(back.tabs[0].title, None);
        assert_eq!(back.tabs[1].title.as_deref(), Some("build"));
        assert_eq!(back.tabs[0].color, None);
        assert_eq!(back.tabs[1].color, Some(4));
        match &back.tabs[1].root {
            NodeSnap::Split { vertical, ratio, a, b } => {
                assert!(*vertical);
                assert!((*ratio - 0.4).abs() < 1e-6);
                assert!(matches!(**a, NodeSnap::Leaf { cwd: None, .. }));
                assert!(matches!(&**b, NodeSnap::Leaf { cwd: Some(_), .. }));
            }
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn window_bounds_are_backward_compatible() {
        // a file with tabs but no window key loads with window None
        let no_win = r#"{"tabs":[{"root":{"kind":"leaf","shell":"pwsh"}}]}"#;
        assert!(SessionFile::parse(no_win).expect("parse").window.is_none());
        // a window without the "max" key defaults to not-maximized
        let no_max = r#"{"tabs":[{"root":{"kind":"leaf","shell":"pwsh"}}],"window":{"x":10,"y":20,"w":800,"h":600}}"#;
        let w = SessionFile::parse(no_max).expect("parse").window.expect("window");
        assert_eq!((w.x, w.y, w.width, w.height, w.maximized), (10, 20, 800, 600, false));
    }

    #[test]
    fn corrupt_or_empty_is_none() {
        assert!(SessionFile::parse("not json").is_none());
        assert!(SessionFile::parse("{}").is_none());
        assert!(SessionFile::parse(r#"{"tabs":[]}"#).is_none());
        // a split missing a child is rejected (whole tab dropped -> empty -> None)
        assert!(SessionFile::parse(r#"{"tabs":[{"root":{"kind":"split","dir":"v","ratio":0.5,"a":{"kind":"leaf","shell":"pwsh"}}}]}"#).is_none());
    }

    #[test]
    fn caps_restored_tabs_and_panes() {
        let leaf = r#"{"root":{"kind":"leaf"}}"#;
        let tabs = std::iter::repeat_n(leaf, MAX_RESTORED_TABS + 1).collect::<Vec<_>>().join(",");
        let session = SessionFile::parse(&format!(r#"{{"tabs":[{tabs}]}}"#)).expect("parse tabs");
        assert_eq!(session.tabs.len(), MAX_RESTORED_TABS);

        fn split_tree(depth: usize) -> String {
            if depth == 0 {
                return r#"{"kind":"leaf"}"#.to_string();
            }
            let child = split_tree(depth - 1);
            format!(r#"{{"kind":"split","a":{child},"b":{child}}}"#)
        }

        let root = split_tree(6);
        let tabs = format!(r#"{{"root":{root}}},{{"root":{root}}},{{"root":{root}}}"#);
        let session = SessionFile::parse(&format!(r#"{{"tabs":[{tabs}]}}"#)).expect("parse panes");
        assert_eq!(session.tabs.len(), 2);
    }

    #[test]
    fn drops_oversized_optional_session_fields() {
        let title = "t".repeat(MAX_RESTORED_TITLE_BYTES + 1);
        let cwd = "c".repeat(MAX_RESTORED_CWD_BYTES + 1);
        let shell = "s".repeat(MAX_RESTORED_SHELL_BYTES + 1);
        let text = format!(
            r#"{{"tabs":[{{"title":"{title}","root":{{"kind":"leaf","cwd":"{cwd}","shell":"{shell}"}}}}]}}"#
        );
        let session = SessionFile::parse(&text).expect("parse");
        assert_eq!(session.tabs[0].title, None);
        match &session.tabs[0].root {
            NodeSnap::Leaf { cwd, shell } => {
                assert_eq!(cwd, &None);
                assert_eq!(shell, "auto");
            }
            _ => panic!("expected leaf"),
        }
    }
}
