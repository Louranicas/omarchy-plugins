//! Frozen, read-only legacy action policy. Resolving an action grants no effect
//! authority. Custom commands are retained as argv and are never executed here.
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, Visitor},
};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::File,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
};
const MAX_BYTES: usize = 65_536;
pub const DEFAULT_TAP_MS: u64 = 280;
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Action {
    ToggleMaximized,
    PromoteMaster,
    Disabled,
    Custom(Vec<String>),
}
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAction {
    Builtin(String),
    Argv(Vec<String>),
    Command(Command),
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    command: Vec<String>,
}
#[derive(Debug, Default)]
struct Layouts(BTreeMap<String, Option<RawAction>>);
impl<'de> Deserialize<'de> for Layouts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Unique;
        impl<'de> Visitor<'de> for Unique {
            type Value = Layouts;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bounded unique layout map")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Layouts, M::Error> {
                let mut out = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Option<RawAction>>()? {
                    if out.len() >= 128
                        || key.is_empty()
                        || key.len() > 128
                        || key.contains('\0')
                        || out.insert(key, value).is_some()
                    {
                        return Err(de::Error::custom("invalid or duplicate layout"));
                    }
                }
                Ok(Layouts(out))
            }
        }
        deserializer.deserialize_map(Unique)
    }
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    #[serde(rename = "timeoutMs")]
    timeout: Option<serde_json::Value>,
    #[serde(default)]
    layouts: Option<Layouts>,
}
#[derive(Default, Deserialize)]
struct Legacy {
    // Other legacy appearance/hold fields are untouched; no serialization/write.
    #[serde(rename = "doubleTap")]
    normal: Option<Section>,
    #[serde(rename = "altDoubleTap")]
    alternate: Option<Section>,
    #[serde(default, deserialize_with = "present")]
    schema_version: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present")]
    actions: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present")]
    gestures: Option<serde_json::Value>,
}
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(d).map(Some)
}
#[derive(Clone, Debug, Serialize)]
pub struct Policy {
    tap_ms: u64,
    normal: BTreeMap<String, Action>,
    alternate: BTreeMap<String, Action>,
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid legacy Vimarchy policy")
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            tap_ms: DEFAULT_TAP_MS,
            normal: BTreeMap::new(),
            alternate: BTreeMap::new(),
        }
    }
}
fn actions(section: Option<Section>) -> io::Result<BTreeMap<String, Action>> {
    let mut out = BTreeMap::new();
    for (layout, raw) in section.and_then(|s| s.layouts).unwrap_or_default().0 {
        let Some(raw) = raw else { continue }; // legacy jq null falls through to wildcard/default
        let action = match raw {
            RawAction::Builtin(name) => match name.as_str() {
                "toggle-maximized" => Action::ToggleMaximized,
                "promote-master" => Action::PromoteMaster,
                "disabled" => Action::Disabled,
                _ => return Err(invalid()),
            },
            RawAction::Argv(argv) | RawAction::Command(Command { command: argv }) => {
                if argv.is_empty()
                    || argv.len() > 256
                    || argv[0].is_empty()
                    || argv.iter().any(|s| s.contains('\0'))
                    || argv.iter().map(String::len).sum::<usize>() > 16_384
                {
                    return Err(invalid());
                }
                Action::Custom(argv)
            }
        };
        out.insert(layout, action);
    }
    Ok(out)
}
impl Policy {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(invalid());
        }
        let raw: Legacy = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        // A proposed modern schema is not silently interpreted as absent legacy choices.
        if raw.schema_version.is_some() || raw.actions.is_some() || raw.gestures.is_some() {
            return Err(invalid());
        }
        let timeout = raw.normal.as_ref().and_then(|s| s.timeout.as_ref());
        let tap_ms = match timeout {
            None | Some(serde_json::Value::Null) => DEFAULT_TAP_MS,
            Some(serde_json::Value::Number(n)) => n.as_u64().ok_or_else(invalid)?.clamp(120, 600),
            Some(serde_json::Value::String(s))
                if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) =>
            {
                s.parse::<u64>().map_err(|_| invalid())?.clamp(120, 600)
            }
            _ => return Err(invalid()),
        };
        Ok(Self {
            tap_ms,
            normal: actions(raw.normal)?,
            alternate: actions(raw.alternate)?,
        })
    }
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).expect("closed policy"))
        )
    }
    pub fn tap_ms(&self) -> u64 {
        self.tap_ms
    }
    /// Caller must obtain the current target layout through authenticated native
    /// observations. This pure resolution is not an executable permit.
    pub fn resolve(&self, layout: &str, alt: bool) -> Action {
        let map = if alt { &self.alternate } else { &self.normal };
        map.get(layout)
            .or_else(|| map.get("*"))
            .cloned()
            .unwrap_or_else(|| {
                if alt || matches!(layout, "dwindle" | "scrolling") {
                    Action::ToggleMaximized
                } else if layout == "master" {
                    Action::PromoteMaster
                } else {
                    Action::Disabled
                }
            })
    }
    /// Explicit supplied path must exist. Only omission by the trusted launcher
    /// selects defaults; malformed/unreadable supplied settings never fall back.
    pub fn load(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            return Err(invalid());
        }
        let parts: Vec<_> = path.components().collect();
        let mut file = File::open("/")?;
        for (index, part) in parts.iter().enumerate() {
            let Component::Normal(name) = part else {
                if *part == Component::RootDir {
                    continue;
                }
                return Err(invalid());
            };
            let name = CString::new(name.as_bytes()).map_err(|_| invalid())?;
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if index + 1 < parts.len() {
                    libc::O_DIRECTORY
                } else {
                    0
                };
            // SAFETY: retained parent FD, NUL-terminated component and fixed flags.
            let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful openat returns a fresh owned descriptor.
            file = unsafe { File::from_raw_fd(fd) };
        }
        let before = file.metadata()?;
        // SAFETY: geteuid has no arguments or memory requirements.
        if !before.is_file()
            || before.uid() != unsafe { libc::geteuid() }
            || before.nlink() != 1
            || before.mode() & 0o022 != 0
            || before.len() > MAX_BYTES as u64
        {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        if before.len() != bytes.len() as u64
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
            || before.mode() != after.mode()
            || before.nlink() != after.nlink()
        {
            return Err(invalid());
        }
        Self::parse(&bytes)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn legacy_defaults_clamps_and_explicit_choices() {
        let p = Policy::parse(br#"{}"#).unwrap();
        assert_eq!(p.tap_ms(), 280);
        assert_eq!(p.resolve("master", false), Action::PromoteMaster);
        assert_eq!(p.resolve("master", true), Action::ToggleMaximized);
        for (n, want) in [(0, 120), (279, 279), (601, 600)] {
            assert_eq!(
                Policy::parse(format!(r#"{{"doubleTap":{{"timeoutMs":{n}}}}}"#).as_bytes())
                    .unwrap()
                    .tap_ms(),
                want
            )
        }
        let p=Policy::parse(br#"{"doubleTap":{"layouts":{"*":"disabled","master":"promote-master"}},"altDoubleTap":{"layouts":{"*":"disabled","dwindle":["sh","-c","printf hi"]}}}"#).unwrap();
        assert_eq!(p.resolve("scrolling", false), Action::Disabled);
        assert_eq!(p.resolve("master", false), Action::PromoteMaster);
        assert_eq!(p.resolve("master", true), Action::Disabled);
        assert!(matches!(p.resolve("dwindle", true), Action::Custom(_)));
    }
    #[test]
    fn malformed_or_ambiguous_settings_never_choose_defaults() {
        for bytes in [
            r#"{"doubleTap":{},"doubleTap":{}}"#,
            r#"{"doubleTap":{"layouts":{"*":"disabled","*":"toggle-maximized"}}}"#,
            r#"{"altDoubleTap":{"layouts":{"*":["tool",42]}}}"#,
            r#"{"doubleTap":{"timeoutMs":-1}}"#,
            r#"{"schema_version":1}"#,
            r#"{"schema_version":null}"#,
            r#"{"altDoubleTap":{"layouts":{"*":"unknown"}}}"#,
        ] {
            assert!(Policy::parse(bytes.as_bytes()).is_err(), "{bytes}")
        }
        assert!(Policy::parse(&vec![b' '; MAX_BYTES + 1]).is_err());
    }
    #[test]
    fn null_override_falls_through_without_losing_custom_arguments() {
        let p=Policy::parse(br#"{"doubleTap":{"timeoutMs":"280","layouts":{"master":null,"*":{"command":["/tool","one two","line\nnext"]}}}}"#).unwrap();
        assert_eq!(
            p.resolve("master", false),
            Action::Custom(vec!["/tool".into(), "one two".into(), "line\nnext".into()])
        );
    }
    #[test]
    fn explicit_policy_special_files_and_writable_inputs_refuse() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("settings");
        std::fs::write(&file, b"{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(Policy::load(&file).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&file, root.path().join("hardlink")).unwrap();
        assert!(Policy::load(&file).is_err());
        std::fs::remove_file(&file).unwrap();
        let cpath = CString::new(file.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid private NUL-terminated fixture path.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(Policy::load(&file).is_err());
        let alias = root.path().join("directory-alias");
        symlink(root.path(), &alias).unwrap();
        assert!(Policy::load(&alias.join("hardlink")).is_err());
    }

    #[test]
    fn file_bounds_and_symlink_refusal() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vimarchy.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Policy::load(&path).is_ok());
        symlink(&path, root.path().join("alias")).unwrap();
        assert!(Policy::load(&root.path().join("alias")).is_err());
        std::fs::write(&path, vec![b' '; MAX_BYTES + 1]).unwrap();
        assert!(Policy::load(&path).is_err());
        assert!(Policy::load(&root.path().join("missing")).is_err());
    }
}
