//! The proxy's pure logic: reading rules, classifying paths into contexts,
//! filtering a `textDocument/completion` response, and framing LSP messages.
//! Everything touching processes lives in `main.rs`.

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::Value;
use std::io::{self, BufRead, Write};
use std::path::Path;

/// The two namespaces a rule can address.
///
/// An open file is named by its path on disk. A suggested module is named
/// either by its virtual DataModel path (`game/ReplicatedStorage/...`, for
/// instance requires) or by its path on disk (for string requires). Both of
/// those go through `modules`.
#[derive(Debug)]
struct Matcher {
    files: GlobSet,
    modules: GlobSet,
}

#[derive(Debug, Default)]
pub struct Rules {
    contexts: Vec<(String, Matcher)>,
    /// `"log": true` in the rules file. This is the only switch reachable from
    /// VS Code, which lets you neither add an argument to the language server
    /// nor set its environment.
    log: bool,
}

fn build_globs(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|e| format!("invalid glob `{pattern}`: {e}"))?;
        builder.add(glob);
    }
    builder.build().map_err(|e| e.to_string())
}

fn patterns_from(value: &Value) -> Result<Vec<String>, String> {
    let Some(items) = value.as_array() else {
        return Err("expected an array of glob strings".to_string());
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("expected a glob string, found `{item}`"))
        })
        .collect()
}

impl Rules {
    /// No rules at all: everything is Shared, so nothing is ever filtered.
    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    pub fn context_names(&self) -> Vec<&str> {
        self.contexts.iter().map(|(n, _)| n.as_str()).collect()
    }

    pub fn wants_log(&self) -> bool {
        self.log
    }

    /// Every rules file is validated rather than best-effort parsed.
    ///
    /// The failure this guards against is the quiet one: a misspelled key or a
    /// bare string where an array belongs would otherwise leave the proxy with
    /// no rule that ever matches, filtering nothing and saying nothing. A
    /// rejected file at least reports itself.
    pub fn from_json(json: &Value) -> Result<Rules, String> {
        let Some(top) = json.as_object() else {
            return Err("the rules file must contain a JSON object".to_string());
        };
        for key in top.keys() {
            // `$schema` is the editor's, not ours: it is what makes a mistake
            // visible while the file is being typed rather than at runtime.
            if key != "log" && key != "contexts" && key != "$schema" {
                return Err(format!("unknown key `{key}`, expected `log` or `contexts`"));
            }
        }

        let log = match top.get("log") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err("`log` must be true or false".to_string()),
        };

        let map = match top.get("contexts") {
            None => {
                return Ok(Rules {
                    log,
                    ..Rules::default()
                })
            }
            Some(Value::Object(map)) => map,
            Some(_) => return Err("`contexts` must be an object".to_string()),
        };

        let mut contexts = Vec::new();
        for (name, spec) in map {
            // Short form: an array of globs applies to both namespaces.
            // Long form: { "files": [...], "modules": [...] }.
            let (files, modules) = match spec {
                Value::Array(_) => {
                    let patterns = patterns_from(spec)?;
                    (patterns.clone(), patterns)
                }
                Value::Object(fields) => {
                    for key in fields.keys() {
                        if key != "files" && key != "modules" {
                            return Err(format!(
                                "context `{name}`: unknown key `{key}`, expected `files` or `modules`"
                            ));
                        }
                    }
                    (
                        fields.get("files").map(patterns_from).transpose()?.unwrap_or_default(),
                        fields.get("modules").map(patterns_from).transpose()?.unwrap_or_default(),
                    )
                }
                _ => {
                    return Err(format!(
                        "context `{name}` must be an array of globs, or an object with `files` and `modules`"
                    ))
                }
            };

            if files.is_empty() && modules.is_empty() {
                return Err(format!(
                    "context `{name}` has no globs, so it can never match"
                ));
            }

            contexts.push((
                name.clone(),
                Matcher {
                    files: build_globs(&files)?,
                    modules: build_globs(&modules)?,
                },
            ));
        }
        Ok(Rules { contexts, log })
    }

    pub fn from_file(path: &Path) -> Result<Rules, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let json: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Rules::from_json(&json)
    }

    /// The context of a file open in the editor.
    pub fn context_for_file(&self, path: &str) -> Option<&str> {
        let path = normalize(path);
        self.contexts
            .iter()
            .find(|(_, m)| m.files.is_match(&path))
            .map(|(n, _)| n.as_str())
    }

    /// The context of a suggested module, tested against every path the
    /// completion item exposes.
    pub fn context_for_module(&self, candidates: &[String]) -> Option<&str> {
        self.contexts
            .iter()
            .find(|(_, m)| candidates.iter().any(|c| m.modules.is_match(c)))
            .map(|(n, _)| n.as_str())
    }
}

