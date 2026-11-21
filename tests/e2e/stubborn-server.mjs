// A stand-in server that refuses to die, launched through a stand-in shim.
//
// It reproduces the two things that kept luau-lsp alive after the editor was
// closed: it ignores EOF on its input as well as `exit`, and the process the
// proxy spawned is not the one doing the work. Here the proxy's direct child
// launches a grandchild, the way mise and rokit shims launch the real server.
//
//   node stubborn-server.mjs <pid-file>
//
// Writes "<own pid> <grandchild pid>" to the file once both are running.
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
import { argv, execPath, pid, stdin } from "node:process";

const grandchild = spawn(execPath, ["-e", "setInterval(() => {}, 1000)"], {
  stdio: "ignore",
});

grandchild.on("spawn", () => {
  writeFileSync(argv[2], `${pid} ${grandchild.pid}`);
});

// Read and discard everything, `exit` included, and stay up after EOF.
stdin.on("data", () => {});
stdin.on("end", () => {});
setInterval(() => {}, 1000);
