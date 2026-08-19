//! A stdio proxy in front of luau-lsp.
//!
//! It relays every LSP message untouched, with one exception: in
//! `textDocument/completion` responses it drops auto-import suggestions whose
//! module belongs to a context incompatible with the file that asked for the
//! completion.
//!
//! luau-lsp already does this filtering, but only against hardcoded Roblox
//! service names, and only for instance requires. It knows nothing of a
//! project's own folder conventions, nor of string requires.

use luau_lsp_boundary::{
    contains, filter_completion, newest_extension, read_message, uri_to_path, write_message,
    Pending, Workspace,
};
use serde_json::Value;
use std::io::{self, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// Prefix reserved for the proxy: these arguments are never forwarded to
/// luau-lsp, which would reject them.
const OWN_ARG_PREFIX: &str = "--boundary-";

/// The only LSP method this proxy acts on.
const COMPLETION_METHOD: &[u8] = b"textDocument/completion";

struct Log {
    enabled: bool,
}

impl Log {
    fn say(&self, message: impl AsRef<str>) {
        if self.enabled {
            // stderr is inherited as-is: in VS Code these lines land in the
            // "Luau Language Server" output channel.
            eprintln!("[boundary] {}", message.as_ref());
        }
    }
}

/// A JSON-RPC response carries an id and no `method`; a request carries both.
fn is_response(message: &Value) -> bool {
    message.get("id").is_some() && message.get("method").is_none()
}

fn id_key(id: &Value) -> Option<String> {
    match id {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// The roots the editor announced, `workspaceFolders` first.
fn roots_from_initialize(message: &Value) -> Vec<String> {
    let params = message.get("params");

    let folders = params
        .and_then(|p| p.get("workspaceFolders"))
        .and_then(Value::as_array)
        .map(|folders| {
            folders
                .iter()
                .filter_map(|f| f.get("uri").and_then(Value::as_str))
                .map(uri_to_path)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if !folders.is_empty() {
        return folders;
    }

    params
        .and_then(|p| p.get("rootUri"))
        .and_then(Value::as_str)
        .map(uri_to_path)
        .into_iter()
        .collect()
}

/// The server bundled with the VS Code extension, as a last resort.
///
/// This is the common case rather than the rare one: most VS Code users never
/// install luau-lsp separately and so have none on their PATH. Without this
/// fallback the proxy would find nothing at all for them.
fn bundled_extension_servers(home: &str) -> Vec<String> {
    let roots = [
        ".vscode/extensions",
        ".vscode-insiders/extensions",
        ".vscode-server/extensions",
        ".cursor/extensions",
        ".windsurf/extensions",
    ];

    let mut found = Vec::new();
    for root in roots {
        let root = format!("{home}/{root}");
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        let Some(newest) = newest_extension(names.iter().map(String::as_str)) else {
            continue;
        };
        for binary in ["bin/server.exe", "bin/server"] {
            let path = format!("{root}/{newest}/{binary}");
            if std::path::Path::new(&path).exists() {
                found.push(path);
                break;
            }
        }
    }
    found
}

/// Launches luau-lsp, trying the plausible locations in order.
///
/// `cwd` is the workspace root, which is what lets a per-project version
/// manager (rokit, aftman) resolve the version pinned in that repo's manifest.
/// The editor itself spawns the proxy from an arbitrary directory.
fn spawn_server(args: &[String], cwd: Option<&String>, log: &Log) -> Option<Child> {
    let mut candidates = Vec::new();
    if let Ok(explicit) = std::env::var("LUAU_LSP_BOUNDARY_SERVER") {
        candidates.push(explicit);
    } else {
        candidates.push("luau-lsp".to_string());
        if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            candidates.push(format!("{home}/.rokit/bin/luau-lsp.exe"));
            candidates.push(format!("{home}/.rokit/bin/luau-lsp"));
            candidates.push(format!("{home}/.aftman/bin/luau-lsp.exe"));
            candidates.push(format!("{home}/.aftman/bin/luau-lsp"));
            candidates.extend(bundled_extension_servers(&home));
        }
    }

    for candidate in &candidates {
        let mut command = Command::new(candidate);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        match command.spawn() {
            Ok(child) => {
                log.say(format!("server: {candidate}"));
                return Some(child);
            }
            Err(error) => log.say(format!("server `{candidate}` unreachable: {error}")),
        }
    }

    eprintln!("[boundary] no luau-lsp found among {candidates:?}");
    None
}

fn main() {
    let all_args: Vec<String> = std::env::args().skip(1).collect();
    let own: Vec<&String> = all_args
        .iter()
        .filter(|a| a.starts_with(OWN_ARG_PREFIX))
        .collect();
    let forwarded: Vec<String> = all_args
        .iter()
        .filter(|a| !a.starts_with(OWN_ARG_PREFIX))
        .cloned()
        .collect();

    let mut log = Log {
        enabled: own.iter().any(|a| *a == "--boundary-log")
            || std::env::var("LUAU_LSP_BOUNDARY_LOG").is_ok_and(|v| v != "0"),
    };

    // Wait for `initialize` before launching the server: it carries the
    // workspace roots, which give us the child's working directory and rules.
    let mut editor_in = BufReader::new(io::stdin());
    let mut backlog: Vec<Vec<u8>> = Vec::new();
    let mut roots: Vec<String> = Vec::new();

    while let Ok(Some(body)) = read_message(&mut editor_in) {
        let is_initialize = serde_json::from_slice::<Value>(&body)
            .ok()
            .filter(|m| m.get("method").and_then(Value::as_str) == Some("initialize"))
            .inspect(|m| roots = roots_from_initialize(m))
            .is_some();
        backlog.push(body);
        if is_initialize {
            break;
        }
    }

    let mut workspace = Workspace::load(&roots, |root, error| {
        // Always reported, even without --boundary-log: a broken rules file
        // turns the proxy into a passthrough with nothing to say so.
        eprintln!(
            "[boundary] {}/{} unreadable, no filtering for this root: {error}",
            root,
            Workspace::RULES_FILE
        );
    });

    // `"log": true` in the rules file: the only switch a VS Code user can flip.
    log.enabled |= workspace.roots().any(|(_, rules)| rules.wants_log());

    for (root, rules) in workspace.roots() {
        if rules.is_empty() {
            log.say(format!("{root}: no rules, passthrough"));
        } else {
            log.say(format!("{root}: contexts {:?}", rules.context_names()));
        }
    }

    let Some(mut child) = spawn_server(&forwarded, roots.first(), &log) else {
        std::process::exit(1);
    };

    let mut server_in = child.stdin.take().expect("server stdin");
    let server_out = child.stdout.take().expect("server stdout");

    for body in &backlog {
        if write_message(&mut server_in, body).is_err() {
            eprintln!("[boundary] the server closed its input during the handshake");
            std::process::exit(1);
        }
    }

    let pending = Arc::new(Mutex::new(Pending::default()));

    // editor -> server
    let up_pending = Arc::clone(&pending);
    let upstream = thread::spawn(move || {
        while let Ok(Some(body)) = read_message(&mut editor_in) {
            // Rule the message out on its bytes first. Document syncs are the
            // bulk of this direction and none of them can be a completion.
            if contains(&body, COMPLETION_METHOD) {
                if let Ok(message) = serde_json::from_slice::<Value>(&body) {
                    if message.get("method").and_then(Value::as_str)
                        == Some("textDocument/completion")
                    {
                        let id = message.get("id").and_then(id_key);
                        let uri = message
                            .get("params")
                            .and_then(|p| p.get("textDocument"))
                            .and_then(|d| d.get("uri"))
                            .and_then(Value::as_str);
                        if let (Some(id), Some(uri)) = (id, uri) {
                            up_pending.lock().unwrap().insert(id, uri.to_string());
                        }
                    }
                }
            }
            if write_message(&mut server_in, &body).is_err() {
                break;
            }
        }
    });

    // server -> editor
    let mut server_out = BufReader::new(server_out);
    let mut editor_out = io::stdout();
    while let Ok(Some(body)) = read_message(&mut server_out) {
        let mut body = body;
        // With nothing awaiting a completion there is nothing this direction
        // can act on, and diagnostics and semantic tokens are large.
        let worth_parsing = !pending.lock().unwrap().is_empty();
        if let Some(Ok(mut message)) = worth_parsing.then(|| serde_json::from_slice::<Value>(&body))
        {
            // Only a response can match a pending request. Server-initiated
            // requests such as `workspace/configuration` travel the same way
            // and number their ids from their own counter, so without this
            // guard one of them would consume the entry for an unrelated
            // completion and that completion would silently go unfiltered.
            let uri = if is_response(&message) {
                message
                    .get("id")
                    .and_then(id_key)
                    .and_then(|id| pending.lock().unwrap().take(&id))
            } else {
                None
            };

            if let Some(uri) = uri {
                // Rules are re-read when their file changes, so editing,
                // adding or removing one takes effect without a reload.
                workspace.refresh(
                    |root, error| {
                        eprintln!(
                            "[boundary] {}/{} unreadable, no filtering for this root: {error}",
                            root,
                            Workspace::RULES_FILE
                        );
                    },
                    |root, rules| {
                        if rules.is_empty() {
                            log.say(format!("{root}: rules gone, passthrough"));
                        } else {
                            log.say(format!(
                                "{root}: rules reloaded, {:?}",
                                rules.context_names()
                            ));
                        }
                    },
                );
                // So that turning `"log": true` on is itself hot.
                log.enabled |= workspace.roots().any(|(_, rules)| rules.wants_log());

                let path = uri_to_path(&uri);
                if let Some(rules) = workspace.rules_for(&path) {
                    let caller = rules.context_for_file(&path);
                    let removed = filter_completion(&mut message, rules, caller);
                    if removed > 0 {
                        log.say(format!(
                            "dropped {} suggestion(s) for {} (context {})",
                            removed,
                            path,
                            caller.unwrap_or("Shared")
                        ));
                        if let Ok(encoded) = serde_json::to_vec(&message) {
                            body = encoded;
                        }
                    }
                }
            }
        }
        if write_message(&mut editor_out, &body).is_err() {
            break;
        }
    }

    // The server closed its output, so it is on its way out. Leave with its
    // status rather than joining the upstream thread: that thread blocks in a
    // read until the editor closes its own end, and waiting on it would leave a
    // live but mute proxy where the editor expects a dead server it can
    // restart.
    drop(upstream);
    let code = child
        .wait()
        .ok()
        .and_then(|status| status.code())
        .unwrap_or(0);
    std::process::exit(code);
}
