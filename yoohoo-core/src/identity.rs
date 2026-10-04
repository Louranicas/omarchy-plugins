use crate::Error;
use serde::Serialize;

/// Compositor address accepted only as hexadecimal data; never a dispatch fragment.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Address(String);
impl Address {
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let hex = raw.strip_prefix("0x").ok_or(Error::InvalidIdentity)?;
        if hex.is_empty() || hex.len() > 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::InvalidIdentity);
        }
        let value = u64::from_str_radix(hex, 16).map_err(|_| Error::InvalidIdentity)?;
        if value == 0 {
            return Err(Error::InvalidIdentity);
        }
        Ok(Self(format!("0x{value:x}")))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Address(<redacted>)")
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct WindowKey {
    instance: String,
    address: Address,
    stable_id: String,
}
impl WindowKey {
    pub fn new(instance: &str, address: Address, stable_id: &str) -> Result<Self, Error> {
        for id in [instance, stable_id] {
            if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
                return Err(Error::InvalidIdentity);
            }
        }
        Ok(Self {
            instance: instance.into(),
            address,
            stable_id: stable_id.into(),
        })
    }
    pub fn instance(&self) -> &str {
        &self.instance
    }
    pub fn address(&self) -> &Address {
        &self.address
    }
    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }
}
impl std::fmt::Debug for WindowKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WindowKey(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessKey {
    pub pid: u32,
    pub start_ticks: u64,
}
impl ProcessKey {
    pub fn new(pid: u32, start_ticks: u64) -> Result<Self, Error> {
        if pid == 0 || start_ticks == 0 {
            return Err(Error::InvalidIdentity);
        }
        Ok(Self { pid, start_ticks })
    }
}
