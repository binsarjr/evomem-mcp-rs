use std::ffi::OsStr;
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
const OPENCODE_COMPACT_CONTEXT: &str = "Preserve this continuation action after compaction: before continuing the task, activate the evomem-memory skill, recall the active project/task, and checkpoint only verified durable facts that are new or changed. Never save raw summaries, transcripts, secrets, hypotheses, or transient progress.";

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Client {
    All,
    Codex,
    ClaudeCode,
    Opencode,
    GeminiCli,
    Cursor,
    RooCode,
}

impl Client {
    const SUPPORTED: [Self; 6] = [
        Self::Codex,
        Self::ClaudeCode,
        Self::Opencode,
        Self::GeminiCli,
        Self::Cursor,
        Self::RooCode,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::Opencode => "opencode",
            Self::GeminiCli => "gemini-cli",
            Self::Cursor => "cursor",
            Self::RooCode => "roo-code",
        }
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
    let clients = select_clients(args.client, &project)?;
    let changes = plan(&project, &args.url, &namespace, &clients, &executable)?;

    println!(
        "Evomem setup: {} [{}]\nclients    {}",
        project.display(),
        if args.dry_run { "dry run" } else { "apply" },
        clients
            .iter()
            .map(|client| client.label())
            .collect::<Vec<_>>()
            .join(", ")
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
    clients: &[Client],
    executable: &Path,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    let command = format!("{} hook compact", shell_quote(executable));
    let selected = |client| clients.contains(&client);

    if selected(Client::Codex) {
        plan_text(&mut changes, project.join(".codex/config.toml"), |old| {
            merge_codex_config(old, url, namespace)
        })?;
        plan_json(&mut changes, project.join(".codex/hooks.json"), |root| {
            merge_hook(root, &command, true)
        })?;

        if !project.join(".git").exists() {
            let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
            plan_text(
                &mut changes,
                PathBuf::from(home).join(".codex/config.toml"),
                merge_project_markers,
            )?;
        }
    }

    if selected(Client::ClaudeCode) {
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

    if selected(Client::Opencode) {
        plan_json(&mut changes, project.join("opencode.json"), |root| {
            merge_opencode_config(root, url, namespace)
        })?;
        plan_bytes(
            &mut changes,
            project.join(".opencode/plugins/evomem-memory.js"),
            opencode_plugin().as_bytes(),
        )?;
    }

    if selected(Client::GeminiCli) {
        plan_json(
            &mut changes,
            project.join(".gemini/settings.json"),
            |root| merge_gemini_config(root, url, namespace),
        )?;
        plan_markdown(&mut changes, project.join("GEMINI.md"))?;
    }

    if selected(Client::Cursor) {
        plan_json(&mut changes, project.join(".cursor/mcp.json"), |root| {
            merge_cursor_config(root, url, namespace)
        })?;
    }

    if selected(Client::RooCode) {
        plan_json(&mut changes, project.join(".roo/mcp.json"), |root| {
            merge_roo_config(root, url, namespace)
        })?;
    }

    if clients.iter().any(|client| {
        matches!(
            client,
            Client::Codex | Client::Opencode | Client::Cursor | Client::RooCode
        )
    }) {
        plan_markdown(&mut changes, project.join("AGENTS.md"))?;
    }

    plan_skill(&mut changes, project, selected(Client::ClaudeCode))?;
    Ok(changes)
}

fn select_clients(requested: Client, project: &Path) -> Result<Vec<Client>> {
    if requested != Client::All {
        return Ok(vec![requested]);
    }
    let path = std::env::var_os("PATH");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let clients = Client::SUPPORTED
        .into_iter()
        .filter(|client| client_detected(*client, project, path.as_deref(), home.as_deref()))
        .collect::<Vec<_>>();
    if clients.is_empty() {
        bail!("no supported clients detected; use --client <name> to configure one explicitly");
    }
    Ok(clients)
}

fn client_detected(
    client: Client,
    project: &Path,
    path: Option<&OsStr>,
    home: Option<&Path>,
) -> bool {
    let commands = match client {
        Client::Codex => &["codex"][..],
        Client::ClaudeCode => &["claude"][..],
        Client::Opencode => &["opencode"][..],
        Client::GeminiCli => &["gemini"][..],
        Client::Cursor => &["cursor", "cursor-agent"][..],
        Client::RooCode => &["roo"][..],
        Client::All => return false,
    };
    let marker = match client {
        Client::Codex => project.join(".codex").exists(),
        Client::ClaudeCode => project.join(".claude").exists(),
        Client::Opencode => {
            project.join(".opencode").exists() || project.join("opencode.json").exists()
        }
        Client::GeminiCli => project.join(".gemini").exists(),
        Client::Cursor => project.join(".cursor").exists(),
        Client::RooCode => project.join(".roo").exists(),
        Client::All => false,
    };
    marker
        || command_exists(path, commands)
        || (client == Client::Cursor && Path::new("/Applications/Cursor.app").exists())
        || (client == Client::RooCode && roo_extension_exists(home))
}

fn command_exists(path: Option<&OsStr>, commands: &[&str]) -> bool {
    path.into_iter()
        .flat_map(std::env::split_paths)
        .any(|directory| {
            commands
                .iter()
                .any(|command| directory.join(command).is_file())
        })
}

fn roo_extension_exists(home: Option<&Path>) -> bool {
    let Some(home) = home else { return false };
    [
        ".vscode/extensions",
        ".vscode-insiders/extensions",
        ".cursor/extensions",
        ".windsurf/extensions",
    ]
    .into_iter()
    .filter_map(|directory| fs::read_dir(home.join(directory)).ok())
    .flatten()
    .filter_map(Result::ok)
    .any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("rooveterinaryinc.roo-cline-")
    })
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

fn merge_opencode_config(root: &mut JsonValue, url: &str, namespace: &str) -> Result<()> {
    let object = root_object(root)?;
    let servers = object_entry(object, "mcp")?;
    let server = object_entry(servers, "evomem")?;
    server.insert("type".into(), json!("remote"));
    server.insert("url".into(), json!(url));
    server.insert("enabled".into(), json!(true));
    let headers = object_entry(server, "headers")?;
    headers.insert("X-Evomem-Namespace".into(), json!(namespace));

    let permissions = object_entry(object, "permission")?;
    permissions.insert("evomem_memory_recall".into(), json!("allow"));
    permissions.insert("evomem_memory_remember".into(), json!("allow"));
    permissions.insert("evomem_memory_forget".into(), json!("ask"));
    Ok(())
}

fn merge_gemini_config(root: &mut JsonValue, url: &str, namespace: &str) -> Result<()> {
    let servers = object_entry(root_object(root)?, "mcpServers")?;
    let server = object_entry(servers, "evomem")?;
    server.insert("httpUrl".into(), json!(url));
    server.insert("trust".into(), json!(false));
    let headers = object_entry(server, "headers")?;
    headers.insert("X-Evomem-Namespace".into(), json!(namespace));
    Ok(())
}

fn merge_cursor_config(root: &mut JsonValue, url: &str, namespace: &str) -> Result<()> {
    let servers = object_entry(root_object(root)?, "mcpServers")?;
    let server = object_entry(servers, "evomem")?;
    server.insert("url".into(), json!(url));
    let headers = object_entry(server, "headers")?;
    headers.insert("X-Evomem-Namespace".into(), json!(namespace));
    Ok(())
}

fn merge_roo_config(root: &mut JsonValue, url: &str, namespace: &str) -> Result<()> {
    let servers = object_entry(root_object(root)?, "mcpServers")?;
    let server = object_entry(servers, "evomem")?;
    server.insert("type".into(), json!("streamable-http"));
    server.insert("url".into(), json!(url));
    server.insert("disabled".into(), json!(false));
    let headers = object_entry(server, "headers")?;
    headers.insert("X-Evomem-Namespace".into(), json!(namespace));
    let always_allow = array_entry(server, "alwaysAllow")?;
    always_allow.retain(|tool| tool.as_str() != Some("memory_forget"));
    for tool in ["memory_recall", "memory_remember"] {
        if !always_allow
            .iter()
            .any(|value| value.as_str() == Some(tool))
        {
            always_allow.push(json!(tool));
        }
    }
    Ok(())
}

fn opencode_plugin() -> String {
    format!(
        "export const EvomemMemoryPlugin = async () => ({{\n  \"experimental.session.compacting\": async (_input, output) => {{\n    output.context.push({});\n  }},\n}});\n",
        serde_json::to_string(OPENCODE_COMPACT_CONTEXT).expect("static string is valid JSON")
    )
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    fn temp_project() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "evomem-setup-test-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
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
            &Client::SUPPORTED,
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
            &Client::SUPPORTED,
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
        assert!(fs::read_to_string(project.join("opencode.json"))
            .unwrap()
            .contains("evomem_memory_forget"));
        assert!(fs::read_to_string(project.join(".gemini/settings.json"))
            .unwrap()
            .contains("\"trust\": false"));
        assert!(fs::read_to_string(project.join(".cursor/mcp.json"))
            .unwrap()
            .contains("X-Evomem-Namespace"));
        assert!(fs::read_to_string(project.join(".roo/mcp.json"))
            .unwrap()
            .contains("streamable-http"));
        assert!(fs::read_to_string(project.join("GEMINI.md"))
            .unwrap()
            .contains("Evomem long-term memory"));
        let _ = fs::remove_dir_all(project);
    }

    #[test]
    fn detects_project_markers_and_path_commands() {
        let project = temp_project();
        fs::create_dir_all(project.join(".roo")).unwrap();
        let bin = project.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("opencode"), "").unwrap();
        let path = std::env::join_paths([&bin]).unwrap();

        assert!(client_detected(
            Client::RooCode,
            &project,
            Some(path.as_os_str()),
            None
        ));
        assert!(client_detected(
            Client::Opencode,
            &project,
            Some(path.as_os_str()),
            None
        ));
        assert!(!client_detected(
            Client::GeminiCli,
            &project,
            Some(path.as_os_str()),
            None
        ));
        let _ = fs::remove_dir_all(project);
    }

    #[test]
    fn opencode_plugin_is_plain_javascript() {
        let plugin = opencode_plugin();
        assert!(plugin.contains("\"experimental.session.compacting\""));
        assert!(!plugin.contains("\\\\\"experimental.session.compacting"));
        assert!(plugin.contains("output.context.push"));
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
