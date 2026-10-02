//! YAML accessors, merge-key resolution and path normalization for the Compose parser.

use super::*;

pub(crate) fn resolve_merges(value: Value, path: &str) -> Result<Value, ComposeError> {
    match value {
        Value::Sequence(values) => values
            .into_iter()
            .enumerate()
            .map(|(index, value)| resolve_merges(value, &format!("{path}[{index}]")))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Sequence),
        Value::Mapping(values) => {
            let mut output = Mapping::new();
            let mut merge_values = Vec::new();
            for (key, value) in values {
                if key.as_str() == Some("<<") {
                    merge_values.push(value);
                } else {
                    output.insert(key, resolve_merges(value, path)?);
                }
            }
            for merge in merge_values.into_iter().rev() {
                let sources = match merge {
                    Value::Mapping(map) => vec![map],
                    Value::Sequence(items) => items
                        .into_iter()
                        .map(|item| match item {
                            Value::Mapping(map) => Ok(map),
                            _ => Err(shape(path, "YAML merge values must be mappings")),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => return Err(shape(path, "YAML merge values must be mappings")),
                };
                for source in sources.into_iter().rev() {
                    for (key, value) in source {
                        output.entry(key).or_insert(resolve_merges(value, path)?);
                    }
                }
            }
            Ok(Value::Mapping(output))
        }
        other => Ok(other),
    }
}

pub(crate) fn check_keys(map: &Mapping, allowed: &[&str], path: &str) -> Result<(), ComposeError> {
    for key in map.keys() {
        let key = key_string(key, path)?;
        if key.starts_with("x-") || allowed.contains(&key.as_str()) {
            continue;
        }
        return Err(ComposeError::unsupported(
            join(path, &key),
            format!("supported fields are {}", allowed.join(", ")),
        ));
    }
    Ok(())
}

pub(crate) fn mapping<'a>(value: &'a Value, path: &str) -> Result<&'a Mapping, ComposeError> {
    value
        .as_mapping()
        .ok_or_else(|| shape(path, "must be a YAML mapping"))
}
pub(crate) fn sequence<'a>(value: &'a Value, path: &str) -> Result<&'a Vec<Value>, ComposeError> {
    value
        .as_sequence()
        .ok_or_else(|| shape(path, "must be a YAML list"))
}
pub(crate) fn get<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(Value::String(key.into()))
}
pub(crate) fn key_string(value: &Value, path: &str) -> Result<String, ComposeError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| shape(path, "mapping keys must be strings"))
}
pub(crate) fn required_mapping<'a>(
    map: &'a Mapping,
    key: &str,
    path: &str,
) -> Result<&'a Mapping, ComposeError> {
    map.get(Value::String(key.into()))
        .ok_or_else(|| invalid(path, format!("missing required {key:?} mapping")))
        .and_then(|value| mapping(value, path))
}
pub(crate) fn required_string(
    map: &Mapping,
    key: &str,
    path: &str,
) -> Result<String, ComposeError> {
    get(map, key)
        .ok_or_else(|| invalid(path, format!("missing required {key:?} string")))
        .and_then(|value| scalar_string(value, path))
}
pub(crate) fn optional_string(
    map: &Mapping,
    key: &str,
    path: &str,
) -> Result<Option<String>, ComposeError> {
    get(map, key)
        .map(|value| scalar_string(value, path))
        .transpose()
}
pub(crate) fn optional_bool(
    map: &Mapping,
    key: &str,
    path: &str,
) -> Result<Option<bool>, ComposeError> {
    get(map, key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| shape(path, "must be a boolean"))
        })
        .transpose()
}
pub(crate) fn optional_u64(
    map: &Mapping,
    key: &str,
    path: &str,
) -> Result<Option<u64>, ComposeError> {
    get(map, key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| shape(path, "must be a non-negative integer"))
        })
        .transpose()
}
pub(crate) fn optional_string_list(
    map: &Mapping,
    key: &str,
    path: &str,
) -> Result<Option<Vec<String>>, ComposeError> {
    get(map, key)
        .map(|value| string_list(value, path))
        .transpose()
}
pub(crate) fn optional_command(
    map: &Mapping,
    key: &str,
    path: &str,
) -> Result<Option<Vec<String>>, ComposeError> {
    get(map, key)
        .map(|value| string_or_list(value, path))
        .transpose()
}
pub(crate) fn scalar_string(value: &Value, path: &str) -> Result<String, ComposeError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        _ => Err(shape(path, "must be a scalar string, number, or boolean")),
    }
}
pub(crate) fn string_list(value: &Value, path: &str) -> Result<Vec<String>, ComposeError> {
    sequence(value, path)?
        .iter()
        .enumerate()
        .map(|(index, value)| scalar_string(value, &format!("{path}[{index}]")))
        .collect()
}
pub(crate) fn string_or_list(value: &Value, path: &str) -> Result<Vec<String>, ComposeError> {
    match value {
        Value::String(value) => Ok(vec![value.clone()]),
        Value::Sequence(_) => string_list(value, path),
        _ => Err(shape(path, "must be a string or list of strings")),
    }
}
pub(crate) fn parse_string_map(
    value: &Value,
    path: &str,
) -> Result<BTreeMap<String, String>, ComposeError> {
    let map = mapping(value, path)?;
    map.iter()
        .map(|(key, value)| {
            Ok((
                key_string(key, path)?,
                scalar_string(value, &join(path, &key_string(key, path)?))?,
            ))
        })
        .collect()
}
pub(crate) fn parse_string_map_or_list(
    value: &Value,
    path: &str,
) -> Result<BTreeMap<String, String>, ComposeError> {
    match value {
        Value::Mapping(_) => parse_string_map(value, path),
        Value::Sequence(values) => {
            let mut result = BTreeMap::new();
            for (index, value) in values.iter().enumerate() {
                let entry = scalar_string(value, &format!("{path}[{index}]"))?;
                let Some((key, value)) = entry.split_once('=') else {
                    return Err(ComposeError::new(
                        ComposeErrorCode::Ambiguous,
                        format!("{path}[{index}]"),
                        "list entries must contain an explicit key=value pair",
                        "use mapping syntax for inherited environment values",
                    ));
                };
                if key.is_empty() {
                    return Err(invalid(format!("{path}[{index}]"), "key must not be empty"));
                }
                result.insert(key.into(), value.into());
            }
            Ok(result)
        }
        _ => Err(shape(path, "must be a mapping or key=value list")),
    }
}
pub(crate) fn reject_present(map: &Mapping, key: &str, path: &str) -> Result<(), ComposeError> {
    if get(map, key).is_some() {
        return Err(ComposeError::unsupported(
            path,
            "nested mount options are not yet represented in Bosn's plan",
        ));
    }
    Ok(())
}
pub(crate) fn identifier(value: &str, path: &str) -> Result<(), ComposeError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ComposeError::new(
            ComposeErrorCode::InvalidValue,
            path,
            format!("{value:?} is not a safe Compose identifier"),
            "use a non-empty ASCII name containing only letters, digits, _, -, or .",
        ));
    }
    Ok(())
}
pub(crate) fn normalize_relative_path(
    value: &str,
    path: &str,
) -> Result<RelativePath, ComposeError> {
    if value.is_empty() || value.starts_with('/') || value.contains('\\') {
        return Err(unsafe_path(
            path,
            "path must be a non-empty slash-separated relative path",
        ));
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(unsafe_path(path, "path traversal is not permitted")),
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Ok(RelativePath(".".into()));
    }
    Ok(RelativePath(parts.join("/")))
}
pub(crate) fn normalize_container_path(value: &str, path: &str) -> Result<String, ComposeError> {
    if !value.starts_with('/') || value.contains('\\') {
        return Err(unsafe_path(
            path,
            "container target must be an absolute slash-separated path",
        ));
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(unsafe_path(
                    path,
                    "container target must not traverse with ..",
                ));
            }
            _ => parts.push(part),
        }
    }
    Ok(if parts.is_empty() {
        "/".into()
    } else {
        format!("/{}", parts.join("/"))
    })
}
pub(crate) fn join(prefix: &str, field: &str) -> String {
    if prefix == "compose" {
        field.into()
    } else {
        format!("{prefix}.{field}")
    }
}
pub(crate) fn shape(path: impl Into<String>, message: impl Into<String>) -> ComposeError {
    ComposeError::new(
        ComposeErrorCode::InvalidShape,
        path,
        message,
        "use the documented typed Compose shape",
    )
}
pub(crate) fn invalid(path: impl Into<String>, message: impl Into<String>) -> ComposeError {
    ComposeError::new(
        ComposeErrorCode::InvalidValue,
        path,
        message,
        "correct the value before planning",
    )
}
pub(crate) fn unsafe_path(path: impl Into<String>, message: impl Into<String>) -> ComposeError {
    ComposeError::new(
        ComposeErrorCode::UnsafePath,
        path,
        message,
        "use a relative path contained by the selected root",
    )
}
