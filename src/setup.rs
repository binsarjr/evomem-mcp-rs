use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use serde_json::{json, Map, Value as JsonValue};
use toml_edit::{value, Array, DocumentMut, InlineTable, Item, Value as TomlValue};

const SKILL: &str = include_str!("../integrations/agent-memory/evomem-memory/SKILL.md");
const OPENAI_YAML: &str =
    include_str!("../integrations/agent-memory/evomem-memory/agents/openai.yaml");
const LEGACY_HOOK: &str = r#"#!/usr/bin/env python3
"""Inject an LLM-driven Evomem checkpoint after context compaction."""

import json
import sys


def main() -> None:
    try:
        event = json.load(sys.stdin)
    except (json.JSONDecodeError, OSError):
        return

    if event.get("hook_event_name") != "SessionStart" or event.get("source") != "compact":
        return

    context = (
        "Run the evomem-memory compaction checkpoint now. Let the LLM inspect "
        "the compacted context, call memory_recall for the active project/task "
        "and each candidate fact, and call memory_remember only for verified, "
        "durable facts that are new or changed. Never save the raw summary, "
        "transcript, secrets, hypotheses, or transient progress. If nothing is "
        "new, write nothing. Recall the active task state again, then continue it."
    )
    print(
        json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": context,
                }
            }
        )
    )


if __name__ == "__main__":
    main()
"#;
const START: &str = "<!-- evomem-mcp-rs:start -->";
const END: &str = "<!-- evomem-mcp-rs:end -->";
const TRIGGER: &str = r#"<!-- evomem-mcp-rs:start -->
## Evomem long-term memory

Use the `evomem-memory` skill for non-trivial work that may overlap prior
sessions. Recall before asking the user to repeat context, and checkpoint new
durable facts at verified milestones. Never save secrets or raw transcripts.
<!-- evomem-mcp-rs:end -->"#;
const CHECKPOINT_CONTEXT: &str = "Run the evomem-memory compaction checkpoint now. Let the LLM inspect the compacted context, call memory_recall for the active project/task and each candidate fact, and call memory_remember only for verified, durable facts that are new or changed. Never save the raw summary, transcript, secrets, hypotheses, or transient progress. If nothing is new, write nothing. Recall the active task state again, then continue it.";

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Client {
    All,
    Codex,
    ClaudeCode,
}

impl Client {
    fn codex(self) -> bool {
        matches!(self, Self::All | Self::Codex)
    }

    fn claude(self) -> bool {
        matches!(self, Self::All | Self::ClaudeCode)
    }
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Existing project directory to configure.
    #[arg(long, default_value = ".")]
    project: PathBuf,
    /// Evomem Streamable HTTP MCP endpoint.
    #[arg(long)]
    url: String,
    /// Project memory namespace (a-z, 0-9, '-' and '_' only).
    #[arg(long)]
    namespace: String,
    /// Client configuration to install.
    #[arg(long, value_enum, default_value = "all")]
    client: Client,
    /// Show changes without writing files.
    #[arg(long)]
    dry_run: bool,
}

enum Change {
    Write { path: PathBuf, bytes: Vec<u8> },
    Remove { path: PathBuf },
    Symlink { path: PathBuf, target: PathBuf },
}

impl Change {
    fn path(&self) -> &Path {
        match self {
            Self::Write { path, .. } | Self::Remove { path } | Self::Symlink { path, .. } => path,
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Self::Write { path, .. } if path.exists() => "update",
            Self::Write { .. } | Self::Symlink { .. } => "create",
            Self::Remove { .. } => "remove",
        }
    }
}

