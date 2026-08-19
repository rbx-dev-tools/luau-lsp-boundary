// End-to-end test against the real luau-lsp.
//
// Every other test in this repo drives a stand-in server whose completion items
// were written from a reading of luau-lsp's source. If that reading is wrong,
// all of them pass while the filter matches nothing. This one closes that gap:
// it builds a small Rojo-shaped project in a temporary directory, asks the
// actual language server for a completion, and compares the answer with and
// without a rules file.
//
// The comparison is what makes it trustworthy. The unfiltered run must contain
// the client module; if it does not, auto-import never fired and the test says
// so instead of passing on an empty result.
//
//   node tests/e2e/real-server.mjs [path/to/luau-lsp-boundary]
//
// Skips with exit code 0 when no luau-lsp can be found.

import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { mkdtemp, writeFile, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const isWindows = process.platform === "win32";
const proxy =
  process.argv[2] ??
  path.join(
    here,
    "..",
    "..",
    "target",
    "debug",
    isWindows ? "luau-lsp-boundary.exe" : "luau-lsp-boundary",
  );

const RULES = {
  contexts: {
    Client: ["**/Client/**"],
    Server: ["**/Server/**"],
  },
};

const MODULE = "return {}\n";
// The caller: a server script part-way through typing a module name.
const CALLER = "local widget = client";

const sourcemap = {
  name: "root",
  className: "DataModel",
  children: [
    {
      name: "ReplicatedStorage",
      className: "ReplicatedStorage",
      children: [
        {
          name: "Features",
          className: "Folder",
          children: [
            {
              name: "Example",
              className: "Folder",
              children: [
                {
                  name: "Client",
                  className: "Folder",
                  children: [
                    {
                      name: "clientWidget",
                      className: "ModuleScript",
                      filePaths: ["features/Example/Client/clientWidget.luau"],
                    },
                  ],
                },
                {
                  name: "Shared",
                  className: "Folder",
                  children: [
                    {
                      name: "clientAgnosticHelper",
                      className: "ModuleScript",
                      filePaths: ["features/Example/Shared/clientAgnosticHelper.luau"],
                    },
                  ],
                },
              ],
            },
          ],
        },
      ],
    },
    {
      name: "ServerScriptService",
      className: "ServerScriptService",
      children: [
        {
          name: "Features",
          className: "Folder",
          children: [
            {
              name: "Example",
              className: "Folder",
              children: [
                {
                  name: "Server",
                  className: "Folder",
                  children: [
                    {
                      name: "main",
                      className: "Script",
                      filePaths: ["features/Example/Server/main.server.luau"],
                    },
                  ],
                },
              ],
            },
          ],
        },
      ],
    },
  ],
};

const findServer = () => {
  if (process.env.LUAU_LSP_BOUNDARY_SERVER) {
    return process.env.LUAU_LSP_BOUNDARY_SERVER;
  }
  const probe = spawnSync("luau-lsp", ["--version"], { encoding: "utf8" });
  return probe.status === 0 ? "luau-lsp" : null;
};

/** Lays out a project the language server can index. */
const makeProject = async (withRules) => {
  const root = await mkdtemp(path.join(tmpdir(), "boundary-real-"));
  const write = async (relative, contents) => {
    const target = path.join(root, relative);
    await mkdir(path.dirname(target), { recursive: true });
    await writeFile(target, contents);
  };

  await write("features/Example/Client/clientWidget.luau", MODULE);
  await write("features/Example/Shared/clientAgnosticHelper.luau", MODULE);
  await write("features/Example/Server/main.server.luau", CALLER);
  await write("sourcemap.json", JSON.stringify(sourcemap, null, 2));
  // Without the roblox platform the sourcemap is never read, and no instance
  // require would ever be suggested.
  await write(".lsprc.json", JSON.stringify({ "luau-lsp.platform.type": "roblox" }));
  await write(".luaurc", JSON.stringify({ languageMode: "nonstrict" }));
  if (withRules) {
    await write(".luau-lsp-boundary.json", JSON.stringify(RULES, null, 2));
  }
  return root.replaceAll("\\", "/");
};

const uriOf = (absolute) =>
  `file:///${absolute.replace(/^\//, "").replaceAll("\\", "/")}`;

/** Drives one full session and returns the labels of the auto-import items. */
const completionLabels = (root, server) =>
  new Promise((resolve, reject) => {
    const child = spawn(proxy, ["lsp"], {
      env: { ...process.env, LUAU_LSP_BOUNDARY_SERVER: server },
      stdio: ["pipe", "pipe", "inherit"],
    });

    let buffer = Buffer.alloc(0);
    let nextId = 10;
    let attempts = 0;
    let settled = false;

    const finish = (fn, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(deadline);
      child.kill();
      fn(value);
    };

    const deadline = setTimeout(
      () => finish(reject, new Error("the language server never answered in 60s")),
      60000,
    );

    const send = (object) => {
      const body = Buffer.from(JSON.stringify(object), "utf8");
      child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
      child.stdin.write(body);
    };

    const askForCompletion = () => {
      attempts += 1;
      send({
        jsonrpc: "2.0",
        id: nextId,
        method: "textDocument/completion",
        params: {
          textDocument: { uri: uriOf(`${root}/features/Example/Server/main.server.luau`) },
          position: { line: 0, character: CALLER.length },
        },
      });
    };

    child.on("error", (error) => finish(reject, error));

    child.stdout.on("data", (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      for (;;) {
        const separator = buffer.indexOf("\r\n\r\n");
        if (separator === -1) return;
        const header = buffer.subarray(0, separator).toString();
        const length = Number(/Content-Length: (\d+)/i.exec(header)[1]);
        if (buffer.length < separator + 4 + length) return;
        const message = JSON.parse(
          buffer.subarray(separator + 4, separator + 4 + length).toString(),
        );
        buffer = buffer.subarray(separator + 4 + length);

        if (message.id === 1 && message.result) {
          send({ jsonrpc: "2.0", method: "initialized", params: {} });
          send({
            jsonrpc: "2.0",
            method: "textDocument/didOpen",
            params: {
              textDocument: {
                uri: uriOf(`${root}/features/Example/Server/main.server.luau`),
                languageId: "luau",
                version: 1,
                text: CALLER,
              },
            },
          });
          // Indexing the sourcemap is asynchronous, so the first ask can
          // legitimately come back empty.
          setTimeout(askForCompletion, 1500);
          continue;
        }

        if (message.id === nextId && message.result) {
          const items = message.result.items ?? message.result ?? [];
          const modules = items
            .filter((item) => item.kind === 9 && item.additionalTextEdits?.length)
            .map((item) => item.label);

          if (modules.length === 0 && attempts < 8) {
            nextId += 1;
            setTimeout(askForCompletion, 1000);
            continue;
          }
          finish(resolve, modules);
        }
      }
    });

    send({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        processId: process.pid,
        capabilities: { textDocument: { completion: { completionItem: {} } } },
        workspaceFolders: [{ uri: uriOf(root), name: "example" }],
      },
    });
  });

