use crate::{Error, Id};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Codex,
    Claude,
}
impl Harness {
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}
/// Caller distinguishes missing default file from permission/read failure.
pub fn resolve_harness(
    override_value: Option<&str>,
    default: Result<Option<&str>, Error>,
) -> Result<Harness, Error> {
    let explicit = override_value.unwrap_or("").trim();
    let selected = if !explicit.is_empty() {
        explicit
    } else {
        default?.unwrap_or("").trim()
    };
    match selected {
        "codex" => Ok(Harness::Codex),
        "claude" => Ok(Harness::Claude),
        "" => Err(Error::MissingHarness),
        _ => Err(Error::Unsupported),
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct Command(Vec<String>);
impl Command {
    pub fn new(args: Vec<String>) -> Result<Self, Error> {
        if args.is_empty()
            || args.len() > 64
            || args[0].is_empty()
            || args.iter().any(|a| a.len() > 16_384 || a.contains('\0'))
            || args.iter().map(String::len).sum::<usize>() > 65_536
        {
            return Err(Error::InvalidConfig);
        }
        Ok(Self(args))
    }
    pub fn args(&self) -> &[String] {
        &self.0
    }
    pub fn append_operand(&self, path: &str) -> Result<Self, Error> {
        let mut args = self.0.clone();
        args.push(path.into());
        Self::new(args)
    }
    pub fn legacy_value(value: &serde_json::Value) -> Result<Self, Error> {
        match value {
            serde_json::Value::String(s) => Self::new(vec![s.clone()]),
            serde_json::Value::Array(v) => Self::new(
                v.iter()
                    .map(|x| x.as_str().map(str::to_owned).ok_or(Error::InvalidConfig))
                    .collect::<Result<_, _>>()?,
            ),
            _ => Err(Error::InvalidConfig),
        }
    }
}
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Command(<redacted argv>)")
    }
}
/// Exact JS precedence: nonempty harness-specific env wins, then shared, then explicit
/// installed adapter default. Whitespace specific becomes default, not shared fallback.
pub fn acp_command(
    specific: Option<&str>,
    shared: Option<&str>,
    installed_default: Command,
) -> Result<Command, Error> {
    let raw = specific
        .filter(|s| !s.is_empty())
        .or(shared)
        .unwrap_or("")
        .trim();
    if raw.is_empty() {
        return Ok(installed_default);
    }
    if raw.len() > 65_536 {
        return Err(Error::LimitExceeded);
    }
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| Error::InvalidConfig)?;
    let array = value.as_array().ok_or(Error::InvalidConfig)?;
    if array.iter().any(|a| a.as_str().is_none_or(str::is_empty)) {
        return Err(Error::InvalidConfig);
    }
    Command::legacy_value(&value)
}
pub fn bridge_prefix(raw: Option<&str>, installed_default: Command) -> Result<Command, Error> {
    acp_command(raw, None, installed_default)
}
/// No filesystem access here. The adapter checks regular/executable identity at launch.
/// Explicit override failure never silently falls back to another harness.
pub fn executable_candidates(
    harness: Harness,
    override_value: Option<&str>,
    path: &str,
) -> Result<Vec<PathBuf>, Error> {
    if path.len() > 32_768 {
        return Err(Error::LimitExceeded);
    }
    let executable = override_value
        .filter(|s| !s.is_empty())
        .unwrap_or(harness.name());
    if executable.len() > 4096 || executable.contains('\0') {
        return Err(Error::InvalidConfig);
    }
    if executable.contains('/') {
        return Ok(vec![PathBuf::from(executable)]);
    }
    let entries: Vec<_> = path.split(':').filter(|s| !s.is_empty()).collect();
    if entries.len() > 256 {
        return Err(Error::LimitExceeded);
    }
    Ok(entries
        .into_iter()
        .map(|dir| Path::new(dir).join(executable))
        .collect())
}
pub fn choose_executable(
    candidates: &[PathBuf],
    mut regular_executable: impl FnMut(&Path) -> bool,
) -> Result<PathBuf, Error> {
    candidates
        .iter()
        .find(|p| regular_executable(p))
        .cloned()
        .ok_or(Error::MissingExecutable)
}
pub fn cwd(
    explicit: Option<&str>,
    home: Option<&str>,
    process_cwd: &Path,
) -> Result<PathBuf, Error> {
    let path = explicit
        .filter(|s| !s.is_empty())
        .or(home.filter(|s| !s.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| process_cwd.to_owned());
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path.as_os_str().as_encoded_bytes().contains(&0)
    {
        return Err(Error::InvalidConfig);
    }
    Ok(path)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenLaunch {
    pub harness: Harness,
    pub cwd: PathBuf,
    pub command: Command,
    pub model: Option<Id>,
    pub reasoning: Option<Id>,
}