pub fn run(args: SetupArgs) -> Result<()> {
    let project = fs::canonicalize(&args.project)
        .with_context(|| format!("project does not exist: {}", args.project.display()))?;
    if !project.is_dir() {
        bail!("project is not a directory: {}", project.display());
    }
    if !(args.url.starts_with("http://") || args.url.starts_with("https://")) {
        bail!("URL must start with http:// or https://");
    }
    let namespace =
        super::normalize_namespace(Some(&args.namespace)).map_err(anyhow::Error::msg)?;
    let executable = std::env::current_exe().context("cannot locate evomem-mcp-rs executable")?;
    let changes = plan(&project, &args.url, &namespace, args.client, &executable)?;

    println!(
        "Evomem setup: {} [{}]",
        project.display(),
        if args.dry_run { "dry run" } else { "apply" }
    );
    if changes.is_empty() {
        println!("unchanged  project already configured");
    } else {
        for change in &changes {
            println!("{:<9} {}", change.verb(), change.path().display());
        }
        if !args.dry_run {
            for change in changes {
                apply(change)?;
            }
        }
    }

    if !args.dry_run {
        println!("done       restart the client and trust this project's MCP server and hook when prompted");
        println!("rollback   restore any *.evomem.bak files, then remove newly created Evomem entries/files");
    }
    Ok(())
}

fn plan(
    project: &Path,
    url: &str,
    namespace: &str,
    client: Client,
    executable: &Path,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    let command = format!("{} hook compact", shell_quote(executable));

    if client.codex() {
        plan_text(&mut changes, project.join(".codex/config.toml"), |old| {
            merge_codex_config(old, url, namespace)
        })?;
        plan_json(&mut changes, project.join(".codex/hooks.json"), |root| {
            merge_hook(root, &command, true)
        })?;
        plan_markdown(&mut changes, project.join("AGENTS.md"))?;

        if !project.join(".git").exists() {
            let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
            plan_text(
                &mut changes,
                PathBuf::from(home).join(".codex/config.toml"),
                merge_project_markers,
            )?;
        }
    }

    if client.claude() {
        plan_json(&mut changes, project.join(".mcp.json"), |root| {
            merge_claude_mcp(root, url, namespace)
        })?;
        plan_json(
            &mut changes,
            project.join(".claude/settings.local.json"),
            |root| merge_claude_settings(root, &command),
        )?;
        plan_markdown(&mut changes, project.join("CLAUDE.md"))?;
    }

    plan_skill(&mut changes, project, client.claude())?;
    Ok(changes)
}

fn plan_text<F>(changes: &mut Vec<Change>, path: PathBuf, merge: F) -> Result<()>
where
    F: FnOnce(&str) -> Result<String>,
{
    let old = read_text(&path)?;
    let new = merge(&old).with_context(|| format!("invalid config: {}", path.display()))?;
    if old.as_bytes() != new.as_bytes() {
        changes.push(Change::Write {
            path,
            bytes: new.into_bytes(),
        });
    }
    Ok(())
}

fn plan_json<F>(changes: &mut Vec<Change>, path: PathBuf, merge: F) -> Result<()>
where
    F: FnOnce(&mut JsonValue) -> Result<()>,
{
    let old = read_text(&path)?;
    let mut root = if old.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&old).with_context(|| format!("invalid JSON: {}", path.display()))?
    };
    merge(&mut root)?;
    let mut new = serde_json::to_string_pretty(&root)?;
    new.push('\n');
    if old.as_bytes() != new.as_bytes() {
        changes.push(Change::Write {
            path,
            bytes: new.into_bytes(),
        });
    }
    Ok(())
}

fn plan_markdown(changes: &mut Vec<Change>, path: PathBuf) -> Result<()> {
    let old = read_text(&path)?;
    let new = merge_markdown(&old)?;
    if old != new {
        changes.push(Change::Write {
            path,
            bytes: new.into_bytes(),
        });
    }
    Ok(())
}

fn read_text(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn root_object(root: &mut JsonValue) -> Result<&mut Map<String, JsonValue>> {
    root.as_object_mut()
        .ok_or_else(|| anyhow!("top-level JSON value must be an object"))
}

fn object_entry<'a>(
    object: &'a mut Map<String, JsonValue>,
    key: &str,
) -> Result<&'a mut Map<String, JsonValue>> {
    object
        .entry(key.to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("'{key}' must be an object"))
}