const server = findServer();
if (!server) {
  console.log("skip  no luau-lsp on PATH, real-server test not run");
  process.exit(0);
}
console.log(`using luau-lsp: ${server}`);

let failures = 0;
const check = (name, passed, detail) => {
  console.log(`${passed ? "ok  " : "FAIL"} ${name}`);
  if (!passed) {
    console.log(`     ${detail}`);
    failures += 1;
  }
};

const bare = await makeProject(false);
const configured = await makeProject(true);

try {
  const unfiltered = await completionLabels(bare, server);

  // The control. If this fails, nothing below means anything: the language
  // server never offered the module in the first place, so an empty filtered
  // result would prove nothing at all.
  if (!unfiltered.includes("clientWidget")) {
    console.log("FAIL control: luau-lsp never suggested the client module");
    console.log(`     auto-import items seen: ${JSON.stringify(unfiltered)}`);
    console.log("     the shape this proxy filters on may have changed upstream");
    process.exit(1);
  }
  console.log(`     unfiltered: ${JSON.stringify(unfiltered)}`);

  const filtered = await completionLabels(configured, server);
  console.log(`     filtered:   ${JSON.stringify(filtered)}`);

  check(
    "the client module is gone from a server file",
    !filtered.includes("clientWidget"),
    `still present in ${JSON.stringify(filtered)}`,
  );
  check(
    "the shared module survives",
    filtered.includes("clientAgnosticHelper"),
    `missing from ${JSON.stringify(filtered)}`,
  );
} finally {
  // The language server has only just been killed and may still hold the
  // directory open, which Windows reports as EBUSY.
  const clean = { recursive: true, force: true, maxRetries: 10, retryDelay: 200 };
  await rm(bare, clean).catch(() => {});
  await rm(configured, clean).catch(() => {});
}

console.log(failures === 0 ? "\nall cases pass" : `\n${failures} failure(s)`);
process.exit(failures === 0 ? 0 : 1);
