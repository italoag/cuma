// CUMA's stdio tunnel, run inside an OpenSandbox sandbox.
//
// OpenSandbox's execd runs commands and streams their output, but has no way
// to write to a running process's stdin, which an ACP agent needs. This
// tunnel starts the agent and exposes its stdio over plain HTTP, so it works
// through OpenSandbox's endpoint proxy:
//
//   GET  /stdout   server-sent events: `data` (base64 stdout), then `exit`
//   POST /stdin    a chunk of stdin; `x-seq` orders chunks, `x-eof: 1` closes
//   GET  /health   200 once listening
//
// The agent starts when the first /stdout reader connects, so no output is
// lost, and the tunnel exits a moment after the agent does.
//
// Usage: node cuma-tunnel.mjs <port> -- <command> [args...]

import { createServer } from "node:http";
import { spawn } from "node:child_process";

const [port, separator, command, ...args] = process.argv.slice(2);
if (!port || separator !== "--" || !command) {
  console.error("usage: node cuma-tunnel.mjs <port> -- <command> [args...]");
  process.exit(2);
}

let child = null;
let readers = new Set();
let backlog = [];
let nextSeq = 0;
const waiting = new Map();

function emit(event, data) {
  const message = `event: ${event}\ndata: ${data}\n\n`;
  if (readers.size === 0) backlog.push(message);
  for (const reader of readers) reader.write(message);
}

function start() {
  child = spawn(command, args, { stdio: ["pipe", "pipe", "inherit"] });
  child.stdout.on("data", (chunk) => emit("data", chunk.toString("base64")));
  child.on("error", (error) => {
    process.stderr.write(`cuma-tunnel: ${error.message}\n`);
    emit("exit", "127");
  });
  child.on("exit", (code, signal) => {
    emit("exit", String(code ?? (signal ? 128 : 1)));
    setTimeout(() => process.exit(0), 1000);
  });
}

// Chunks may arrive out of order through a proxy; apply them by sequence.
function apply(seq, body, eof) {
  waiting.set(seq, { body, eof });
  while (waiting.has(nextSeq)) {
    const { body: chunk, eof: last } = waiting.get(nextSeq);
    waiting.delete(nextSeq);
    nextSeq += 1;
    if (chunk.length > 0) child?.stdin.write(chunk);
    if (last) child?.stdin.end();
  }
}

createServer((req, res) => {
  if (req.method === "GET" && req.url === "/health") {
    res.writeHead(200).end("ok");
  } else if (req.method === "GET" && req.url === "/stdout") {
    res.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      "x-accel-buffering": "no",
    });
    // Headers are otherwise held until the first output, and the agent may
    // say nothing until it is spoken to: send them, and a comment for
    // proxies that wait for a body, now.
    res.flushHeaders();
    res.write(": attached\n\n");
    readers.add(res);
    for (const message of backlog) res.write(message);
    backlog = [];
    req.on("close", () => readers.delete(res));
    if (!child) start();
  } else if (req.method === "POST" && req.url === "/stdin") {
    const parts = [];
    req.on("data", (part) => parts.push(part));
    req.on("end", () => {
      const seq = Number(req.headers["x-seq"] ?? nextSeq);
      apply(seq, Buffer.concat(parts), req.headers["x-eof"] === "1");
      res.writeHead(200).end();
    });
  } else {
    res.writeHead(404).end();
  }
}).listen(Number(port), "0.0.0.0");