/// A missing context (Shared) is compatible with everything; otherwise the two
/// must be equal. This mirrors `isScriptContextCompatible` upstream.
pub fn compatible(from: Option<&str>, target: Option<&str>) -> bool {
    match (from, target) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

/// The shape of a luau-lsp auto-import item: `createSuggestRequire` sets
/// `kind = Module` (9) and always fills `additionalTextEdits` in the initial
/// response, with no lazy `completionItem/resolve`.
pub fn is_auto_import(item: &Value) -> bool {
    item.get("kind").and_then(Value::as_u64) == Some(9)
        && item
            .get("additionalTextEdits")
            .and_then(Value::as_array)
            .is_some_and(|edits| !edits.is_empty())
}

/// Every path an auto-import item exposes, normalized.
///
/// `detail` carries the require path (`ReplicatedStorage.Features.X`, or
/// `@game/...`), and the documentation ends with the module's full path:
/// virtual for an instance require, on disk for a string require.
pub fn item_paths(item: &Value) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(detail) = item.get("detail").and_then(Value::as_str) {
        out.push(normalize(&detail.replace('.', "/")));
    }

    let documentation = match item.get("documentation") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Object(o)) => o.get("value").and_then(Value::as_str),
        _ => None,
    };
    if let Some(doc) = documentation {
        // The path is the last paragraph, after the code block.
        if let Some(tail) = doc.rsplit("\n\n").next() {
            let tail = tail.trim();
            if !tail.is_empty() {
                out.push(normalize(tail));
            }
        }
    }

    out
}

/// Removes auto-imports incompatible with `caller` from a response.
/// Returns how many items were dropped.
pub fn filter_completion(message: &mut Value, rules: &Rules, caller: Option<&str>) -> usize {
    let Some(result) = message.get_mut("result") else {
        return 0;
    };
    let items = match result {
        Value::Object(o) => o.get_mut("items").and_then(Value::as_array_mut),
        Value::Array(_) => result.as_array_mut(),
        _ => None,
    };
    let Some(items) = items else {
        return 0;
    };

    let before = items.len();
    items.retain(|item| {
        if !is_auto_import(item) {
            return true;
        }
        compatible(caller, rules.context_for_module(&item_paths(item)))
    });
    before - items.len()
}

