use crate::allocation::{CAPACITY, MAX_ID_BYTES, MAX_LIVE_IDS};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Workspace {
    pub id: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Monitor {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
    #[serde(rename = "activeWorkspace")]
    pub active_workspace: Workspace,
    #[serde(rename = "specialWorkspace")]
    pub special_workspace: Workspace,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Client {
    pub address: String,
    #[serde(rename = "stableId")]
    pub stable_id: String,
    pub mapped: bool,
    pub hidden: bool,
    #[serde(default)]
    pub pinned: bool,
    pub workspace: Workspace,
    pub at: [f64; 2],
    pub size: [f64; 2],
    #[serde(default)]
    pub grouped: Vec<String>,
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Window {
    pub address: String,
    pub stable_id: String,
    pub hint_id: String,
    pub workspace: i64,
    pub at: [f64; 2],
    pub size: [f64; 2],
    pub monitor: String,
    pub focused: bool,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Snapshot {
    pub windows: Vec<Window>,
    pub live_ids: Vec<String>,
    pub omitted: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    TooManyClients,
    InvalidMonitor,
    InvalidClient,
    DuplicateIdentity,
    InvalidGroup,
}

/// Normalize legacy visibility, grouping and logical coordinates. Titles and
/// classes are intentionally absent from the core snapshot and its diagnostics.
pub fn normalize(monitors: &[Monitor], clients: &[Client]) -> Result<Snapshot, Error> {
    if clients.len() > MAX_LIVE_IDS || monitors.len() > 128 {
        return Err(Error::TooManyClients);
    }
    let mut monitor_names = BTreeSet::new();
    for m in monitors {
        if m.name.is_empty()
            || m.name.len() > 256
            || !monitor_names.insert(&m.name)
            || ![m.x, m.y, m.width, m.height, m.scale]
                .iter()
                .all(|v| v.is_finite())
            || m.scale <= 0.0
            || m.width <= 0.0
            || m.height <= 0.0
            || !((m.width / m.scale).is_finite() && (m.height / m.scale).is_finite())
        {
            return Err(Error::InvalidMonitor);
        }
    }
    let mut stable = BTreeSet::new();
    let mut by_address = BTreeMap::new();
    for c in clients {
        if c.address.is_empty()
            || c.address.len() > 128
            || c.stable_id.is_empty()
            || c.stable_id.len() > 128
            || c.grouped.len() > CAPACITY
            || !c.at.iter().chain(&c.size).all(|v| v.is_finite())
            || c.size.iter().any(|v| *v < 0.0)
        {
            return Err(Error::InvalidClient);
        }
        if !stable.insert(&c.stable_id) || by_address.insert(&c.address, &c.stable_id).is_some() {
            return Err(Error::DuplicateIdentity);
        }
    }
    let special: BTreeSet<_> = monitors
        .iter()
        .map(|m| m.special_workspace.id)
        .filter(|id| *id != 0)
        .collect();
    let active: BTreeSet<_> = monitors.iter().map(|m| m.active_workspace.id).collect();
    let mut live = BTreeSet::new();
    let mut groups: BTreeMap<String, &Client> = BTreeMap::new();
    for c in clients {
        let hint_id = if c.grouped.is_empty() {
            c.stable_id.clone()
        } else {
            let mut members = Vec::with_capacity(c.grouped.len());
            for address in &c.grouped {
                members.push(*by_address.get(address).ok_or(Error::InvalidGroup)?);
            }
            members.sort();
            members.dedup();
            if !members.contains(&&c.stable_id) {
                return Err(Error::InvalidGroup);
            }
            let id = format!(
                "group:{}",
                members
                    .into_iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("+")
            );
            if id.len() > MAX_ID_BYTES {
                return Err(Error::InvalidGroup);
            }
            id
        };
        if c.mapped {
            live.insert(c.stable_id.clone());
            live.insert(hint_id.clone());
        }
        let visible = if special.is_empty() {
            c.pinned || active.contains(&c.workspace.id)
        } else {
            special.contains(&c.workspace.id)
        };
        if c.mapped && !c.hidden && visible {
            let representative = groups.entry(hint_id).or_insert(c);
            if c.focus_history_id < representative.focus_history_id {
                *representative = c;
            }
        }
    }
    if live.len() > MAX_LIVE_IDS {
        return Err(Error::TooManyClients);
    }
    let mut windows = Vec::new();
    for (hint_id, c) in groups {
        let center = [
            (c.at[0] + c.size[0] / 2.0).floor(),
            (c.at[1] + c.size[1] / 2.0).floor(),
        ];
        if let Some(m) = monitors.iter().find(|m| {
            center[0] >= m.x
                && center[0] < m.x + (m.width / m.scale).round()
                && center[1] >= m.y
                && center[1] < m.y + (m.height / m.scale).round()
        }) {
            windows.push(Window {
                address: c.address.clone(),
                stable_id: c.stable_id.clone(),
                hint_id,
                workspace: c.workspace.id,
                at: c.at,
                size: c.size,
                monitor: m.name.clone(),
                focused: c.focus_history_id == 0,
            });
        }
    }
    windows.sort_by(|a, b| {
        a.monitor
            .cmp(&b.monitor)
            .then(a.at[1].total_cmp(&b.at[1]))
            .then(a.at[0].total_cmp(&b.at[0]))
            .then(a.hint_id.cmp(&b.hint_id))
    });
    let omitted = windows.len().saturating_sub(CAPACITY);
    windows.truncate(CAPACITY);
    Ok(Snapshot {
        windows,
        live_ids: live.into_iter().collect(),
        omitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn monitor() -> Monitor {
        Monitor {
            name: "DP-1".into(),
            x: -1280.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
            scale: 1.5,
            active_workspace: Workspace { id: 1 },
            special_workspace: Workspace { id: 0 },
        }
    }
    fn client(id: &str, workspace: i64) -> Client {
        Client {
            address: format!("0x{id}"),
            stable_id: id.into(),
            mapped: true,
            hidden: false,
            pinned: false,
            workspace: Workspace { id: workspace },
            at: [-1000.0, 20.0],
            size: [200.0, 100.0],
            grouped: vec![],
            focus_history_id: 0,
        }
    }
    #[test]
    fn fractional_scale_negative_origins_and_offscreen() {
        let m = monitor();
        let mut c = client("a", 1);
        assert_eq!(
            normalize(std::slice::from_ref(&m), std::slice::from_ref(&c))
                .unwrap()
                .windows[0]
                .monitor,
            "DP-1"
        );
        c.at = [5.0, 20.0];
        assert!(normalize(&[m], &[c]).unwrap().windows.is_empty());
    }
    #[test]
    fn pinned_hidden_and_special_precedence() {
        let mut m = monitor();
        let mut pin = client("a", 3);
        pin.pinned = true;
        let mut hidden = client("b", 1);
        hidden.hidden = true;
        let special = client("c", -99);
        let clients = [pin, hidden, special];
        let normal = normalize(std::slice::from_ref(&m), &clients).unwrap();
        assert_eq!(normal.windows.len(), 1);
        assert!(normal.live_ids.contains(&"b".into()));
        m.special_workspace.id = -99;
        let only = normalize(&[m], &clients).unwrap();
        assert_eq!(only.windows.len(), 1);
        assert_eq!(only.windows[0].stable_id, "c");
    }
    #[test]
    fn group_id_stable_when_active_tab_changes() {
        let m = monitor();
        let mut a = client("a", 1);
        let mut b = client("b", 1);
        a.grouped = vec![b.address.clone(), a.address.clone()];
        b.grouped = a.grouped.clone();
        b.focus_history_id = 1;
        let first = normalize(std::slice::from_ref(&m), &[a.clone(), b.clone()]).unwrap();
        assert_eq!(first.windows.len(), 1);
        assert_eq!(first.windows[0].hint_id, "group:a+b");
        assert_eq!(first.windows[0].stable_id, "a");
        a.focus_history_id = 1;
        b.focus_history_id = 0;
        b.grouped.reverse();
        let next = normalize(&[m], &[a, b]).unwrap();
        assert_eq!(next.windows[0].hint_id, first.windows[0].hint_id);
        assert_eq!(next.windows[0].stable_id, "b");
    }
    #[test]
    fn malformed_geometry_groups_and_duplicate_id_fail_closed() {
        let m = monitor();
        let mut c = client("a", 1);
        c.at[0] = f64::NAN;
        assert_eq!(
            normalize(std::slice::from_ref(&m), &[c]),
            Err(Error::InvalidClient)
        );
        let mut c = client("a", 1);
        c.grouped = vec!["missing".into()];
        assert_eq!(
            normalize(std::slice::from_ref(&m), &[c]),
            Err(Error::InvalidGroup)
        );
        let c = client("a", 1);
        assert_eq!(
            normalize(std::slice::from_ref(&m), &[c.clone(), c]),
            Err(Error::DuplicateIdentity)
        );
        let mut m = m;
        m.scale = 0.0;
        assert_eq!(normalize(&[m], &[]), Err(Error::InvalidMonitor));
    }
}
