use crate::{Error, MAX_FRAME};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};
use std::fmt;

struct Strict(Value);
impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Strict;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("unique-key JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Strict, E> {
                Number::from_f64(v)
                    .map(|n| Strict(Value::Number(n)))
                    .ok_or_else(|| E::custom("nonfinite"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut v = Vec::new();
                while let Some(Strict(x)) = a.next_element()? {
                    v.push(x);
                }
                Ok(Strict(Value::Array(v)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut v = Map::new();
                while let Some((k, Strict(x))) = a.next_entry::<String, Strict>()? {
                    if v.insert(k, x).is_some() {
                        return Err(de::Error::custom("duplicate key"));
                    }
                }
                Ok(Strict(Value::Object(v)))
            }
        }
        d.deserialize_any(V)
    }
}

pub fn validate(v: &Value) -> Result<(), Error> {
    let m = v.as_object().ok_or(Error::Protocol)?;
    if m.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || m.keys().any(|k| {
            !matches!(
                k.as_str(),
                "jsonrpc" | "id" | "method" | "params" | "result" | "error"
            )
        })
    {
        return Err(Error::Protocol);
    }
    if let Some(id) = m.get("id")
        && !(id.is_i64()
            || id.is_u64()
            || id.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 256)
            || (id.is_null() && m.contains_key("error")))
    {
        return Err(Error::Protocol);
    }
    if let Some(method) = m.get("method") {
        if !method
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
            || m.contains_key("result")
            || m.contains_key("error")
            || m.get("params")
                .is_some_and(|v| !v.is_object() && !v.is_array())
        {
            return Err(Error::Protocol);
        }
    } else {
        if !m.contains_key("id")
            || m.contains_key("params")
            || m.contains_key("result") == m.contains_key("error")
        {
            return Err(Error::Protocol);
        }
        if let Some(e) = m.get("error") {
            let e = e.as_object().ok_or(Error::Protocol)?;
            if !e.get("code").is_some_and(Value::is_i64)
                || !e.get("message").is_some_and(Value::is_string)
                || e.keys()
                    .any(|k| !matches!(k.as_str(), "code" | "message" | "data"))
            {
                return Err(Error::Protocol);
            }
        }
    }
    Ok(())
}
pub fn decode(bytes: &[u8]) -> Result<Value, Error> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME || bytes.contains(&b'\n') {
        return Err(Error::Protocol);
    }
    let Strict(value) = serde_json::from_slice(bytes).map_err(|_| Error::Protocol)?;
    validate(&value)?;
    Ok(value)
}
pub fn encode(value: &Value) -> Result<Vec<u8>, Error> {
    validate(value)?;
    let mut bytes = serde_json::to_vec(value).map_err(|_| Error::Protocol)?;
    if bytes.len() > MAX_FRAME {
        return Err(Error::Limit);
    }
    bytes.push(b'\n');
    Ok(bytes)
}
