//! Controlled-launch configuration. No process discovery or live installation.
use crate::{
    actor::Application,
    backend::PresenterSpec,
    process_identity::{Identity, Pinned},
};
use serde::Deserialize;
use std::{
    ffi::CString,
    fs::File,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Component, Path, PathBuf},
};
const MAX_CONFIG: usize = 65536;
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Vimarchy,
    Ask,
    Yoohoo,
}
impl Role {
    pub fn application(self) -> Application {
        match self {
            Self::Vimarchy => Application::Vimarchy,
            Self::Ask => Application::Ask,
            Self::Yoohoo => Application::Yoohoo,
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Vimarchy => 0,
            Self::Ask => 1,
            Self::Yoohoo => 2,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub role: Role,
    pub process: Identity,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Dialect {
    Legacy,
    Lua,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositorConfig {
    pub runtime: PathBuf,
    pub instance: String,
    pub process: Identity,
    pub dialect: Dialect,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresenterConfig {
    pub role: Role,
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub controller_socket: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub vimarchy_numeric_workspace_moves: bool,
    pub vimarchy_settings: Option<PathBuf>,
    pub version: u32,
    pub runtime: PathBuf,
    pub compositor: CompositorConfig,
    pub controllers: Vec<Registration>,
    pub presenters: Vec<PresenterConfig>,
}
/// Open an absolute path component by component; reject parent traversal and symlinks.
fn open_absolute(path: &Path, directory: bool) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::other("absolute path required"));
    }
    let parts: Vec<_> = path.components().collect();
    if parts.len() > 128 {
        return Err(io::Error::other("path limit"));
    }
    let mut current = File::open("/")?;
    for (i, part) in parts.iter().enumerate() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                let name = CString::new(name.as_encoded_bytes()).map_err(io::Error::other)?;
                let is_dir = directory || i + 1 < parts.len();
                let flags = libc::O_RDONLY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK
                    | if is_dir { libc::O_DIRECTORY } else { 0 };
                let raw = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
                if raw < 0 {
                    return Err(io::Error::last_os_error());
                }
                current = unsafe { File::from_raw_fd(raw) };
            }
            _ => return Err(io::Error::other("path traversal refused")),
        }
    }
    Ok(current)
}
fn private_dir(path: &Path) -> io::Result<File> {
    let file = open_absolute(path, true)?;
    let m = file.metadata()?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o777 != 0o700 {
        return Err(io::Error::other("private directory must be owned 0700"));
    }
    Ok(file)
}
impl Config {
    pub fn read(path: &Path) -> io::Result<Self> {
        Self::read_with_digest(path).map(|(config, _)| config)
    }
    /// Hash the exact validated startup bytes; effects can reject any changed
    /// authority configuration rather than adopting unrelated fields silently.
    pub fn read_with_digest(path: &Path) -> io::Result<(Self, [u8; 32])> {
        use sha2::{Digest, Sha256};
        private_dir(
            path.parent()
                .ok_or_else(|| io::Error::other("config parent"))?,
        )?;
        let mut file = open_absolute(path, false)?;
        let m = file.metadata()?;
        if !m.is_file()
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o777 != 0o600
            || m.nlink() != 1
            || m.len() > MAX_CONFIG as u64
        {
            return Err(io::Error::other(
                "configuration must be bounded private regular file",
            ));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_CONFIG as u64 + 1)
            .read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        if (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
            m.uid(),
            m.mode(),
            m.nlink(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
            after.uid(),
            after.mode(),
            after.nlink(),
        ) {
            return Err(io::Error::other("configuration changed during read"));
        }
        if bytes.len() > MAX_CONFIG {
            return Err(io::Error::other("configuration limit"));
        }
        let config: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        config.validate()?;
        Ok((config, Sha256::digest(&bytes).into()))
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.version != 1
            || self.controllers.is_empty()
            || self.controllers.len() > 32
            || self.presenters.len() != 3
        {
            return Err(io::Error::other("configuration cardinality/version"));
        }
        private_dir(&self.runtime)?;
        private_dir(&self.compositor.runtime)?;
        if self.compositor.instance.is_empty()
            || self.compositor.instance.len() > 128
            || !self
                .compositor
                .instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(io::Error::other("invalid compositor instance"));
        }
        let mut roles = [false; 3];
        let mut endpoints = std::collections::BTreeSet::new();
        for spec in &self.presenters {
            if std::mem::replace(&mut roles[spec.role.index()], true) {
                return Err(io::Error::other("duplicate presenter role"));
            }
            let parent = spec
                .controller_socket
                .parent()
                .ok_or_else(|| io::Error::other("socket parent"))?;
            private_dir(parent)?;
            if spec.controller_socket.file_name() != Some(std::ffi::OsStr::new("modal.sock"))
                || !endpoints.insert(&spec.controller_socket)
                || spec.controller_socket == self.runtime.join("modal.sock")
                || spec.controller_socket.as_os_str().len() > 100
            {
                return Err(io::Error::other(
                    "distinct private modal.sock endpoint required",
                ));
            }
            let program = open_absolute(&spec.program, false)?;
            let m = program.metadata()?;
            if !m.is_file()
                || m.mode() & 0o022 != 0
                || m.mode() & 0o111 == 0
                || (m.uid() != 0 && m.uid() != unsafe { libc::geteuid() })
            {
                return Err(io::Error::other("untrusted presenter executable"));
            }
            let mut keys = std::collections::BTreeSet::new();
            let mut size = spec.program.as_os_str().len();
            if spec.arguments.len() + 2 * spec.environment.len() > 240 {
                return Err(io::Error::other("presenter item limit"));
            }
            for argument in &spec.arguments {
                size = size
                    .checked_add(argument.len())
                    .ok_or_else(|| io::Error::other("argument limit"))?;
                if argument.contains('\0') {
                    return Err(io::Error::other("nul argument"));
                }
            }
            for (key, value) in &spec.environment {
                size = size
                    .checked_add(key.len() + value.len())
                    .ok_or_else(|| io::Error::other("environment limit"))?;
                if key.is_empty()
                    || key.contains(['=', '\0'])
                    || value.contains('\0')
                    || key.starts_with("OMARCHY_MODAL_")
                    || !keys.insert(key)
                {
                    return Err(io::Error::other("invalid environment"));
                }
            }
            if size > 60000 {
                return Err(io::Error::other("presenter bytes limit"));
            }
        }
        let mut pids = std::collections::BTreeSet::new();
        for controller in &self.controllers {
            if !pids.insert(controller.process.pid) {
                return Err(io::Error::other("duplicate controller PID"));
            }
        }
        Ok(())
    }
    /// Retain each exact no-follow executable inode until the host exits.
    pub fn pin_presenters(&self) -> io::Result<[File; 3]> {
        self.validate()?;
        let mut files = Vec::new();
        for index in 0..3 {
            let spec = self
                .presenters
                .iter()
                .find(|p| p.role.index() == index)
                .ok_or_else(|| io::Error::other("missing presenter"))?;
            files.push(open_absolute(&spec.program, false)?);
        }
        files
            .try_into()
            .map_err(|_| io::Error::other("presenter count"))
    }
    pub fn presenter_specs(&self) -> [PresenterSpec; 3] {
        std::array::from_fn(|index| {
            let spec = self
                .presenters
                .iter()
                .find(|p| p.role.index() == index)
                .expect("validated config");
            PresenterSpec {
                program: spec.program.clone(),
                arguments: spec.arguments.iter().map(Into::into).collect(),
                environment: spec
                    .environment
                    .iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect(),
            }
        })
    }
    pub fn controller_endpoints(&self) -> [PathBuf; 3] {
        std::array::from_fn(|index| {
            self.presenters
                .iter()
                .find(|p| p.role.index() == index)
                .expect("validated config")
                .controller_socket
                .clone()
        })
    }
}
pub struct Admissions(Vec<(Application, Pinned)>);
impl Admissions {
    pub fn pin(config: &Config) -> io::Result<Self> {
        Ok(Self(
            config
                .controllers
                .iter()
                .map(|r| Ok((r.role.application(), Pinned::pin(r.process)?)))
                .collect::<io::Result<_>>()?,
        ))
    }
    pub fn admit(&self, uid: u32, pid: i32) -> Option<Application> {
        if uid != unsafe { libc::geteuid() } || pid <= 0 {
            return None;
        }
        self.0
            .iter()
            .find(|(_, p)| p.identity().pid == pid as u32 && p.check().is_ok())
            .map(|(app, _)| *app)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn fixture(root: &Path) -> serde_json::Value {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["runtime", "vimarchy", "ask", "yoohoo"] {
            let path = root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let process = Pinned::capture(std::process::id()).unwrap().identity();
        serde_json::json!({"version":1,"runtime":root.join("runtime"),"compositor":{"runtime":root.join("runtime"),"instance":"fixture","process":process,"dialect":"lua"},"controllers":[{"role":"vimarchy","process":process}],"presenters":(["vimarchy","ask","yoohoo"].map(|role|serde_json::json!({"role":role,"program":"/usr/bin/sleep","arguments":["30"],"environment":[],"controller_socket":root.join(role).join("modal.sock")})))})
    }
    fn write(root: &Path, value: &serde_json::Value) -> PathBuf {
        let path = root.join("config.json");
        std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
    #[test]
    fn private_config_pins_explicit_roles_and_rejects_unknown_or_ambiguous_input() {
        let root = tempfile::tempdir().unwrap();
        let value = fixture(root.path());
        let path = write(root.path(), &value);
        let config = Config::read(&path).unwrap();
        let policy = Admissions::pin(&config).unwrap();
        assert_eq!(
            policy.admit(unsafe { libc::geteuid() }, std::process::id() as i32),
            Some(Application::Vimarchy)
        );
        assert_eq!(
            policy.admit(unsafe { libc::geteuid() } + 1, std::process::id() as i32),
            None
        );
        for bad in [
            serde_json::json!({"unknown":true}),
            serde_json::json!({"version":2}),
        ] {
            let mut changed = value.clone();
            changed
                .as_object_mut()
                .unwrap()
                .extend(bad.as_object().unwrap().clone());
            assert!(Config::read(&write(root.path(), &changed)).is_err());
        }
        let mut duplicate = value.clone();
        duplicate["presenters"][1]["role"] = serde_json::json!("vimarchy");
        assert!(Config::read(&write(root.path(), &duplicate)).is_err());
        let mut arbitrary = value.clone();
        arbitrary["presenters"][0]["controller_socket"] = serde_json::json!("/tmp/attacker.sock");
        assert!(Config::read(&write(root.path(), &arbitrary)).is_err());
        let mut reserved = value.clone();
        reserved["presenters"][0]["environment"] =
            serde_json::json!([["OMARCHY_MODAL_CONTROLLER_PID", "1"]]);
        assert!(Config::read(&write(root.path(), &reserved)).is_err());
    }
    #[test]
    fn config_symlink_hardlink_mode_and_oversize_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = write(root.path(), &fixture(root.path()));
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(Config::read(&alias).is_err());
        std::fs::remove_file(&alias).unwrap();
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(Config::read(&path).is_err());
        std::fs::remove_file(alias).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Config::read(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, vec![b' '; MAX_CONFIG + 1]).unwrap();
        assert!(Config::read(&path).is_err());
    }
}