fn array_entry<'a>(
    object: &'a mut Map<String, JsonValue>,
    key: &str,
) -> Result<&'a mut Vec<JsonValue>> {
    object
        .entry(key.to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow!("'{key}' must be an array"))
}

fn merge_claude_mcp(root: &mut JsonValue, url: &str, namespace: &str) -> Result<()> {
    let servers = object_entry(root_object(root)?, "mcpServers")?;
    let server = object_entry(servers, "evomem")?;
    server.insert("type".into(), json!("http"));
    server.insert("url".into(), json!(url));
    server.insert("alwaysLoad".into(), json!(true));
    let headers = object_entry(server, "headers")?;
    headers.insert("X-Evomem-Namespace".into(), json!(namespace));
    Ok(())
}

fn merge_hook(root: &mut JsonValue, command: &str, codex: bool) -> Result<()> {
    let hooks = object_entry(root_object(root)?, "hooks")?;
    let sessions = array_entry(hooks, "SessionStart")?;
    let mut found = false;

    for group in sessions.iter_mut() {
        let Some(group_object) = group.as_object_mut() else {
            continue;
        };
        let Some(commands) = group_object
            .get_mut("hooks")
            .and_then(JsonValue::as_array_mut)
        else {
            continue;
        };
        for hook in commands.iter_mut() {
            let Some(hook_object) = hook.as_object_mut() else {
                continue;
            };
            let existing = hook_object
                .get("command")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            if existing.contains("compact_checkpoint.py") || existing.contains(" hook compact") {
                hook_object.insert("type".into(), json!("command"));
                hook_object.insert("command".into(), json!(command));
                hook_object.insert("timeout".into(), json!(5));
                if codex {
                    hook_object.insert(
                        "statusMessage".into(),
                        json!("Checkpointing durable memory"),
                    );
                }
                group_object.insert(
                    "matcher".into(),
                    json!(if codex { "^compact$" } else { "compact" }),
                );
                found = true;
                break;
            }
        }
        if found {
            break;
        }
    }

    if !found {
        let mut hook = json!({ "type": "command", "command": command, "timeout": 5 });
        if codex {
            hook["statusMessage"] = json!("Checkpointing durable memory");
        }
        sessions.push(json!({
            "matcher": if codex { "^compact$" } else { "compact" },
            "hooks": [hook]
        }));
    }
    Ok(())
}

fn merge_claude_settings(root: &mut JsonValue, command: &str) -> Result<()> {
    merge_hook(root, command, false)?;
    let object = root_object(root)?;
    let permissions = object_entry(object, "permissions")?;
    let allow = array_entry(permissions, "allow")?;
    for permission in ["mcp__evomem__memory_recall", "mcp__evomem__memory_remember"] {
        if !allow.iter().any(|value| value.as_str() == Some(permission)) {
            allow.push(json!(permission));
        }
    }
    let enabled = array_entry(object, "enabledMcpjsonServers")?;
    if !enabled.iter().any(|value| value.as_str() == Some("evomem")) {
        enabled.push(json!("evomem"));
    }
    Ok(())
}

fn merge_codex_config(old: &str, url: &str, namespace: &str) -> Result<String> {
    let mut doc = parse_toml(old)?;
    doc["mcp_servers"]["evomem"]["url"] = value(url);
    merge_toml_header(&mut doc["mcp_servers"]["evomem"]["http_headers"], namespace)?;
    doc["mcp_servers"]["evomem"]["tools"]["memory_recall"]["approval_mode"] = value("approve");
    doc["mcp_servers"]["evomem"]["tools"]["memory_remember"]["approval_mode"] = value("approve");
    Ok(ensure_newline(doc.to_string()))
}

fn merge_toml_header(item: &mut Item, namespace: &str) -> Result<()> {
    if item.is_none() {
        *item = Item::Value(TomlValue::InlineTable(InlineTable::new()));
    }
    if let Some(table) = item.as_inline_table_mut() {
        table.insert("X-Evomem-Namespace", TomlValue::from(namespace));
        return Ok(());
    }
    if let Some(table) = item.as_table_mut() {
        table["X-Evomem-Namespace"] = value(namespace);
        return Ok(());
    }
    bail!("mcp_servers.evomem.http_headers must be a table");
}

