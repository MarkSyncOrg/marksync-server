//! Route selection by `Accept-Version`, reproducing `express-routes-versioning@1.0.1`.
//!
//! That library does not implement semver ranges: `~x.y.z` matches on `major.minor`,
//! `^x.y.z` matches on `major` only, and mappings are tried in declaration order. The
//! first match wins; no match is an `UnsupportedVersionException` (412).

/// Returns the index of the first `mappings` entry that matches `version`.
pub fn select(version: &str, mappings: &[&str]) -> Option<usize> {
    mappings.iter().position(|key| matches(version, key))
}

fn matches(version: &str, key: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    // JS `versionArr[i] || 0`: missing or empty components count as "0".
    let part = |i: usize| parts.get(i).copied().filter(|p| !p.is_empty()).unwrap_or("0");
    if let Some(key) = key.strip_prefix('~') {
        let key: Vec<&str> = key.split('.').take(2).collect();
        key.join(".") == format!("{}.{}", parts[0], part(1))
    } else if let Some(key) = key.strip_prefix('^') {
        key.split('.').next() == Some(parts[0])
    } else {
        // Exact keys pad the requested version to three components.
        let mut padded = parts.clone();
        while padded.len() < 3 {
            padded.push("0");
        }
        key == padded.join(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATE: &[&str] = &["~1.0.0", "^1.1.3"];

    #[test]
    fn tilde_matches_major_minor() {
        assert_eq!(select("1.0.5", CREATE), Some(0));
        assert_eq!(select("1", CREATE), Some(0));
        assert_eq!(select("1.0", CREATE), Some(0));
    }

    #[test]
    fn caret_matches_major() {
        assert_eq!(select("1.1.13", CREATE), Some(1));
        assert_eq!(select("1.1.0", CREATE), Some(1));
        assert_eq!(select("1.9", CREATE), Some(1));
        assert_eq!(select("1.1.13", &["^1.0.0"]), Some(0));
    }

    #[test]
    fn rejects_other_majors_and_garbage() {
        assert_eq!(select("2.0.0", CREATE), None);
        assert_eq!(select("0.9.0", &["^1.0.0"]), None);
        assert_eq!(select("abc", &["^1.0.0"]), None);
        assert_eq!(select("", &["^1.0.0"]), None);
    }

    #[test]
    fn exact_keys_pad_version() {
        assert_eq!(select("1", &["1.0.0"]), Some(0));
        assert_eq!(select("1.0.1", &["1.0.0"]), None);
    }
}
