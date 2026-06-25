// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Validates a relative path against a base path to prevent path traversal attacks.
///
/// Returns the canonicalized absolute path if it is safe and exists (or its parent exists),
/// otherwise returns an error.
pub fn validate_path(relative: &str, base: &Path) -> Result<PathBuf> {
    if relative.contains("..") || relative.starts_with('/') {
        return Err(anyhow!("Invalid path: {}", relative));
    }
    let full_path = base.join(relative);

    let canonical_base = base
        .canonicalize()
        .map_err(|e| anyhow!("Failed to canonicalize base path: {}", e))?;

    let canonical_full = match full_path.canonicalize() {
        Ok(p) => p,
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = full_path.parent() {
                let canonical_parent = parent
                    .canonicalize()
                    .map_err(|e| anyhow!("Failed to canonicalize parent path: {}", e))?;
                if !canonical_parent.starts_with(&canonical_base) {
                    return Err(anyhow!("Path traversal detected in parent: {:?}", parent));
                }
                full_path
            } else {
                return Err(anyhow!("No parent directory for path: {:?}", full_path));
            }
        }
        Err(e) => return Err(anyhow!("Failed to canonicalize path: {}", e)),
    };

    if !canonical_full.starts_with(&canonical_base) {
        return Err(anyhow!("Path traversal detected: {:?}", canonical_full));
    }

    Ok(canonical_full)
}

/// Converts a simple glob pattern (supporting `*` and `?`) into a compiled Regex.
pub fn glob_to_regex(glob: &str) -> Result<regex::Regex> {
    let mut regex_str = String::new();
    regex_str.push('^');
    for c in glob.chars() {
        match c {
            '*' => regex_str.push_str(".*"),
            '?' => regex_str.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\' => {
                regex_str.push('\\');
                regex_str.push(c);
            }
            _ => regex_str.push(c),
        }
    }
    regex_str.push('$');
    regex::Regex::new(&regex_str).map_err(|e| anyhow!("Invalid glob converted to regex: {}", e))
}

fn get_grep_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^([a-zA-Z0-9_./-]+)(:|-)([0-9]+)(:|-)(.*)$").unwrap())
}

/// Formats raw git grep output into a clean, grouped structure, sorting findings
/// by proximity to the files modified in the active patchset.
pub fn format_git_grep_output(stdout: &str, revision: &str, active_files: &[String]) -> String {
    let prefix = format!("{}:", revision);
    let re = get_grep_regex();

    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current_file: Option<String> = None;

    for line in stdout.lines() {
        if line == "--" {
            if let Some(ref cur) = current_file
                && let Some(list) = grouped.get_mut(cur)
            {
                list.push("  --".to_string());
            }
            continue;
        }

        let stripped = if line.starts_with(&prefix) {
            &line[prefix.len()..]
        } else {
            line
        };

        if let Some(caps) = re.captures(stripped) {
            let path = &caps[1];
            let sep1 = &caps[2];
            let line_num = &caps[3];
            let sep2 = &caps[4];
            let content = &caps[5];

            if sep1 == sep2 {
                let formatted_line = format!("  {}{}{}", line_num, sep1, content);
                let path_str = path.to_string();
                current_file = Some(path_str.clone());
                grouped.entry(path_str).or_default().push(formatted_line);
            } else if let Some(ref cur) = current_file {
                grouped
                    .entry(cur.clone())
                    .or_default()
                    .push(stripped.to_string());
            }
        } else if let Some(ref cur) = current_file {
            grouped
                .entry(cur.clone())
                .or_default()
                .push(stripped.to_string());
        }
    }

    // Proximity Ranking: sort matching files so that files closest to modified files appear first
    let mut blocks: Vec<(String, Vec<String>)> = grouped.into_iter().collect();
    blocks.sort_by_key(|(path, _)| (get_priority_score(path, active_files), path.clone()));

    let mut result = String::new();
    for (path, lines) in blocks {
        result.push_str(&format!("[file: {}]\n", path));
        for l in lines {
            result.push_str(&l);
            result.push('\n');
        }
        result.push('\n');
    }

    result.trim_end().to_string()
}

fn get_priority_score(path: &str, active_files: &[String]) -> u32 {
    if active_files.is_empty() {
        return 4;
    }

    // 1. Exact Match (highest priority)
    if active_files.iter().any(|f| f == path) {
        return 1;
    }

    // 2. Directory Prefix Match
    let path_parent = Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if !path_parent.is_empty() {
        for active_file in active_files {
            let active_parent = Path::new(active_file)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if !active_parent.is_empty() && path_parent == active_parent {
                return 2;
            }
        }
    }

    // 3. Include Directory Match
    if path.starts_with("include/") {
        return 3;
    }

    // 4. Default (lowest priority)
    4
}
