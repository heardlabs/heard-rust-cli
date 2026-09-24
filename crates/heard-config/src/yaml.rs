//! YAML in, `serde_json::Value` out — `yaml.safe_load` / `yaml.safe_dump`
//! exactly as `config.py` calls them, via the PyYAML port in
//! [`crate::pyyaml`].
//!
//! The config layers are plain data, so everything is normalised to
//! `serde_json::Value`: that is the type the fixtures are written in, and it
//! keeps one representation across the crate instead of two.

use crate::error::{ConfigError, Result};
use crate::pyyaml::{self, Py, YamlError};
use serde_json::{Map, Value};
use std::path::Path;

/// What a YAML document became. `Falsy` exists because Python writes
/// `yaml.safe_load(f) or {}` — a document that parses to `None`, `False`, `0`,
/// `0.0`, `""`, `[]` or `{}` is silently an empty config, and only a TRUTHY
/// non-mapping blows up.
pub(crate) enum Parsed {
    Mapping(Map<String, Value>),
    Falsy,
    Truthy(&'static str),
}

/// `yaml.safe_load(text)`, classified.
pub(crate) fn parse(text: &str) -> std::result::Result<Parsed, YamlError> {
    let py = pyyaml::load_py(text)?;
    if !py.truthy() {
        return Ok(Parsed::Falsy);
    }
    Ok(match &py {
        Py::Dict(_) => match py.to_json() {
            Value::Object(m) => Parsed::Mapping(m),
            _ => unreachable!("a dict converts to an object"),
        },
        other => Parsed::Truthy(other.kind_name()),
    })
}

/// `yaml.safe_dump(data, sort_keys=True, allow_unicode=True)`.
pub(crate) fn dump(data: &Map<String, Value>) -> String {
    pyyaml::safe_dump(&Value::Object(data.clone()))
}

/// Turn a classified document into the mapping `load()` merges, or the error
/// Python would have raised out of `dict.update`.
pub(crate) fn mapping_or_error(parsed: Parsed, path: &Path) -> Result<Map<String, Value>> {
    match parsed {
        Parsed::Mapping(m) => Ok(m),
        Parsed::Falsy => Ok(Map::new()),
        Parsed::Truthy(found) => Err(ConfigError::NotAMapping {
            path: path.to_path_buf(),
            found,
        }),
    }
}
