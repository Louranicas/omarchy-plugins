//! Explicit launch profiles. Reading or checking a profile never authenticates or launches a provider.
use ask_core::{
    Id,
    config::strict_object,
    launch::{Command, FrozenLaunch, Harness},
};
use serde_json::{Map, Value};
use std::{
    ffi::{CString, OsString},
    fs::{File, OpenOptions},
    io::Read,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
};
const MAX_PROFILE: usize = 65_536;
const ALLOWED_ENV: &[&str] = &[
    "HOME",
    "PATH",
    "LANG",
    "LC_ALL",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "CODEX_HOME",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileError {
    Invalid,
    Read,
    Unsafe,
    Changed,
    MissingExecutable,
    MissingDirectory,
    MissingEnvironment,
    EnvironmentLimit,
}
/// Debug deliberately omits paths, arguments, model names and environment values.
pub struct ProviderProfile {
    launch: FrozenLaunch,
    harness_executable: PathBuf,
    inherit: Vec<String>,
    source: Option<(PathBuf, Stamp)>,
}
impl std::fmt::Debug for ProviderProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProviderProfile(<redacted>)")
    }
}
type Stamp = (u64, u64, u64, u32, i64, i64);
fn stamp(m: &std::fs::Metadata) -> Stamp {
    (
        m.dev(),
        m.ino(),
        m.len(),
        m.mode(),
        m.ctime(),
        m.ctime_nsec(),
    )
}
/// Detects observed replacement since preflight, not an atomic path-to-exec pin.
pub struct Preflight {
    paths: Vec<(PathBuf, Stamp)>,
    profile: Option<(PathBuf, Stamp)>,
}
impl Preflight {
    pub fn recheck(&self) -> Result<(), ProfileError> {
        for (path, expected) in &self.paths {
            let m = std::fs::metadata(path).map_err(|_| ProfileError::Changed)?;
            if stamp(&m) != *expected {
                return Err(ProfileError::Changed);
            }
        }
        if let Some((path, expected)) = &self.profile {
            let current = ProviderProfile::read(path).map_err(|_| ProfileError::Changed)?;
            if current.source.as_ref().map(|(_, value)| value) != Some(expected) {
                return Err(ProfileError::Changed);
            }
        }
        Ok(())
    }
}
fn path(value: String) -> Result<PathBuf, ProfileError> {
    let p = PathBuf::from(value);
    if !p.is_absolute()
        || p.as_os_str().len() > 4096
        || p.as_os_str().as_bytes().contains(&0)
        || p.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err(ProfileError::Invalid);
    }
    Ok(p)
}
fn string(o: &mut Map<String, Value>, key: &str) -> Result<String, ProfileError> {
    o.remove(key)
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or(ProfileError::Invalid)
}
fn optional_id(o: &mut Map<String, Value>, key: &str) -> Result<Option<Id>, ProfileError> {
    o.remove(key)
        .map(|v| {
            v.as_str()
                .ok_or(ProfileError::Invalid)
                .and_then(|s| Id::new(s).map_err(|_| ProfileError::Invalid))
        })
        .transpose()
}
fn executable(p: &Path) -> Result<(), ProfileError> {
    let m = std::fs::metadata(p).map_err(|_| ProfileError::MissingExecutable)?;
    let c = CString::new(p.as_os_str().as_bytes()).map_err(|_| ProfileError::Invalid)?;
    if !m.is_file()
        || unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::X_OK, libc::AT_EACCESS) } != 0
    {
        return Err(ProfileError::MissingExecutable);
    }
    Ok(())
}
impl ProviderProfile {
    pub fn parse(bytes: &[u8]) -> Result<Self, ProfileError> {
        let mut o = strict_object(bytes).map_err(|_| ProfileError::Invalid)?;
        if o.remove("version") != Some(Value::from(1)) {
            return Err(ProfileError::Invalid);
        }
        let harness = match string(&mut o, "harness")?.as_str() {
            "codex" => Harness::Codex,
            "claude" => Harness::Claude,
            _ => return Err(ProfileError::Invalid),
        };
        let argv = o.remove("adapter_argv").ok_or(ProfileError::Invalid)?;
        let command = Command::legacy_value(&argv).map_err(|_| ProfileError::Invalid)?;
        if !argv.is_array() {
            return Err(ProfileError::Invalid);
        }
        path(command.args()[0].clone())?;
        let cwd = path(string(&mut o, "cwd")?)?;
        let harness_executable = path(string(&mut o, "harness_executable")?)?;
        let model = optional_id(&mut o, "model")?;
        let reasoning = optional_id(&mut o, "reasoning")?;
        let inherit = match o.remove("inherit_env") {
            None => vec![],
            Some(Value::Array(v)) if v.len() <= ALLOWED_ENV.len() => v
                .into_iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or(ProfileError::Invalid))
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(ProfileError::Invalid),
        };
        let mut names = std::collections::BTreeSet::new();
        if inherit
            .iter()
            .any(|name| !ALLOWED_ENV.contains(&name.as_str()) || !names.insert(name))
            || !o.is_empty()
        {
            return Err(ProfileError::Invalid);
        }
        Ok(Self {
            launch: FrozenLaunch {
                harness,
                cwd,
                command,
                model,
                reasoning,
            },
            harness_executable,
            inherit,
            source: None,
        })
    }
    /// Every path component is opened no-follow; the file is private, singly linked and owned by this user.
    pub fn read(profile: &Path) -> Result<Self, ProfileError> {
        if !profile.is_absolute() || profile.as_os_str().len() > 4096 {
            return Err(ProfileError::Invalid);
        }
        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open("/")
            .map_err(|_| ProfileError::Read)?;
        let parts: Vec<_> = profile
            .components()
            .filter(|p| !matches!(p, Component::RootDir))
            .collect();
        if parts.is_empty() {
            return Err(ProfileError::Invalid);
        }
        use std::os::fd::{AsRawFd, FromRawFd};
        let mut leaf = None;
        for (i, part) in parts.iter().enumerate() {
            let Component::Normal(name) = part else {
                return Err(ProfileError::Invalid);
            };
            let name = CString::new(name.as_bytes()).map_err(|_| ProfileError::Invalid)?;
            let last = i + 1 == parts.len();
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if last { 0 } else { libc::O_DIRECTORY };
            let raw = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            if raw < 0 {
                return Err(ProfileError::Unsafe);
            }
            let opened = unsafe { File::from_raw_fd(raw) };
            if last {
                leaf = Some(opened)
            } else {
                directory = opened
            }
        }
        let mut f = leaf.ok_or(ProfileError::Invalid)?;
        let before = f.metadata().map_err(|_| ProfileError::Read)?;
        if !before.is_file()
            || before.uid() != unsafe { libc::geteuid() }
            || before.mode() & 0o077 != 0
            || before.nlink() != 1
            || before.len() > MAX_PROFILE as u64
        {
            return Err(ProfileError::Unsafe);
        }
        let mut bytes = Vec::new();
        (&mut f)
            .take(MAX_PROFILE as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ProfileError::Read)?;
        let after = f.metadata().map_err(|_| ProfileError::Read)?;
        let identity = |m: &std::fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mode(),
                m.nlink(),
                m.uid(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        if bytes.len() > MAX_PROFILE
            || bytes.len() as u64 != before.len()
            || identity(&before) != identity(&after)
        {
            return Err(ProfileError::Changed);
        }
        let mut parsed = Self::parse(&bytes)?;
        parsed.source = Some((profile.to_owned(), stamp(&after)));
        Ok(parsed)
    }
    /// Filesystem availability only: no process execution, credential file access or auth claim.
    pub fn check(&self) -> Result<(), ProfileError> {
        executable(Path::new(&self.launch.command.args()[0]))?;
        executable(&self.harness_executable)?;
        if !self.launch.cwd.is_dir() {
            return Err(ProfileError::MissingDirectory);
        }
        Ok(())
    }
    pub fn preflight(&self) -> Result<Preflight, ProfileError> {
        self.check()?;
        let mut paths = vec![];
        for p in [
            Path::new(&self.launch.command.args()[0]),
            &self.harness_executable,
            &self.launch.cwd,
        ] {
            paths.push((
                p.to_owned(),
                stamp(&std::fs::metadata(p).map_err(|_| ProfileError::Changed)?),
            ));
        }
        let checked = Preflight {
            paths,
            profile: self.source.clone(),
        };
        checked.recheck()?;
        Ok(checked)
    }
    pub fn launch(&self) -> &FrozenLaunch {
        &self.launch
    }
    pub fn environment(
        &self,
        mut lookup: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Vec<(OsString, OsString)>, ProfileError> {
        let mut result = vec![];
        let mut size = 0;
        for key in &self.inherit {
            let value = lookup(key).ok_or(ProfileError::MissingEnvironment)?;
            size += key.len() + value.len();
            if value.as_os_str().as_bytes().contains(&0) || size > 65_536 {
                return Err(ProfileError::EnvironmentLimit);
            }
            result.push((key.into(), value));
        }
        let key = match self.launch.harness {
            Harness::Codex => "CODEX_PATH",
            Harness::Claude => "CLAUDE_CODE_EXECUTABLE",
        };
        result.push((key.into(), self.harness_executable.as_os_str().to_owned()));
        Ok(result)
    }
    pub fn redacted_status(&self) -> Value {
        serde_json::json!({"profile_version":1,"harness":self.launch.harness.name(),"environment_names_requested":self.inherit.len(),"model_configured":self.launch.model.is_some(),"reasoning_configured":self.launch.reasoning.is_some(),"authentication":"not_checked","environment_availability":"not_checked","policy":"interactive_only"})
    }
}
