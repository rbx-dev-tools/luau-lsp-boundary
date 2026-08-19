// A stand-in language server: answers one canned completion list.
//
// The items mirror what luau-lsp's `createSuggestRequire` produces, including
// one ordinary completion under a /Client/ path that must never be filtered.
import { stdin, stdout } from "node:process";

let buf = Buffer.alloc(0);

const send = (obj) => {
  const body = Buffer.from(JSON.stringify(obj), "utf8");
  stdout.write(`Content-Length: ${body.length}\r\n\r\n`);
  stdout.write(body);
};

const item = (label, kind, detail, docPath, withEdits = true) => ({
  label,
  kind,
  detail,
  documentation: {
    kind: "markdown",
    value: "```luau\nlocal x = require(...)\n```\n\n" + docPath,
  },
  insertText: label,
  ...(withEdits
    ? {
        additionalTextEdits: [
          {
            range: { start: { line: 0, character: 0 }, end: { line: 0, character: 0 } },
            newText: `local ${label} = require(...)\n`,
          },
        ],
      }
    : {}),
});

stdin.on("data", (chunk) => {
  buf = Buffer.concat([buf, chunk]);
  for (;;) {
    const sep = buf.indexOf("\r\n\r\n");
    if (sep === -1) return;
    const header = buf.subarray(0, sep).toString();
    const len = Number(/Content-Length: (\d+)/i.exec(header)[1]);
    if (buf.length < sep + 4 + len) return;
    const msg = JSON.parse(buf.subarray(sep + 4, sep + 4 + len).toString());
    buf = buf.subarray(sep + 4 + len);

    if (msg.method === "initialize") {
      send({ jsonrpc: "2.0", id: msg.id, result: { capabilities: {} } });
    } else if (msg.method === "textDocument/completion") {
      send({
        jsonrpc: "2.0",
        id: msg.id,
        result: {
          isIncomplete: false,
          items: [
            item(
              "clientWidget",
              9,
              "ReplicatedStorage.Features.Example.Client.clientWidget",
              "game/ReplicatedStorage/Features/Example/Client/clientWidget",
            ),
            item(
              "serverStore",
              9,
              "ServerScriptService.Features.Example.Server.serverStore",
              "game/ServerScriptService/Features/Example/Server/serverStore",
            ),
            item(
              "sharedConfig",
              9,
              "ReplicatedStorage.Features.Example.Shared.sharedConfig",
              "game/ReplicatedStorage/Features/Example/Shared/sharedConfig",
            ),
            // An ordinary completion, not an auto-import: never filtered,
            // even though its path contains /Client/.
            item("plainLocal", 6, "a local in Client", "somewhere/Client/x", false),
          ],
        },
      });
    }
  }
});
