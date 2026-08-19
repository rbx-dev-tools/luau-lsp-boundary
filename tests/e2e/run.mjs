// End-to-end test: launches the actual compiled binary, talks LSP to it the
// way an editor would, and checks what comes back out.
//
// The Rust unit tests cover the filtering logic. This covers what they cannot
// see: message framing, request/response correlation by id, loading rules from
// the root announced at `initialize`, and the fact that a repo with no rules
// file gets back exactly what the server produced.
//
//   node tests/e2e/run.mjs [path/to/luau-lsp-boundary]

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { mkdtemp, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const mock = path.join(here, "mock-server.mjs");
const binary =
  process.argv[2] ??
  path.join(
    here,
    "..",
    "..",
    "target",
    "debug",
    process.platform === "win32" ? "luau-lsp-boundary.exe" : "luau-lsp-boundary",
  );

const RULES = {
  contexts: {
    Client: ["**/Client/**"],
    Server: ["**/Server/**"],
  },
};

/** Creates a throwaway workspace root, with or without a rules file. */
const makeRoot = async (rules) => {
  const root = await mkdtemp(path.join(tmpdir(), "boundary-"));
  await mkdir(path.join(root, "features", "Client"), { recursive: true });
  await mkdir(path.join(root, "features", "Server"), { recursive: true });
  if (rules) {
    await writeFile(
      path.join(root, ".luau-lsp-boundary.json"),
      JSON.stringify(rules, null, 2),
    );
  }
  return root.replaceAll("\\", "/");
};

const complete = (root, callerPath) =>
  new Promise((resolve, reject) => {
    const child = spawn(binary, [mock], {
      env: { ...process.env, LUAU_LSP_BOUNDARY_SERVER: "node" },
      stdio: ["pipe", "pipe", "inherit"],
    });

    let buffer = Buffer.alloc(0);
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error("no response within 10s"));
    }, 10000);

    child.stdout.on("data", (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      for (;;) {
        const sep = buffer.indexOf("\r\n\r\n");
        if (sep === -1) return;
        const header = buffer.subarray(0, sep).toString();
        const length = Number(/Content-Length: (\d+)/i.exec(header)[1]);
        if (buffer.length < sep + 4 + length) return;
        const message = JSON.parse(
          buffer.subarray(sep + 4, sep + 4 + length).toString(),
        );
        buffer = buffer.subarray(sep + 4 + length);
        if (message.id === 2) {
          clearTimeout(timer);
          child.kill();
          resolve(message.result.items.map((item) => item.label));
        }
      }
    });
    child.on("error", reject);

    const send = (object) => {
      const body = Buffer.from(JSON.stringify(object), "utf8");
      child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
      child.stdin.write(body);
    };

    send({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: { workspaceFolders: [{ uri: `file:///${root}`, name: "test" }] },
    });
    send({
      jsonrpc: "2.0",
      id: 2,
      method: "textDocument/completion",
      params: {
        textDocument: { uri: `file:///${root}/${callerPath}` },
        position: { line: 0, character: 0 },
      },
    });
  });

let failures = 0;
const check = (name, actual, expected) => {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  console.log(`${ok ? "ok  " : "FAIL"} ${name}`);
  if (!ok) {
    console.log("     expected:", expected);
    console.log("     actual  :", actual);
    failures += 1;
  }
};

const configured = await makeRoot(RULES);
const bare = await makeRoot(null);

check(
  "from Server/, the Client/ suggestion is dropped",
  await complete(configured, "features/Server/main.server.luau"),
  ["serverStore", "sharedConfig", "plainLocal"],
);

check(
  "from Client/, the Server/ suggestion is dropped",
  await complete(configured, "features/Client/main.client.luau"),
  ["clientWidget", "sharedConfig", "plainLocal"],
);

check(
  "from Shared/, nothing is dropped",
  await complete(configured, "features/Shared/draw.luau"),
  ["clientWidget", "serverStore", "sharedConfig", "plainLocal"],
);

check(
  "with no rules file, the proxy is a passthrough",
  await complete(bare, "features/Server/main.server.luau"),
  ["clientWidget", "serverStore", "sharedConfig", "plainLocal"],
);

console.log(failures === 0 ? "\nall cases pass" : `\n${failures} failure(s)`);
process.exit(failures === 0 ? 0 : 1);
