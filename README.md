# luau-lsp-boundary

Stops [luau-lsp](https://github.com/JohnnyMorganz/luau-lsp) from suggesting a
client module while you are editing a server script, and the other way round,
using your project's folder layout rather than a fixed list of Roblox service
names.

A transparent proxy: your editor launches it, it launches luau-lsp, every LSP
message passes through untouched except completion responses.

```
editor  ←→  luau-lsp-boundary  ←→  luau-lsp
```

## Quickstart

**1. Install**

```sh
rokit add rbx-dev-tools/luau-lsp-boundary
```

Or download from [Releases](https://github.com/rbx-dev-tools/luau-lsp-boundary/releases),
or `cargo build --release`.

**2. Point your editor at it**

VS Code, in your **user** settings (`Preferences: Open User Settings (JSON)`):

```json
"luau-lsp.server.path": "/absolute/path/to/luau-lsp-boundary"
```

Any other editor: replace the luau-lsp command with `luau-lsp-boundary`.
Arguments are forwarded verbatim.

**3. Add rules to your repo**

`.luau-lsp-boundary.json`, at the workspace root:

```json
{
  "$schema": "https://raw.githubusercontent.com/rbx-dev-tools/luau-lsp-boundary/main/schema/luau-lsp-boundary.schema.json",
  "contexts": {
    "Client": ["**/Client/**"],
    "Server": ["**/Server/**"]
  }
}
```

The `$schema` line is optional but worth keeping: it gives you completion and
inline errors while you type the file, instead of a message on stderr after a
reload.

**4. Reload the window.** Done.

Without a rules file the proxy is a pure passthrough, so the user setting in
step 2 is safe to leave on for every project. Only that first step needs a
reload: the rules file itself is re-read whenever it changes, so later edits,
and adding or deleting it, take effect on the next completion.

## Rules

A file matching no context is **Shared**: it sees everything, and everything
sees it. Two *different* named contexts are incompatible. The names themselves
carry no meaning, so invent your own:

```json
{
  "contexts": {
    "Client": ["**/client/**", "**/ui/**"],
    "Server": ["**/server/**", "**/datastore/**"],
    "Plugin": ["**/plugin/**"]
  }
}
```

Contexts are tested in the order written; first match wins. Globs match the
whole path, `/` separators on every platform, `*` never crosses a `/`, so use
`**`. Case-sensitive.

### Files and modules

The file you are editing is a path on disk. A suggested module is either a
DataModel path (`game/ReplicatedStorage/...`) or a path on disk, depending on
whether it is an instance require or a string require. The shorthand above
applies its globs to both. Split them when they differ:

```json
{
  "contexts": {
    "Server": {
      "files": ["**/Server/**"],
      "modules": ["**/ServerScriptService/**", "**/Server/**"]
    }
  }
}
```

### Logging

```json
{ "log": true, "contexts": { } }
```

Reports the server it launched, the contexts it loaded, and every suggestion it
drops. This is the only switch VS Code can reach, since the extension lets you
neither add an argument nor set an environment variable. Elsewhere,
`--boundary-log` or `LUAU_LSP_BOUNDARY_LOG=1` do the same.

Output goes to stderr: in VS Code, the *Luau Language Server* output channel.

A rules file that exists but cannot be parsed is always reported, log or no
log, and the root falls back to passthrough. The file is validated rather than
read best-effort: an unknown key, a bare string where an array belongs, or a
context with no globs is an error. All of those would otherwise produce rules
that match nothing, and a filter that quietly stops filtering is the worst
thing this tool can do.

Arguments starting with `--boundary-` are consumed by the proxy, never
forwarded.

## Which luau-lsp gets launched

In order: `LUAU_LSP_BOUNDARY_SERVER` if set, then `luau-lsp` from `PATH`, then
the rokit and aftman shims in your home directory, then the server bundled with
the VS Code extension.

That last fallback covers the common case rather than the rare one: most VS Code
users never install luau-lsp separately and have none on their `PATH`. The
newest installed extension wins, compared numerically so that `1.10` sorts above
`1.9`.

It is launched **with the workspace root as its working directory**, which is
what lets rokit resolve the version pinned in that repo's `rokit.toml`. Your
editor spawns the proxy from an arbitrary directory, so without this you would
silently get your globally installed version. Two windows on two repos pinning
different versions each get the right one.

The proxy's own version is still resolved from an unknown directory, so its
global pin is what applies. Barely matters for a tool this small, but worth
knowing.

## Limitations

- Completion responses only. It will not stop you writing an invalid require by
  hand, and it is no substitute for the boundary being enforced at runtime.
- Auto-import items are recognised by their shape (`kind == Module` plus a
  non-empty `additionalTextEdits`). If upstream changes how they are built, the
  filter silently stops matching, so turn on logging to find out.
- In a multi-root workspace the deepest root containing the file supplies the
  rules, but the working directory comes from the first root. Open repos with
  different luau-lsp pins in separate windows.

## Why this exists

luau-lsp has filtered these suggestions since 1.67.0, and on a default Rojo
project it works. Three gaps remain.

**It only knows Roblox service names.** A module's context depends on whether an
ancestor is literally named `ServerScriptService`, `ServerStorage`,
`StarterPlayer`, `StarterGui`, `StarterPack` or `ReplicatedFirst`. Everything
under `ReplicatedStorage` is Shared, and Shared is compatible with everything,
so if your client-only modules live in `ReplicatedStorage/.../Client`, as many
projects do, nothing is ever filtered. A deliberate upstream decision, contested
in [#1504](https://github.com/JohnnyMorganz/luau-lsp/issues/1504), still open.

**It only filters instance requires.** `isScriptContextCompatible` has one call
site, in the instance-require importer. String requires, written
`require("@game/...")` and enabled by
`luau-lsp.completion.imports.stringRequires.enabled`, bypass it entirely,
despite the 1.67.0 changelog announcing both.

**It reads `className`, not `RunContext`.** A script is Server if its class is
`Script`, Client if it is `LocalScript`. A sourcemap cannot express
`RunContext`, so under Rojo's `emitLegacyScripts: false` a `.client.luau`
becomes a plain `Script` and is classified Server. The original PR
([#1482](https://github.com/JohnnyMorganz/luau-lsp/pull/1482)) described
categorising by `.client.luau` / `.server.luau` file extension; the merged code
does not.

The first two are worth fixing upstream. The third is why this proxy ignores
class and context entirely and looks only at paths. But no upstream fix can
teach luau-lsp that `features/*/Client` means client in *your* repo. That part
is what this tool owns.

## Development

```sh
cargo test                      # filtering logic, rules parsing, LSP framing
cargo build
node tests/e2e/run.mjs          # end-to-end against a stand-in server
node tests/e2e/real-server.mjs  # end-to-end against the real luau-lsp
```

`real-server.mjs` builds a small Rojo-shaped project in a temporary directory
and asks an actual luau-lsp for a completion, once with a rules file and once
without. It is the only test that can catch upstream changing the shape of an
auto-import item, which would otherwise leave every other test passing on a
filter that matches nothing. It skips when no `luau-lsp` is on `PATH`.

## License

MPL-2.0
