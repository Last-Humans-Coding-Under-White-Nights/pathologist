use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use url::Url;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn point(position: Position) -> Self {
        Self {
            start: position,
            end: position,
        }
    }
}

/// Component normalization works when source files or remote prefixes do not
/// exist. Canonicalize only local paths, so existing symlinks resolve exactly.
pub fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

pub fn local_path(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    let path = normalize(path);
    // Missing files still inherit symlinks in their existing parent path.
    for ancestor in path.ancestors() {
        if let Ok(resolved) = ancestor.canonicalize() {
            let remainder = path.strip_prefix(ancestor).unwrap();
            return if remainder.as_os_str().is_empty() {
                resolved
            } else {
                resolved.join(remainder)
            };
        }
    }
    path
}

pub fn uri_path(uri: &str) -> Result<PathBuf> {
    let url = Url::parse(uri).context("invalid document URI")?;
    if url.scheme() != "file"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_some_and(|host| host != "localhost")
    {
        bail!("expected a local file URI without a query or fragment");
    }
    let path = url
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("URI is not a local file path"))?;
    Ok(local_path(&path))
}

pub fn path_uri(path: &Path) -> Result<String> {
    Ok(Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("source path is not absolute: {}", path.display()))?
        .into())
}

#[derive(Debug, Clone)]
pub struct PathMapping {
    pub database: PathBuf,
    pub local: PathBuf,
}

impl std::str::FromStr for PathMapping {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let (database, local) = value
            .split_once('=')
            .context("path mapping must be DB_PREFIX=LOCAL_PREFIX")?;
        if !Path::new(database).is_absolute() || !Path::new(local).is_absolute() {
            bail!("path mapping prefixes must be absolute");
        }
        Ok(Self {
            database: normalize(Path::new(database)),
            local: local_path(Path::new(local)),
        })
    }
}

pub(crate) fn mapped_path(path: &Path, mappings: &[PathMapping]) -> PathBuf {
    // Longest component prefix wins; command-line order breaks ties.
    let mapping = mappings
        .iter()
        .filter(|m| path.starts_with(&m.database))
        .max_by_key(|m| m.database.components().count());
    local_path(&match mapping {
        Some(m) => m.local.join(path.strip_prefix(&m.database).unwrap()),
        None => path.to_path_buf(),
    })
}

pub(crate) struct Source {
    lines: Vec<String>,
}

impl Source {
    pub fn read(path: &Path) -> Option<Self> {
        std::fs::read_to_string(path)
            .ok()
            .map(|text| Self::from_text(&text))
    }

    pub fn from_text(text: &str) -> Self {
        Self {
            lines: text
                .split('\n')
                .map(|s| s.trim_end_matches('\r').to_owned())
                .collect(),
        }
    }

    pub fn line_end(&self, line: u32) -> Option<u32> {
        self.lines
            .get(line as usize)
            .map(|s| s.encode_utf16().count() as u32)
    }

    /// The preprocessor's stored columns count Unicode scalar values, not
    /// UTF-8 bytes. Convert that one-based column to UTF-16, clamping to EOL.
    pub fn position(&self, line: u32, col: u32) -> Position {
        let line = line.saturating_sub(1);
        let character = self.lines.get(line as usize).map_or(0, |s| {
            s.chars()
                .take(col.saturating_sub(1) as usize)
                .map(char::len_utf16)
                .sum::<usize>() as u32
        });
        Position { line, character }
    }
}

/// Full exported lines, with a point selection at the first line's start.
/// No guessed function-name or call-expression span is presented as precise.
pub(crate) fn function_range(start: u32, end: u32, source: Option<&Source>) -> Range {
    let end = end.max(start);
    Range {
        start: Position {
            line: start.saturating_sub(1),
            character: 0,
        },
        end: match source.and_then(|s| s.line_end(end.saturating_sub(1))) {
            Some(character) => Position {
                line: end.saturating_sub(1),
                character,
            },
            None => Position {
                line: end,
                character: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_coordinates_and_range_fallbacks() {
        let source = Source::from_text("/*é😀*/ run();\r\nsecond\n");
        assert_eq!(
            source.position(1, 8),
            Position {
                line: 0,
                character: 8
            }
        );
        assert_eq!(source.position(1, 999).character, 14);
        assert_eq!(
            source.position(99, 5),
            Position {
                line: 98,
                character: 0
            }
        );
        let range = function_range(1, 2, Some(&source));
        assert_eq!(
            range.end,
            Position {
                line: 1,
                character: 6
            }
        );
        assert!(range.start <= Range::point(range.start).end);
        assert_eq!(
            function_range(2, 3, None).end,
            Position {
                line: 3,
                character: 0
            }
        );
    }

    #[test]
    fn uris_spaces_unicode_and_component_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a space é #%.cpp");
        std::fs::write(&path, "").unwrap();
        let uri = path_uri(&local_path(&path)).unwrap();
        assert!(uri.contains("%20") && uri.contains("%23"));
        assert_eq!(uri_path(&uri).unwrap(), local_path(&path));
        assert!(Source::read(&path).is_some());
        std::fs::write(&path, [0xff]).unwrap();
        assert!(Source::read(&path).is_none());
        let mut remote_uri = Url::parse(&uri).unwrap();
        remote_uri.set_host(Some("remote")).unwrap();
        for invalid in [
            "https://example.org/a.c".to_owned(),
            format!("{uri}?x"),
            format!("{uri}#x"),
            remote_uri.into(),
            "invalid".to_owned(),
        ] {
            assert!(uri_path(&invalid).is_err(), "{invalid}");
        }
        let remote = local_path(dir.path()).join("remote/project");
        let mapping: PathMapping = format!("{}={}", remote.display(), dir.path().display())
            .parse()
            .unwrap();
        assert_eq!(
            mapped_path(&remote.join("a/../file.c"), std::slice::from_ref(&mapping)),
            local_path(&dir.path().join("file.c"))
        );
        let different_prefix = local_path(dir.path()).join("remote/project-other/file.c");
        assert_eq!(
            mapped_path(&different_prefix, std::slice::from_ref(&mapping)),
            different_prefix
        );
        let specific: PathMapping = format!(
            "{}={}",
            remote.join("nested").display(),
            dir.path().join("specific").display()
        )
        .parse()
        .unwrap();
        assert_eq!(
            mapped_path(&remote.join("nested/file.c"), &[specific, mapping.clone()]),
            local_path(&dir.path().join("specific/file.c"))
        );
        let last: PathMapping =
            format!("{}={}", remote.display(), dir.path().join("last").display())
                .parse()
                .unwrap();
        assert_eq!(
            mapped_path(&remote.join("file.c"), &[mapping, last]),
            local_path(&dir.path().join("last/file.c"))
        );
        assert!(format!("relative={}", dir.path().display())
            .parse::<PathMapping>()
            .is_err());
    }
}