fn merge_project_markers(old: &str) -> Result<String> {
    let mut doc = parse_toml(old)?;
    let item = &mut doc["project_root_markers"];
    if item.is_none() {
        let mut array = Array::new();
        array.push(".git");
        array.push(".codex");
        *item = value(array);
    } else {
        let array = item
            .as_array_mut()
            .ok_or_else(|| anyhow!("project_root_markers must be an array"))?;
        if !array.iter().any(|value| value.as_str() == Some(".codex")) {
            array.push(".codex");
        }
    }
    Ok(ensure_newline(doc.to_string()))
}

fn parse_toml(old: &str) -> Result<DocumentMut> {
    if old.trim().is_empty() {
        Ok(DocumentMut::new())
    } else {
        old.parse().map_err(anyhow::Error::msg)
    }
}

fn ensure_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn merge_markdown(old: &str) -> Result<String> {
    if let Some(start) = old.find(START) {
        let tail = &old[start..];
        let relative_end = tail
            .find(END)
            .ok_or_else(|| anyhow!("Evomem managed block has no end marker"))?;
        let end = start + relative_end + END.len();
        let mut new = old.to_string();
        new.replace_range(start..end, TRIGGER);
        return Ok(ensure_newline(new));
    }

    let mut base = old.to_string();
    if let Some(start) = base.find("## Evomem long-term memory") {
        let end = base[start + 3..]
            .find("\n## ")
            .map(|relative| start + 3 + relative + 1)
            .unwrap_or(base.len());
        base.replace_range(start..end, "");
        base = base.trim_end().to_string();
    }
    if !base.is_empty() {
        base.push_str("\n\n");
    }
    base.push_str(TRIGGER);
    base.push('\n');
    Ok(base)
}

