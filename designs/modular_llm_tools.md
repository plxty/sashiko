# Design Document: Modular & Reusable LLM Tool Framework

## 1. Objective

Refactor Sashiko's AI tool implementation (`src/worker/tools.rs`) into a modular, pluggable, and highly reusable architecture. 

The goals are:
1.  **Extract a Generic LLM Tool Library**: Define a reusable framework (`llm_tool`) that has zero dependencies on Sashiko's business logic, allowing it to be copied or published as a standalone crate for other Rust LLM projects.
2.  **Encapsulate Tools (SRP)**: Move each tool (e.g., `git_grep`, `git_read_files`) into its own dedicated file, making the codebase easier to navigate and maintain.
3.  **Adhere to the Open-Closed Principle**: Enable adding new tools (like `compiler_build` or `run_checkpatch`) simply by creating a new struct and registering it, without modifying any central dispatchers or enums.
4.  **Preserve Backward Compatibility**: Retain the exact public API of `ToolBox` so that the rest of the Sashiko codebase (`reviewer.rs`, `prompts.rs`, and `bin/review.rs`) requires zero modifications.
5.  **First-Class Domain**: Move the toolbox out of the `worker` module and elevate it to a top-level package `src/toolbox/` (`sashiko::toolbox`), aligning with its role as a key interface used by the `review` binary.

---

## 2. Proposed Architecture

### The Generic `llm_tool` Framework (`src/toolbox/framework.rs`)
We will introduce a decoupled submodule `crate::toolbox::framework` that defines the core abstractions for LLM function calling. It is generic over a context type `C`, which allows hosting projects to pass their own environment state to the tools.

```rust
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use async_trait::async_trait;

/// Represents a single tool that can be exposed to an LLM.
/// `C` is the project-specific context type passed to the tool during invocation.
#[async_trait]
pub trait LlmTool<C>: Send + Sync {
    /// The unique name of the tool (e.g., "git_grep").
    fn name(&self) -> &'static str;

    /// A detailed description used by the LLM to understand when to call it.
    fn description(&self) -> &'static str;

    /// The JSON Schema defining the parameters this tool accepts.
    fn parameters(&self) -> Value;

    /// Normalizes the arguments passed by the LLM (e.g., filling in default values)
    /// to maximize cache hits. The default implementation returns the arguments unmodified.
    fn normalize_args(&self, args: &Value) -> Value {
        args.clone()
    }

    /// Executes the tool with the given arguments and context.
    async fn call(&self, args: Value, context: &C) -> Result<Value>;
}

/// A registry that manages a set of LLM tools and handles dispatching.
pub struct ToolRegistry<C> {
    tools: HashMap<&'static str, Arc<dyn LlmTool<C>>>,
}

impl<C> ToolRegistry<C> {
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }

    pub fn register(&mut self, tool: impl LlmTool<C> + 'static) {
        self.tools.insert(tool.name(), Arc::new(tool));
    }

    pub fn declarations(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters(),
                })
            })
            .collect()
    }

    pub fn normalize_tool_args(&self, name: &str, args: &Value) -> Value {
        if let Some(tool) = self.tools.get(name) {
            tool.normalize_args(args)
        } else {
            args.clone()
        }
    }

    pub async fn call(&self, name: &str, args: Value, context: &C) -> Result<Value> {
        let tool = self.tools.get(name).ok_or_else(|| {
            anyhow::anyhow!("Tool not found in registry: {}", name)
        })?;
        tool.call(args, context).await
    }
}
```

---

## 3. Sashiko-Specific Integration

Sashiko will define its own tool context and implement `LlmTool<SashikoToolContext>` for all its Git tools.

### Sashiko Tool Context
Since tools are executed in parallel and their environment (like active files and virtual head) can be updated during a review session, we will wrap mutable fields in thread-safe `RwLock`s.

```rust
// src/toolbox/mod.rs
pub struct SashikoToolContext {
    pub worktree_path: std::path::PathBuf,
    pub prompts_path: Option<std::path::PathBuf>,
    pub active_patch_files: std::sync::RwLock<Vec<String>>,
    pub virtual_head: std::sync::RwLock<Option<String>>,
}

impl SashikoToolContext {
    pub fn virtualize_ref(&self, r: &str) -> String {
        let vhead_lock = self.virtual_head.read().unwrap();
        let Some(ref vhead) = *vhead_lock else {
            return r.to_string();
        };
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"(^|[^/])\bHEAD($|[~^:.@])").unwrap());
        re.replace_all(r, format!("${{1}}{}${{2}}", vhead))
            .into_owned()
    }
}
```

### Decoupled Tool Layout
We will split `src/worker/tools.rs` into the following files under `src/toolbox/`:

1.  `mod.rs`: Defines `ToolBox` (the public interface), `SashikoToolContext`, and initializes the `ToolRegistry` registering all Git tools.
2.  `framework.rs`: The generic reusable LLM tool engine.
3.  `utils.rs`: Contains common helpers like `validate_path`, `glob_to_regex`, and grep proximity formatting.
4.  Individual tool implementations:
    *   `git_read_files.rs`
    *   `git_blame.rs`
    *   `git_diff.rs`
    *   `git_show.rs`
    *   `git_log.rs`
    *   `git_ls.rs`
    *   `git_grep.rs`
    *   `git_find_files.rs`
    *   `read_prompt.rs`

---

## 4. Detailed File Implementations (Examples)

