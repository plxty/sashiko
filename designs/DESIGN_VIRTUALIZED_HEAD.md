# Design Document: Virtualized HEAD in Sashiko Toolbox

## 1. Introduction and Goal

Sashiko reviews patches by applying them to a temporary worktree. During the review process, the AI agent can call various Git tools (the "sashiko toolbox") to inspect the codebase, view diffs, read files, etc. These tools often receive `HEAD` or derivations of `HEAD` (like `HEAD~1`, `HEAD^`) as revision arguments.

Currently, these tools pass `HEAD` directly to the underlying Git commands. While the physical `HEAD` of the worktree typically points to the patch being reviewed, we want to decouple this dependency and virtualize `HEAD`.

The goal is to ensure that **no underlying git command executed by the toolbox ever receives the string `HEAD` (or its derivations like `HEAD~1`)**. Instead, they should operate with a **virtual HEAD** that points directly to the commit SHA of the patch currently being reviewed. The behavior must remain exactly the same, and the virtualization must be transparent to the AI agent.

## 2. Proposed Architecture

We will implement a translation layer within the `ToolBox` struct. When the toolbox is initialized for a review session, it will be configured with the commit SHA of the patch being reviewed (the "virtual HEAD").

Before executing any Git command, the toolbox will rewrite any revision arguments, replacing `HEAD` with the virtual HEAD SHA.

### 2.1. Virtual Head Tracking in `ToolBox`

We will add an optional `virtual_head` field to the `ToolBox` struct and a setter method to configure it.

```rust
pub struct ToolBox {
    worktree_path: PathBuf,
    prompts_path: Option<PathBuf>,
    active_patch_files: Vec<String>,
    virtual_head: Option<String>, // <-- New field
    pub(crate) cache: std::sync::RwLock<std::collections::HashMap<String, Value>>,
}

impl ToolBox {
    pub fn new(worktree_path: PathBuf, prompts_path: Option<PathBuf>) -> Self {
        Self {
            worktree_path,
            prompts_path,
            active_patch_files: Vec::new(),
            virtual_head: None, // <-- Initialize to None
            cache: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    pub fn set_virtual_head(&mut self, sha: String) {
        self.virtual_head = Some(sha);
    }
}
```

### 2.2. Translation Logic: `virtualize_ref`

We will implement a helper method `virtualize_ref` in `ToolBox` that replaces `HEAD` with the virtual HEAD SHA.

To do this safely, we must:
1. Only replace `HEAD` when it refers to the local `HEAD` ref, not remote refs like `origin/HEAD`.
2. Handle derivations like `HEAD~1`, `HEAD^`, and ranges like `baseline..HEAD`.
3. Avoid replacing `HEAD` if it is part of a filename or another word (e.g., `FOREHEAD`).

We will use the following regex-based replacement:
- Pattern: `(^|[^/])\bHEAD\b`
- Replacement: `${1}<virtual_head_sha>`

This matches `HEAD` at the start of a string or preceded by any character other than `/` (excluding remote refs like `origin/HEAD`), and ensures it is a full word boundary (preventing matches in `FOREHEAD` or `HEADLESS`).

```rust
    fn virtualize_ref(&self, r: &str) -> String {
        let Some(ref vhead) = self.virtual_head else {
            return r.to_string();
        };
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"(^|[^/])\bHEAD\b").unwrap());
        re.replace_all(r, format!("${{1}}{}", vhead)).into_owned()
    }
```

### 2.3. Tool Updates

We will update the following tools in `src/worker/tools.rs` to use `virtualize_ref` on their revision parameters before constructing and running the Git command:

1. **`read_files` (via `read_single_file`)**: Virtualize `revision`.
2. **`git_blame`**: Virtualize `revision`.
3. **`git_diff`**: Virtualize `base_revision` and `target_revision`.
4. **`git_show`**: Virtualize `object` (could be `HEAD` or `HEAD:path`).
5. **`git_log`**: Virtualize `range` (could be `baseline..HEAD`).
6. **`git_ls`**: Virtualize `revision`.
7. **`git_grep`**: Virtualize `revision`. Also ensure `format_git_grep_output` and output prefix stripping use the virtualized revision so formatting remains identical.
8. **`git_find_files` (via `find_files`)**: Virtualize `revision`.

### 2.4. Setting Virtual HEAD in `review.rs`

In `src/bin/review.rs`, where `ToolBox` is instantiated, we will resolve the virtual HEAD SHA and set it.

If a specific patch index is being reviewed (`args.review_patch_index` is `Some`), we use its SHA.
If we are reviewing all patches, we use the SHA of the first patch in the series (matching `Worker::run`'s default target commit resolution).

```rust
                        let mut tools = ToolBox::new(worktree.path.clone(), prompts_tool_path);
                        tools.set_active_patch_files(patch_files);
                        
                        // Resolve and set virtual HEAD
                        let virtual_head = if let Some(idx) = args.review_patch_index {
                            patch_shas.get(&idx).cloned()
                        } else {
                            patches_to_review.first().and_then(|p| patch_shas.get(&p.index).cloned())
                        };
                        if let Some(sha) = virtual_head {
                            info!("Setting virtual HEAD to {}", sha);
                            tools.set_virtual_head(sha);
                        }
```

## 3. Verification Plan

### 3.1. Unit Tests

We will add unit tests in `src/worker/tools_test.rs` (or inside `tools.rs` tests) to verify:
1. `virtualize_ref` correctly translates:
   - `"HEAD"` -> `"<SHA>"`
   - `"HEAD~1"` -> `"<SHA>~1"`
   - `"HEAD^"` -> `"<SHA>^"`
   - `"baseline..HEAD"` -> `"baseline..<SHA>"`
   - `"HEAD:file.c"` -> `"<SHA>:file.c"`
2. `virtualize_ref` does NOT translate:
   - `"origin/HEAD"` -> `"origin/HEAD"`
   - `"refs/remotes/origin/HEAD"` -> `"refs/remotes/origin/HEAD"`
   - `"FOREHEAD"` -> `"FOREHEAD"`
3. Toolbox tools actually work with virtualized HEAD and produce identical output to non-virtualized calls.

### 3.2. CI/CD Checks

We will run the following checks to ensure no regressions:
- `make lint` to ensure clean code formatting and clippy compliance.
- `make test` to ensure all unit tests pass.
- `make check-pr` to run full pre-PR validation.
