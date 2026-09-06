//! Attribute path parsing.
//!
//! The wire format is `endpoint/cluster/attribute`, where any segment may be
//! `*` for a wildcard. Reads accept wildcards; writes do not, so the two entry
//! points differ only in whether they reject an unresolved segment.

use rs_matter::im::{AttrPath, GenericPath};
use serde_json::Value;

use super::error::ApiError;
use super::message::Args;

/// A parsed path with each segment still optional.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedPath {
    pub endpoint: Option<u16>,
    pub cluster: Option<u32>,
    pub attribute: Option<u32>,
}

impl ParsedPath {
    pub fn to_attr_path(self) -> AttrPath {
        AttrPath::from_gp(&GenericPath::new(
            self.endpoint,
            self.cluster,
            self.attribute,
        ))
    }

    pub fn has_wildcard(self) -> bool {
        self.endpoint.is_none() || self.cluster.is_none() || self.attribute.is_none()
    }
}

/// Render a concrete path back into wire form.
pub fn format_path(endpoint: u16, cluster: u32, attribute: u32) -> String {
    format!("{}/{}/{}", endpoint, cluster, attribute)
}

pub fn parse_path(raw: &str) -> Result<ParsedPath, ApiError> {
    let parts: Vec<&str> = raw.split('/').collect();
    if parts.len() != 3 {
        return Err(ApiError::invalid_args(format!(
            "Invalid attribute path '{}': expected endpoint/cluster/attribute",
            raw
        )));
    }
    let segment = |raw: &str, name: &str, max: u64| -> Result<Option<u64>, ApiError> {
        if raw == "*" {
            return Ok(None);
        }
        let value = raw.parse::<u64>().map_err(|_| {
            ApiError::invalid_args(format!("Invalid {} '{}' in attribute path", name, raw))
        })?;
        if value > max {
            return Err(ApiError::invalid_args(format!(
                "Invalid {} '{}' in attribute path",
                name, raw
            )));
        }
        Ok(Some(value))
    };
    Ok(ParsedPath {
        endpoint: segment(parts[0], "endpoint", u16::MAX as u64)?.map(|v| v as u16),
        cluster: segment(parts[1], "cluster", u32::MAX as u64)?.map(|v| v as u32),
        attribute: segment(parts[2], "attribute", u32::MAX as u64)?.map(|v| v as u32),
    })
}

/// Read the `attribute_path` argument, which is a single path or a list.
pub fn parse_path_arg(args: &Args) -> Result<Vec<ParsedPath>, ApiError> {
    let value = args
        .get("attribute_path")
        .ok_or_else(|| ApiError::invalid_args("Missing attribute_path"))?;
    let paths = match value {
        Value::String(path) => vec![parse_path(path)?],
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .ok_or_else(|| ApiError::invalid_args("attribute_path entries must be strings"))
                    .and_then(parse_path)
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(ApiError::invalid_args(
                "attribute_path must be a string or an array of strings",
            ))
        }
    };
    if paths.is_empty() {
        return Err(ApiError::invalid_args(
            "At least one attribute path is required",
        ));
    }
    Ok(paths)
}

/// Writes address exactly one fully-specified attribute.
pub fn parse_write_path(args: &Args) -> Result<(u16, u32, u32), ApiError> {
    let paths = parse_path_arg(args)?;
    if paths.len() != 1 {
        return Err(ApiError::invalid_args(
            "write_attribute accepts exactly one attribute path",
        ));
    }
    let path = paths[0];
    match (path.endpoint, path.cluster, path.attribute) {
        (Some(endpoint), Some(cluster), Some(attribute)) => Ok((endpoint, cluster, attribute)),
        _ => Err(ApiError::invalid_args(
            "write_attribute does not support wildcards in attribute path",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_concrete_and_wildcard_paths() {
        assert_eq!(
            parse_path("1/6/0").unwrap(),
            ParsedPath {
                endpoint: Some(1),
                cluster: Some(6),
                attribute: Some(0)
            }
        );
        let wildcard = parse_path("*/6/*").unwrap();
        assert_eq!(wildcard.endpoint, None);
        assert_eq!(wildcard.cluster, Some(6));
        assert!(wildcard.has_wildcard());
    }

    #[test]
    fn rejects_malformed_paths() {
        assert_eq!(parse_path("1/6").unwrap_err().code.as_i64(), 8);
        assert_eq!(parse_path("a/6/0").unwrap_err().code.as_i64(), 8);
        assert_eq!(parse_path("99999/6/0").unwrap_err().code.as_i64(), 8);
    }

    #[test]
    fn reads_accept_a_list_of_paths() {
        let args = Args::new(json!({ "attribute_path": ["1/6/0", "0/40/1"] }));
        let paths = parse_path_arg(&args).unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[1].cluster, Some(40));
    }

    #[test]
    fn writes_reject_wildcards() {
        let args = Args::new(json!({ "attribute_path": "1/6/*" }));
        let error = parse_write_path(&args).unwrap_err();
        assert!(error.details.contains("does not support wildcards"));
    }
}
