use crate::{Error, launch::Command};
use serde::de::{Deserialize, Deserializer, Error as DeError, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

struct Strict(Value);
impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Strict;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bounded JSON value without duplicate keys")
            }
            fn visit_bool<E: DeError>(self, v: bool) -> Result<Strict, E> {
                Ok(Strict(Value::Bool(v)))
            }
            fn visit_i64<E: DeError>(self, v: i64) -> Result<Strict, E> {
                Ok(Strict(Value::Number(v.into())))
            }
            fn visit_u64<E: DeError>(self, v: u64) -> Result<Strict, E> {
                Ok(Strict(Value::Number(v.into())))
            }
            fn visit_f64<E: DeError>(self, v: f64) -> Result<Strict, E> {
                Number::from_f64(v)
                    .map(|n| Strict(Value::Number(n)))
                    .ok_or_else(|| E::custom("nonfinite"))
            }
            fn visit_str<E: DeError>(self, v: &str) -> Result<Strict, E> {
                Ok(Strict(Value::String(v.into())))
            }
            fn visit_string<E: DeError>(self, v: String) -> Result<Strict, E> {
                Ok(Strict(Value::String(v)))
            }
            fn visit_unit<E: DeError>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut out = Vec::new();
                while let Some(Strict(v)) = a.next_element()? {
                    if out.len() >= 1024 {
                        return Err(A::Error::custom("array limit"));
                    }
                    out.push(v);
                }
                Ok(Strict(Value::Array(out)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut out = Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if out.len() >= 256 || out.contains_key(&k) {
                        return Err(A::Error::custom("duplicate or excessive keys"));
                    }
                    let Strict(v) = a.next_value()?;
                    out.insert(k, v);
                }
                Ok(Strict(Value::Object(out)))
            }
        }
        d.deserialize_any(V)
    }
}
pub fn strict_object(bytes: &[u8]) -> Result<Map<String, Value>, Error> {
    if bytes.len() > 65_536 {
        return Err(Error::LimitExceeded);
    }
    let Strict(value) = serde_json::from_slice(bytes).map_err(|_| Error::InvalidConfig)?;
    fn depth(v: &Value, n: usize) -> bool {
        n <= 32
            && match v {
                Value::Array(a) => a.iter().all(|v| depth(v, n + 1)),
                Value::Object(o) => o.values().all(|v| depth(v, n + 1)),
                _ => true,
            }
    }
    if !depth(&value, 0) {
        return Err(Error::LimitExceeded);
    }
    match value {
        Value::Object(o) => Ok(o),
        _ => Err(Error::InvalidConfig),
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyPolicy {
    Ask,
    ExplicitYolo,
}
#[derive(Clone)]
pub struct Preferences {
    pub font_scale: f64,
    pub debounce_ms: u64,
    pub permission: LegacyPolicy,
    pub file_open: Option<Command>,
    pub file_edit: Option<Command>,
    /// Preserve unknown keys without interpreting them as commands or authority.
    pub extras: Map<String, Value>,
}
impl Preferences {
    pub fn import(bytes: &[u8]) -> Result<Self, Error> {
        let mut o = strict_object(bytes)?;
        let font_scale = match o.remove("fontScale") {
            None => 1.0,
            Some(v) => v
                .as_f64()
                .filter(|n| n.is_finite() && (0.7..=2.0).contains(n))
                .ok_or(Error::InvalidConfig)?,
        };
        let debounce_ms = match o.remove("searchDebounceMs") {
            None => 270,
            Some(v) => v
                .as_u64()
                .filter(|n| *n <= 2000)
                .ok_or(Error::InvalidConfig)?,
        };
        let permission = match o.remove("permissionMode") {
            None => LegacyPolicy::Ask,
            Some(Value::String(v)) if v == "permission" => LegacyPolicy::Ask,
            Some(Value::String(v)) if v == "yolo" => LegacyPolicy::ExplicitYolo,
            _ => return Err(Error::InvalidConfig),
        };
        let file_open = o
            .remove("fileOpenCommand")
            .as_ref()
            .map(Command::legacy_value)
            .transpose()?;
        let file_edit = o
            .remove("fileEditCommand")
            .as_ref()
            .map(Command::legacy_value)
            .transpose()?;
        Ok(Self {
            font_scale,
            debounce_ms,
            permission,
            file_open,
            file_edit,
            extras: o,
        })
    }
    /// Pure proposed write: IO adapter owns CAS, no-follow opens and durable commit.
    pub fn export(&self) -> Value {
        let mut o = self.extras.clone();
        o.insert("fontScale".into(), Value::from(self.font_scale));
        o.insert("searchDebounceMs".into(), self.debounce_ms.into());
        o.insert(
            "permissionMode".into(),
            Value::from(match self.permission {
                LegacyPolicy::Ask => "permission",
                LegacyPolicy::ExplicitYolo => "yolo",
            }),
        );
        for (key, command) in [
            ("fileOpenCommand", &self.file_open),
            ("fileEditCommand", &self.file_edit),
        ] {
            if let Some(command) = command {
                o.insert(
                    key.into(),
                    Value::Array(command.args().iter().cloned().map(Value::String).collect()),
                );
            }
        }
        Value::Object(o)
    }
}