fn plan_skill(changes: &mut Vec<Change>, project: &Path, claude: bool) -> Result<()> {
    let skill = project.join(".agents/skills/evomem-memory");
    if skill.exists() {
        for entry in walk_files(&skill)? {
            let relative = entry.strip_prefix(&skill)?;
            let allowed = relative == Path::new("SKILL.md")
                || relative == Path::new("agents/openai.yaml")
                || relative == Path::new("scripts/compact_checkpoint.py")
                || relative == Path::new("SKILL.md.evomem.bak")
                || relative == Path::new("agents/openai.yaml.evomem.bak")
                || relative == Path::new("scripts/compact_checkpoint.py.evomem.bak");
            if !allowed {
                bail!(
                    "refusing to overwrite unknown skill file: {}",
                    entry.display()
                );
            }
        }
    }
    plan_bytes(changes, skill.join("SKILL.md"), SKILL.as_bytes())?;
    plan_bytes(
        changes,
        skill.join("agents/openai.yaml"),
        OPENAI_YAML.as_bytes(),
    )?;

    let legacy = skill.join("scripts/compact_checkpoint.py");
    if legacy.exists() {
        let existing = fs::read(&legacy)?;
        if existing != LEGACY_HOOK.as_bytes() {
            bail!(
                "refusing to remove modified legacy hook: {}",
                legacy.display()
            );
        }
        changes.push(Change::Remove { path: legacy });
    }

    if claude {
        let link = project.join(".claude/skills/evomem-memory");
        if fs::symlink_metadata(&link).is_ok() {
            let metadata = fs::symlink_metadata(&link)?;
            if !metadata.file_type().is_symlink()
                || fs::read_link(&link)? != Path::new("../../.agents/skills/evomem-memory")
            {
                bail!(
                    "refusing to replace existing Claude skill: {}",
                    link.display()
                );
            }
        } else {
            changes.push(Change::Symlink {
                path: link,
                target: PathBuf::from("../../.agents/skills/evomem-memory"),
            });
        }
    }
    Ok(())
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

fn plan_bytes(changes: &mut Vec<Change>, path: PathBuf, desired: &[u8]) -> Result<()> {
    match fs::read(&path) {
        Ok(existing) if existing == desired => {}
        Ok(_) => changes.push(Change::Write {
            path,
            bytes: desired.to_vec(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => changes.push(Change::Write {
            path,
            bytes: desired.to_vec(),
        }),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn backup(path: &Path) -> Result<()> {
    if path.exists() {
        let backup = PathBuf::from(format!("{}.evomem.bak", path.display()));
        fs::copy(path, &backup).with_context(|| format!("cannot back up {}", path.display()))?;
    }
    Ok(())
}

fn apply(change: Change) -> Result<()> {
    match change {
        Change::Write { path, bytes } => {
            backup(&path)?;
            let parent = path
                .parent()
                .ok_or_else(|| anyhow!("invalid path: {}", path.display()))?;
            fs::create_dir_all(parent)?;
            let temporary = parent.join(format!(".evomem.tmp.{}", std::process::id()));
            fs::write(&temporary, bytes)?;
            fs::rename(&temporary, &path)?;
        }
        Change::Remove { path } => {
            backup(&path)?;
            fs::remove_file(path)?;
        }
        Change::Symlink { path, target } => {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow!("invalid path: {}", path.display()))?;
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, path)?;
            #[cfg(not(unix))]
            bail!("Claude skill symlinks are currently supported on Unix only");
        }
    }
    Ok(())
}

pub fn run_compact_hook() -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    if let Some(output) = compact_hook_output(&input) {
        println!("{output}");
    }
    Ok(())
}

fn compact_hook_output(input: &str) -> Option<String> {
    let event: JsonValue = serde_json::from_str(input).ok()?;
    if event.get("hook_event_name")?.as_str()? != "SessionStart"
        || event.get("source")?.as_str()? != "compact"
    {
        return None;
    }
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": CHECKPOINT_CONTEXT
            }
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> PathBuf {
        let path = std::env::temp_dir().join(format!("evomem-setup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join(".git")).unwrap();
        path
    }

    #[test]
    fn merges_without_duplicates() {
        let project = temp_project();
        fs::write(project.join("AGENTS.md"), "# Existing\n").unwrap();
        let legacy = project.join(".agents/skills/evomem-memory/scripts/compact_checkpoint.py");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, LEGACY_HOOK).unwrap();
        let first = plan(
            &project,
            "http://localhost:8080/mcp",
            "demo",
            Client::All,
            Path::new("/opt/evomem-mcp-rs"),
        )
        .unwrap();
        for change in first {
            apply(change).unwrap();
        }
        let snapshot = fs::read(project.join(".codex/config.toml")).unwrap();
        let second = plan(
            &project,
            "http://localhost:8080/mcp",
            "demo",
            Client::All,
            Path::new("/opt/evomem-mcp-rs"),
        )
        .unwrap();
        assert!(second.is_empty());
        assert_eq!(
            snapshot,
            fs::read(project.join(".codex/config.toml")).unwrap()
        );
        assert!(!legacy.exists());
        assert!(PathBuf::from(format!("{}.evomem.bak", legacy.display())).exists());
        assert!(fs::read_to_string(project.join("AGENTS.md"))
            .unwrap()
            .starts_with("# Existing"));
        let _ = fs::remove_dir_all(project);
    }

    #[test]
    fn hook_only_emits_for_compaction() {
        let output =
            compact_hook_output(r#"{"hook_event_name":"SessionStart","source":"compact"}"#)
                .unwrap();
        assert!(output.contains("additionalContext"));
        assert!(
            compact_hook_output(r#"{"hook_event_name":"SessionStart","source":"startup"}"#)
                .is_none()
        );
        assert!(compact_hook_output("not json").is_none());
    }

    #[test]
    fn invalid_managed_block_fails_closed() {
        assert!(merge_markdown("<!-- evomem-mcp-rs:start -->").is_err());
    }
}
