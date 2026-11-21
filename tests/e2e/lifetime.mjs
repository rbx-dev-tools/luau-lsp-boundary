// End-to-end test of the proxy's lifetime: when the editor goes away, the
// server and everything it launched go with it.
//
// The server is a stand-in that ignores EOF and `exit`, and that is reached
// through a shim, like luau-lsp behind mise or rokit (stubborn-server.mjs).
//
//   node tests/e2e/lifetime.mjs [path/to/luau-lsp-boundary]

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { existsSync, readFileSync } from "node:fs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const stubborn = path.join(here, "stubborn-server.mjs");
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

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    // EPERM: it exists, it is just not ours to signal.
    return error.code === "EPERM";
  }
};

/** Polls `condition` every 100 ms; true if it held within `ms`. */
const within = async (ms, condition) => {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (condition()) return true;
    await sleep(100);
  }
  return condition();
};

/**
 * Starts the proxy in front of the stubborn server and completes the
 * handshake. Resolves once the server and its grandchild are both running.
 */
const start = async () => {
  const dir = await mkdtemp(path.join(tmpdir(), "boundary-life-"));
  const pidFile = path.join(dir, "pids");
  const root = dir.replaceAll("\\", "/");

  const proxy = spawn(binary, [stubborn, pidFile], {
    env: { ...process.env, LUAU_LSP_BOUNDARY_SERVER: "node" },
    stdio: ["pipe", "ignore", "inherit"],
  });
  const exited = new Promise((resolve) => proxy.on("exit", resolve));

  // The proxy launches the server only once `initialize` has arrived.
  const body = Buffer.from(
    JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: { workspaceFolders: [{ uri: `file:///${root}`, name: "test" }] },
    }),
    "utf8",
  );
  proxy.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
  proxy.stdin.write(body);

  if (!(await within(10000, () => existsSync(pidFile) && readFileSync(pidFile, "utf8")))) {
    proxy.kill();
    throw new Error("the stand-in server did not start within 10s");
  }
  const pids = readFileSync(pidFile, "utf8").trim().split(" ").map(Number);
  return { proxy, exited, pids };
};

let failures = 0;
const check = (name, ok, detail) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${name}`);
  if (!ok) {
    console.log(`     ${detail}`);
    failures += 1;
  }
};

const cleanup = (pids) => {
  for (const pid of pids) {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // Already gone, which is the point.
    }
  }
};

// 1. The editor closes its pipe without `shutdown` or `exit`, as a closed or
//    crashed editor does.
{
  const { proxy, exited, pids } = await start();
  proxy.stdin.end();

  const proxyExited = await Promise.race([
    exited.then(() => true),
    sleep(10000).then(() => false),
  ]);
  check(
    "editor closes its pipe: the proxy exits",
    proxyExited,
    "the proxy was still running 10s after its input closed",
  );

  const treeGone = await within(5000, () => pids.every((pid) => !alive(pid)));
  check(
    "editor closes its pipe: server and grandchild exit",
    treeGone,
    `still running: ${pids.filter(alive).join(", ")}`,
  );
  if (!proxyExited) proxy.kill("SIGKILL");
  cleanup(pids);
}

// 2. The proxy itself is killed (SIGTERM on Unix, TerminateProcess on
//    Windows), as an editor does to a server it gives up on.
{
  const { proxy, pids } = await start();
  proxy.kill();

  const treeGone = await within(5000, () => pids.every((pid) => !alive(pid)));
  check(
    "proxy killed: server and grandchild exit",
    treeGone,
    `still running: ${pids.filter(alive).join(", ")}`,
  );
  cleanup(pids);
}

console.log(failures === 0 ? "\nall cases pass" : `\n${failures} failure(s)`);
process.exit(failures === 0 ? 0 : 1);