### Example: `src/toolbox/git_ls.rs`
```rust
use crate::toolbox::framework::LlmTool;
use crate::toolbox::SashikoToolContext;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::process::Command;
use async_trait::async_trait;

pub struct GitLsTool;

#[async_trait]
impl LlmTool<SashikoToolContext> for GitLsTool {
    fn name(&self) -> &'static str { "git_ls" }
    
    fn description(&self) -> &'static str {
        "List files in a directory at a specific Git revision."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "revision": { "type": "string", "description": "The Git commit SHA or reference to list from." },
                "path": { "type": "string", "description": "Relative path to the directory (e.g., '.' or 'src/')." }
            },
            "required": ["revision", "path"]
        })
    }

    async fn call(&self, args: Value, context: &SashikoToolContext) -> Result<Value> {
        let revision_raw = args["revision"].as_str().ok_or_else(|| anyhow!("Missing revision"))?;
        let revision_virt = context.virtualize_ref(revision_raw);
        let revision = revision_virt.as_str();
        let path_str = args["path"].as_str().ok_or_else(|| anyhow!("Missing path"))?;

        if revision.starts_with('-') || path_str.starts_with('-') {
            return Err(anyhow!("Invalid revision or path name"));
        }

        let tree_spec = if path_str.is_empty() || path_str == "." {
            revision.to_string()
        } else {
            format!("{}:{}", revision, path_str)
        };

        let mut cmd = Command::new("git");
        cmd.current_dir(&context.worktree_path).args(["ls-tree", &tree_spec]);

        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Ok(json!({ "error": format!("git ls-tree failed for {}: {}", tree_spec, stderr) }));
        }

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        let mut entries = Vec::new();
        for line in content.lines() {
            let split_tab: Vec<&str> = line.split('\t').collect();
            if split_tab.len() >= 2 {
                let filename = split_tab[1];
                let metadata: Vec<&str> = split_tab[0].split_whitespace().collect();
                if metadata.len() >= 2 {
                    let ty = match metadata[1] {
                        "tree" => "dir",
                        _ => "file",
                    };
                    entries.push(json!({ "name": filename, "type": ty }));
                }
            }
        }

        let total_entries = entries.len();
        let truncated = total_entries > 1000;
        if truncated {
            entries.truncate(1000);
        }

        Ok(json!({
            "entries": entries,
            "truncated": truncated,
            "total_entries": total_entries,
            "next_page_hint": if truncated {
                Some("Directory listing truncated to 1000 entries. Please call git_ls with a specific subdirectory path to see more files.".to_string())
            } else {
                None
            }
        }))
    }
}
```

---

## 5. Backward-Compatible `ToolBox` Wrapper (`src/toolbox/mod.rs`)

```rust
use crate::toolbox::framework::ToolRegistry;
use crate::ai::AiTool;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub mod framework;
pub mod utils;
pub mod git_read_files;
pub mod git_blame;
pub mod git_diff;
pub mod git_show;
pub mod git_log;
pub mod git_ls;
pub mod git_grep;
pub mod git_find_files;
pub mod read_prompt;

pub struct SashikoToolContext {
    pub worktree_path: PathBuf,
    pub prompts_path: Option<PathBuf>,
    pub active_patch_files: RwLock<Vec<String>>,
    pub virtual_head: RwLock<Option<String>>,
}

// Implement context helpers...

pub struct ToolBox {
    context: SashikoToolContext,
    registry: ToolRegistry<SashikoToolContext>,
    pub(crate) cache: RwLock<std::collections::HashMap<String, Value>>,
}

impl ToolBox {
    pub fn new(worktree_path: PathBuf, prompts_path: Option<PathBuf>) -> Self {
        let context = SashikoToolContext {
            worktree_path,
            prompts_path,
            active_patch_files: RwLock::new(Vec::new()),
            virtual_head: RwLock::new(None),
        };

        let mut registry = ToolRegistry::new();
        registry.register(git_read_files::GitReadFilesTool);
        registry.register(git_blame::GitBlameTool);
        registry.register(git_diff::GitDiffTool);
        registry.register(git_show::GitShowTool);
        registry.register(git_log::GitLogTool);
        registry.register(git_ls::GitLsTool);
        registry.register(git_grep::GitGrepTool);
        registry.register(git_find_files::GitFindFilesTool);
        
        if context.prompts_path.is_some() {
            registry.register(read_prompt::ReadPromptTool);
        }

        Self {
            context,
            registry,
            cache: RwLock::new(std::collections::HashMap::new()),
        }
    }

    pub fn set_virtual_head(&mut self, sha: String) {
        let mut vhead = self.context.virtual_head.write().unwrap();
        *vhead = Some(sha);
    }

    pub fn set_active_patch_files(&mut self, files: Vec<String>) {
        let mut active = self.context.active_patch_files.write().unwrap();
        *active = files;
    }

    pub fn get_worktree_path(&self) -> &Path {
        &self.context.worktree_path
    }

    pub fn get_declarations_generic(&self) -> Vec<AiTool> {
        self.registry
            .declarations()
            .into_iter()
            .map(|decl| AiTool {
                name: decl["name"].as_str().unwrap().to_string(),
                description: decl["description"].as_str().unwrap().to_string(),
                parameters: decl["parameters"].clone(),
            })
            .collect()
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let name_normalized = name.trim().to_lowercase();
        let should_cache = name_normalized != "todowrite";

        let normalized_args = self.registry.normalize_tool_args(&name_normalized, &args);

        let key = if should_cache {
            let k = format!("{}:{}", name_normalized, serde_json::to_string(&normalized_args)?);
            {
                let cache = self.cache.read().unwrap();
                if let Some(val) = cache.get(&k) {
                    return Ok(val.clone());
                }
            }
            Some(k)
        } else {
            None
        };

        let res = self.registry.call(&name_normalized, args, &self.context).await?;

        if let Some(k) = key {
            let mut cache = self.cache.write().unwrap();
            cache.insert(k, res.clone());
        }

        Ok(res)
    }
}