/// Whether `haystack` contains `needle`.
///
/// Used to rule a message out before parsing it. Every byte the editor and the
/// server exchange passes through this proxy, including whole-document syncs
/// and semantic token payloads, and deserializing all of that only to discover
/// it is not a completion would put a JSON parse on the path of every
/// keystroke.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Reads one LSP message (headers then body). `None` at end of stream.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok();
        }
    }
    let Some(length) = length else {
        return Ok(None);
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

pub fn write_message(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()
}

/// `file:///C:/a/b` -> `C:/a/b`, `file:///home/x` -> `/home/x`.
///
/// The slash before a Windows drive letter belongs to the URI syntax, not to
/// the path. Anywhere else it is the root and must survive, otherwise no
/// absolute POSIX path would ever match its workspace root again.
pub fn uri_to_path(uri: &str) -> String {
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    let decoded = percent_decode(path);
    let normalized = normalize(&decoded);
    let stripped = match normalized.strip_prefix('/') {
        Some(rest) if is_windows_drive(rest) => rest.to_string(),
        _ => normalized,
    };
    upper_drive(stripped)
}

fn is_windows_drive(path: &str) -> bool {
    let mut chars = path.chars();
    matches!((chars.next(), chars.next()), (Some(c), Some(':')) if c.is_ascii_alphabetic())
}

/// Uppercases a leading Windows drive letter.
///
/// Editors are not consistent about its case: a root announced as `file:///C:/`
/// and a document opened as `file:///c:/` describe the same place, and without
/// this the file would sit in no root at all and quietly go unfiltered.
fn upper_drive(mut path: String) -> String {
    if is_windows_drive(&path) {
        let head: String = path[..1].to_ascii_uppercase();
        path.replace_range(..1, &head);
    }
    path
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The prefix of luau-lsp's VS Code extension directories, which are named
/// `johnnymorganz.luau-lsp-<version>-<platform>`.
pub const EXTENSION_PREFIX: &str = "johnnymorganz.luau-lsp-";

fn version_of(directory: &str) -> Vec<u32> {
    directory
        .strip_prefix(EXTENSION_PREFIX)
        .unwrap_or(directory)
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map_while(|part| part.parse::<u32>().ok())
        .collect()
}

/// Of several extension directories, the one carrying the highest version.
///
/// The ordering is numeric rather than lexicographic: otherwise `1.9.0` would
/// sort above `1.10.0` and the proxy would silently launch a stale server.
pub fn newest_extension<'a>(directories: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    directories
        .into_iter()
        .filter(|name| name.starts_with(EXTENSION_PREFIX))
        .max_by_key(|name| version_of(name))
}

/// Completion requests waiting for their response: id -> URI of the asking file.
///
/// An entry is normally taken when the response arrives, but the editor cancels
/// completion requests constantly while you type, and a cancelled one may never
/// be answered. The store is therefore bounded and evicts oldest-first, rather
/// than growing for the whole life of the session.
#[derive(Debug, Default)]
pub struct Pending {
    entries: std::collections::VecDeque<(String, String)>,
}

impl Pending {
    pub const CAPACITY: usize = 256;

    pub fn insert(&mut self, id: String, uri: String) {
        if self.entries.len() >= Self::CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((id, uri));
    }

    pub fn take(&mut self, id: &str) -> Option<String> {
        let index = self.entries.iter().position(|(key, _)| key == id)?;
        self.entries.remove(index).map(|(_, uri)| uri)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

struct Root {
    path: String,
    rules: Rules,
    /// Modification time of the rules file, `None` when it does not exist.
    stamp: Option<std::time::SystemTime>,
    checked_at: std::time::Instant,
}

/// The roots announced at `initialize`, each with its own rules.
pub struct Workspace {
    roots: Vec<Root>,
}

impl Workspace {
    pub const RULES_FILE: &'static str = ".luau-lsp-boundary.json";

    /// How long a look at the rules file's timestamp is trusted before taking
    /// another. Completions fire on nearly every keystroke, and this keeps the
    /// reload check off that path without making an edit feel slow to land.
    pub const RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

    /// `on_problem` receives rules files that exist but cannot be read: staying
    /// silent there would turn the proxy into a passthrough with no clue why.
    pub fn load(roots: &[String], mut on_problem: impl FnMut(&str, String)) -> Workspace {
        let roots = roots
            .iter()
            .map(|root| {
                let path = normalize(root);
                let (rules, stamp) = Self::read(&path, &mut on_problem);
                Root {
                    path,
                    rules,
                    stamp,
                    checked_at: std::time::Instant::now(),
                }
            })
            .collect();
        Workspace { roots }
    }

    fn rules_path(root: &str) -> std::path::PathBuf {
        Path::new(root).join(Self::RULES_FILE)
    }

    fn stamp_of(root: &str) -> Option<std::time::SystemTime> {
        std::fs::metadata(Self::rules_path(root))
            .ok()
            .and_then(|meta| meta.modified().ok())
    }

    fn read(
        root: &str,
        on_problem: &mut impl FnMut(&str, String),
    ) -> (Rules, Option<std::time::SystemTime>) {
        let path = Self::rules_path(root);
        if !path.exists() {
            return (Rules::default(), None);
        }
        let stamp = Self::stamp_of(root);
        match Rules::from_file(&path) {
            Ok(rules) => (rules, stamp),
            Err(error) => {
                on_problem(root, error);
                (Rules::default(), stamp)
            }
        }
    }

    /// Re-reads any rules file whose timestamp moved, so editing it, creating
    /// it, or deleting it takes effect without restarting the language server.
    ///
    /// `on_change` fires only for roots that actually reloaded.
    pub fn refresh(
        &mut self,
        mut on_problem: impl FnMut(&str, String),
        mut on_change: impl FnMut(&str, &Rules),
    ) {
        let now = std::time::Instant::now();
        for root in &mut self.roots {
            if now.duration_since(root.checked_at) < Self::RELOAD_INTERVAL {
                continue;
            }
            root.checked_at = now;

            let stamp = Self::stamp_of(&root.path);
            // A file that appeared, vanished, or was written since last time.
            if stamp == root.stamp {
                continue;
            }
            let (rules, stamp) = Self::read(&root.path, &mut on_problem);
            root.rules = rules;
            root.stamp = stamp;
            on_change(&root.path, &root.rules);
        }
    }

    pub fn roots(&self) -> impl Iterator<Item = (&str, &Rules)> {
        self.roots.iter().map(|r| (r.path.as_str(), &r.rules))
    }

    /// The rules of the root containing this file. In a multi-root workspace
    /// the deepest root wins.
    pub fn rules_for(&self, file_path: &str) -> Option<&Rules> {
        let file_path = normalize(file_path);
        self.roots
            .iter()
            .filter(|root| contains_path(&root.path, &file_path))
            .max_by_key(|root| root.path.len())
            .map(|root| &root.rules)
    }
}

/// Whether `file` sits inside `root`.
///
/// A plain `starts_with` is wrong here: it would also place `/work/app-other`
/// inside `/work/app`, handing a sibling directory another root's rules. The
/// match has to land on a path separator.
fn contains_path(root: &str, file: &str) -> bool {
    let root = root.trim_end_matches('/');
    match file.strip_prefix(root) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules() -> Rules {
        Rules::from_json(&json!({
            "contexts": {
                "Client": ["**/Client/**"],
                "Server": ["**/Server/**"]
            }
        }))
        .unwrap()
    }

    fn auto_import(detail: &str, module_path: &str) -> Value {
        named_auto_import("x", detail, module_path)
    }

    fn named_auto_import(label: &str, detail: &str, module_path: &str) -> Value {
        json!({
            "label": label,
            "kind": 9,
            "detail": detail,
            "documentation": {
                "kind": "markdown",
                "value": format!("```luau\nlocal x = require(...)\n```\n\n{module_path}")
            },
            "additionalTextEdits": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "newText": "local x = require(...)\n"
            }]
        })
    }

    #[test]
    fn classifies_files_by_folder() {
        let rules = rules();
        assert_eq!(
            rules.context_for_file("/work/app/features/Example/Client/main.client.luau"),
            Some("Client")
        );
        assert_eq!(
            rules.context_for_file("/work/app/features/Example/Server/main.server.luau"),
            Some("Server")
        );
        assert_eq!(
            rules.context_for_file("/work/app/features/Example/Shared/draw.luau"),
            None
        );
    }

    #[test]
    fn accepts_windows_separators() {
        assert_eq!(
            rules().context_for_file(r"C:\work\app\features\Example\Client\main.luau"),
            Some("Client")
        );
    }

    #[test]
    fn shared_is_compatible_both_ways() {
        assert!(compatible(None, Some("Client")));
        assert!(compatible(Some("Server"), None));
        assert!(compatible(None, None));
        assert!(compatible(Some("Client"), Some("Client")));
        assert!(!compatible(Some("Server"), Some("Client")));
    }

    #[test]
    fn reads_instance_require_dotted_detail() {
        let item = auto_import(
            "ReplicatedStorage.Features.Example.Client.attach",
            "game/ReplicatedStorage/Features/Example/Client/attach",
        );
        assert_eq!(
            rules().context_for_module(&item_paths(&item)),
            Some("Client")
        );
    }

    #[test]
    fn reads_string_require_file_path() {
        let item = auto_import(
            "@game/ReplicatedStorage/Features/Example/Client/attach",
            "/work/app/features/Example/Client/attach.luau",
        );
        assert_eq!(
            rules().context_for_module(&item_paths(&item)),
            Some("Client")
        );
    }

    #[test]
    fn reads_relative_string_require_via_documentation() {
        // `detail` carries no context for a relative require; only the full
        // path at the end of the documentation does.
        let item = auto_import("./attach", "/work/app/features/Example/Client/attach.luau");
        assert_eq!(
            rules().context_for_module(&item_paths(&item)),
            Some("Client")
        );
    }

    #[test]
    fn drops_only_the_crossing_suggestions() {
        let mut message = json!({
            "id": 1,
            "result": {
                "isIncomplete": false,
                "items": [
                    named_auto_import("fromClient", "A.Client.a", "game/A/Client/a"),
                    named_auto_import("fromServer", "A.Server.b", "game/A/Server/b"),
                    named_auto_import("fromShared", "A.Shared.c", "game/A/Shared/c"),
                    {"label": "localVar", "kind": 6, "detail": "in /Client/"}
                ]
            }
        });
        let removed = filter_completion(&mut message, &rules(), Some("Server"));
        assert_eq!(removed, 1);
        let labels: Vec<&str> = message["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, vec!["fromServer", "fromShared", "localVar"]);
    }

    #[test]
    fn never_touches_plain_completions() {
        // kind 6 (Variable) under /Client/: not an auto-import.
        let mut message = json!({
            "id": 1,
            "result": {"items": [{"label": "v", "kind": 6, "detail": "game/A/Client/v"}]}
        });
        assert_eq!(filter_completion(&mut message, &rules(), Some("Server")), 0);
    }

    #[test]
    fn handles_bare_array_results() {
        let mut message = json!({
            "id": 1,
            "result": [auto_import("A.Client.a", "game/A/Client/a")]
        });
        assert_eq!(filter_completion(&mut message, &rules(), Some("Server")), 1);
    }

    #[test]
    fn empty_rules_filter_nothing() {
        let mut message = json!({
            "id": 1,
            "result": {"items": [auto_import("A.Client.a", "game/A/Client/a")]}
        });
        let empty = Rules::default();
        assert!(empty.is_empty());
        assert_eq!(filter_completion(&mut message, &empty, Some("Server")), 0);
    }

    #[test]
    fn long_form_separates_files_from_modules() {
        let rules = Rules::from_json(&json!({
            "contexts": {
                "Server": {
                    "files": ["**/Server/**"],
                    "modules": ["**/ServerScriptService/**"]
                }
            }
        }))
        .unwrap();
        assert_eq!(
            rules.context_for_file("/work/app/Server/a.luau"),
            Some("Server")
        );
        // A path on disk must not classify a module here.
        assert_eq!(
            rules.context_for_module(&["/work/app/Server/a.luau".to_string()]),
            None
        );
        assert_eq!(
            rules.context_for_module(&["game/ServerScriptService/a".to_string()]),
            Some("Server")
        );
    }

    #[test]
    fn contexts_keep_declaration_order() {
        let rules = Rules::from_json(&json!({
            "contexts": {
                "Zebra": ["**/Client/**"],
                "Alpha": ["**/Client/**"]
            }
        }))
        .unwrap();
        assert_eq!(rules.context_names(), vec!["Zebra", "Alpha"]);
        assert_eq!(rules.context_for_file("a/Client/b"), Some("Zebra"));
    }

    #[test]
    fn reads_the_log_switch() {
        assert!(Rules::from_json(&json!({"log": true})).unwrap().wants_log());
        assert!(!Rules::from_json(&json!({"contexts": {}}))
            .unwrap()
            .wants_log());
    }

    /// Every one of these would otherwise parse into rules that match nothing,
    /// leaving the proxy filtering silently and the author none the wiser.
    #[test]
    fn quiet_misconfigurations_are_rejected() {
        let cases = [
            (
                json!({"context": {"Client": ["**/Client/**"]}}),
                "unknown key",
            ),
            (
                json!({"contexts": {"Client": "**/Client/**"}}),
                "must be an array",
            ),
            (
                json!({"contexts": {"Client": {"file": ["a"]}}}),
                "unknown key",
            ),
            (json!({"contexts": {"Client": []}}), "no globs"),
            (json!({"contexts": {"Client": {}}}), "no globs"),
            (json!({"contexts": {"Client": [42]}}), "glob string"),
            (json!({"contexts": ["Client"]}), "must be an object"),
            (json!({"log": "yes"}), "true or false"),
            (json!([]), "must contain a JSON object"),
        ];
        for (input, expected) in cases {
            let error = Rules::from_json(&input)
                .err()
                .unwrap_or_else(|| panic!("{input} should have been rejected"));
            assert!(error.contains(expected), "for {input}, got: {error}");
        }
    }

    #[test]
    fn a_schema_annotation_is_allowed() {
        // Rejecting `$schema` would punish exactly the users who annotated
        // their file to get editor validation in the first place.
        let rules = Rules::from_json(&json!({
            "$schema": "https://example.invalid/luau-lsp-boundary.schema.json",
            "contexts": {"Client": ["**/Client/**"]}
        }))
        .unwrap();
        assert_eq!(rules.context_names(), vec!["Client"]);
    }

    /// The published schema has to accept what the parser accepts, and the
    /// examples in the README are the shapes people will copy.
    #[test]
    fn the_documented_shapes_parse() {
        let shapes = [
            json!({"contexts": {"Client": ["**/Client/**"], "Server": ["**/Server/**"]}}),
            json!({"contexts": {
                "Client": ["**/client/**", "**/ui/**"],
                "Server": ["**/server/**", "**/datastore/**"],
                "Plugin": ["**/plugin/**"]
            }}),
            json!({"contexts": {"Server": {
                "files": ["**/Server/**"],
                "modules": ["**/ServerScriptService/**", "**/Server/**"]
            }}}),
            json!({"log": true, "contexts": {}}),
        ];
        for shape in shapes {
            Rules::from_json(&shape).unwrap_or_else(|e| panic!("{shape} rejected: {e}"));
        }
    }

    #[test]
    fn a_file_with_only_a_log_switch_is_valid() {
        let rules = Rules::from_json(&json!({"log": true})).unwrap();
        assert!(rules.wants_log());
        assert!(rules.is_empty());
    }

    #[test]
    fn rules_out_messages_on_their_bytes() {
        assert!(contains(
            br#"{"method":"textDocument/completion"}"#,
            COMPLETION
        ));
        assert!(!contains(
            br#"{"method":"textDocument/didChange"}"#,
            COMPLETION
        ));
        assert!(!contains(b"", COMPLETION));
        assert!(!contains(b"short", COMPLETION));
        assert!(!contains(b"anything", b""));
    }

    const COMPLETION: &[u8] = b"textDocument/completion";

    #[test]
    fn invalid_glob_is_reported() {
        let err = Rules::from_json(&json!({"contexts": {"Client": ["["]}})).unwrap_err();
        assert!(err.contains("invalid glob"), "{err}");
    }

    fn workspace_of(entries: Vec<(&str, Rules)>) -> Workspace {
        Workspace {
            roots: entries
                .into_iter()
                .map(|(path, rules)| Root {
                    path: path.to_string(),
                    rules,
                    stamp: None,
                    checked_at: std::time::Instant::now(),
                })
                .collect(),
        }
    }

    /// A scratch directory unique to one test, cleaned up on the way out.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir().join(format!("luau-lsp-boundary-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn root(&self) -> String {
            self.0.to_string_lossy().replace('\\', "/")
        }

        fn write_rules(&self, contents: &str) {
            std::fs::write(self.0.join(Workspace::RULES_FILE), contents).unwrap();
        }

        fn remove_rules(&self) {
            let _ = std::fs::remove_file(self.0.join(Workspace::RULES_FILE));
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn force_recheck(workspace: &mut Workspace) {
        // Skip the throttle instead of sleeping through it.
        for root in &mut workspace.roots {
            root.checked_at = std::time::Instant::now() - Workspace::RELOAD_INTERVAL;
        }
    }

    #[test]
    fn picks_up_a_rules_file_created_after_startup() {
        let scratch = Scratch::new("created");
        let mut workspace = Workspace::load(&[scratch.root()], |_, e| panic!("{e}"));
        assert!(workspace.rules_for(&scratch.root()).unwrap().is_empty());

        scratch.write_rules(r#"{"contexts": {"Client": ["**/Client/**"]}}"#);
        force_recheck(&mut workspace);
        let mut changed = 0;
        workspace.refresh(|_, e| panic!("{e}"), |_, _| changed += 1);

        assert_eq!(changed, 1);
        let rules = workspace.rules_for(&scratch.root()).unwrap();
        assert_eq!(rules.context_names(), vec!["Client"]);
    }

    #[test]
    fn picks_up_an_edit_and_a_deletion() {
        let scratch = Scratch::new("edited");
        scratch.write_rules(r#"{"contexts": {"Client": ["**/Client/**"]}}"#);
        let mut workspace = Workspace::load(&[scratch.root()], |_, e| panic!("{e}"));
        assert_eq!(
            workspace
                .rules_for(&scratch.root())
                .unwrap()
                .context_names(),
            vec!["Client"]
        );

        scratch.write_rules(r#"{"contexts": {"Server": ["**/Server/**"]}}"#);
        force_recheck(&mut workspace);
        workspace.refresh(|_, e| panic!("{e}"), |_, _| {});
        assert_eq!(
            workspace
                .rules_for(&scratch.root())
                .unwrap()
                .context_names(),
            vec!["Server"]
        );

        scratch.remove_rules();
        force_recheck(&mut workspace);
        workspace.refresh(|_, e| panic!("{e}"), |_, _| {});
        assert!(
            workspace.rules_for(&scratch.root()).unwrap().is_empty(),
            "deleting the file must fall back to passthrough"
        );
    }

    #[test]
    fn a_broken_edit_is_reported_and_filters_nothing() {
        let scratch = Scratch::new("broken");
        scratch.write_rules(r#"{"contexts": {"Client": ["**/Client/**"]}}"#);
        let mut workspace = Workspace::load(&[scratch.root()], |_, e| panic!("{e}"));

        scratch.write_rules("{ not json");
        force_recheck(&mut workspace);
        let mut problems = 0;
        workspace.refresh(|_, _| problems += 1, |_, _| {});

        assert_eq!(problems, 1);
        assert!(workspace.rules_for(&scratch.root()).unwrap().is_empty());
    }

    #[test]
    fn the_throttle_holds_off_repeated_stats() {
        let scratch = Scratch::new("throttle");
        let mut workspace = Workspace::load(&[scratch.root()], |_, e| panic!("{e}"));
        scratch.write_rules(r#"{"contexts": {"Client": ["**/Client/**"]}}"#);

        let mut changed = 0;
        workspace.refresh(|_, e| panic!("{e}"), |_, _| changed += 1);
        assert_eq!(changed, 0, "not yet due for another look");

        force_recheck(&mut workspace);
        workspace.refresh(|_, e| panic!("{e}"), |_, _| changed += 1);
        assert_eq!(changed, 1);
    }

    #[test]
    fn deepest_root_wins_in_multi_root() {
        let workspace = workspace_of(vec![
            ("/work/app", rules()),
            ("/work/app/nested", Rules::default()),
        ]);
        assert!(workspace
            .rules_for("/work/app/nested/features/Client/x.luau")
            .unwrap()
            .is_empty());
        assert!(!workspace
            .rules_for("/work/app/features/Client/x.luau")
            .unwrap()
            .is_empty());
        assert!(workspace.rules_for("/elsewhere/x.luau").is_none());
    }

    #[test]
    fn a_sibling_directory_is_not_inside_the_root() {
        let workspace = workspace_of(vec![("/work/app", rules())]);
        assert!(
            workspace
                .rules_for("/work/app-other/features/Client/x.luau")
                .is_none(),
            "`/work/app-other` only shares a prefix with `/work/app`"
        );
        assert!(workspace
            .rules_for("/work/app/features/Client/x.luau")
            .is_some());
        assert!(
            workspace.rules_for("/work/app").is_some(),
            "the root itself"
        );
    }

    #[test]
    fn a_drive_letter_matches_whatever_its_case() {
        // VS Code is not consistent between the root it announces and the
        // documents it opens.
        assert_eq!(
            uri_to_path("file:///c:/work/app/x.luau"),
            "C:/work/app/x.luau"
        );
        let workspace = workspace_of(vec![(&uri_to_path("file:///C:/work/app"), rules())]);
        assert!(workspace
            .rules_for(&uri_to_path("file:///c:/work/app/features/Client/x.luau"))
            .is_some());
    }

    #[test]
    fn picks_the_newest_extension_numerically() {
        let dirs = [
            "johnnymorganz.luau-lsp-1.9.0-win32-x64",
            "johnnymorganz.luau-lsp-1.10.0-win32-x64",
            "johnnymorganz.luau-lsp-1.2.0-win32-x64",
            "drewbluewasabi.luau-theme-0.0.2",
        ];
        assert_eq!(
            newest_extension(dirs),
            Some("johnnymorganz.luau-lsp-1.10.0-win32-x64"),
            "1.10 must sort above 1.9"
        );
    }

    #[test]
    fn ignores_unrelated_extensions() {
        assert_eq!(
            newest_extension(["crossstarcross.solarized-luau-1.0.5"]),
            None
        );
        assert_eq!(newest_extension(std::iter::empty()), None);
    }

    #[test]
    fn pending_takes_by_id_and_stays_bounded() {
        let mut pending = Pending::default();
        pending.insert("1".into(), "file:///a".into());
        pending.insert("2".into(), "file:///b".into());
        assert_eq!(pending.take("2").as_deref(), Some("file:///b"));
        assert_eq!(pending.take("2"), None, "an entry is taken only once");
        assert_eq!(pending.take("1").as_deref(), Some("file:///a"));
        assert!(pending.is_empty());

        // Cancelled requests are never answered, so entries can accumulate;
        // the oldest must fall out instead of growing without bound.
        for i in 0..Pending::CAPACITY * 2 {
            pending.insert(i.to_string(), format!("file:///{i}"));
        }
        assert_eq!(pending.len(), Pending::CAPACITY);
        assert_eq!(pending.take("0"), None, "oldest evicted");
        assert!(pending
            .take(&(Pending::CAPACITY * 2 - 1).to_string())
            .is_some());
    }

    #[test]
    fn framing_round_trip() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, br#"{"a":1}"#).unwrap();
        assert_eq!(buffer, b"Content-Length: 7\r\n\r\n{\"a\":1}");
        let mut reader = io::BufReader::new(&buffer[..]);
        assert_eq!(
            read_message(&mut reader).unwrap().unwrap(),
            br#"{"a":1}"#.to_vec()
        );
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn decodes_uris() {
        assert_eq!(uri_to_path("file:///C:/a/b.luau"), "C:/a/b.luau");
        assert_eq!(
            uri_to_path("file:///C:/my%20repo/b.luau"),
            "C:/my repo/b.luau"
        );
        // The POSIX root must survive: without it no absolute Linux or macOS
        // path would still be prefixed by its workspace root.
        assert_eq!(uri_to_path("file:///home/x/b.luau"), "/home/x/b.luau");
    }
}
