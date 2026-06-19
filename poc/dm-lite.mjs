#!/usr/bin/env node
// dm-lite PoC: the sidecar seam, smallest slice.
//
// What stays in dm-lite (this file): the daimon TYPED layer.
//   - typed kinds + per-kind required-field contracts (ma8e's EntryKind is closed)
//   - the daimon:// namespace + URI grammar
//   - formatting a record into a body ma8e can store and recall
// What stays in ma8e: the STORE.
//   - `ma8e remember` persists the record; `ma8e recall` (BM25) reads it back.
//
// dm-lite never touches ma8e's internals - it shells out to the ma8e binary,
// exactly as a sidecar would call ma8e over its CLI/socket/MCP. Zero ma8e changes.
//
// Usage:
//   node dm-lite.mjs log_decision --title T --context C --rationale R [--namespace NS]
//   node dm-lite.mjs recall "<query>"

import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

// --- locate the ma8e binary (built debug, then release, then PATH) -----------
function ma8eBin() {
  if (process.env.MA8E_BIN) return process.env.MA8E_BIN; // override (stub or custom build path)
  const candidates = [
    join(homedir(), "Projects/ma8e/target/debug/ma8e"),
    join(homedir(), "Projects/ma8e/target/release/ma8e"),
  ];
  for (const c of candidates) if (existsSync(c)) return c;
  return "ma8e"; // fall back to PATH
}

// --- dm-lite's typed layer (the part ma8e does NOT have) ---------------------
// Per-kind required fields. ma8e stores one flat `kind=memory` blob; dm-lite is
// what makes a "decision" a first-class typed thing with a contract.
const KINDS = {
  decision: { required: ["title", "context", "rationale"], defaultNs: "resources/decisions" },
};

const slug = (s) =>
  String(s).toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 60);

// Build the daimon-typed record. The kind + URI + namespace are encoded into the
// FIRST line as machine-readable markers (ma8e `remember` has no kind/tags flag,
// so we fold the type into the stored text - it round-trips verbatim and BM25
// recalls on it). dm-lite owns this format; ma8e just stores the string.
function buildRecord(kind, fields) {
  const ns = fields.namespace || KINDS[kind].defaultNs;
  const uri = `daimon://${ns}/${kind}/${slug(fields.title)}`;
  const marker = `[daimon:kind=${kind}] [daimon:ns=${ns}] [daimon:uri=${uri}]`;
  const body =
    `${marker}\n` +
    `# ${fields.title}\n\n` +
    `**Context:** ${fields.context}\n\n` +
    `**Rationale:** ${fields.rationale}\n`;
  return { uri, body };
}

function validate(kind, fields) {
  const spec = KINDS[kind];
  if (!spec) throw new Error(`dm-lite: unknown kind '${kind}' (have: ${Object.keys(KINDS).join(", ")})`);
  const missing = spec.required.filter((f) => !fields[f] || !String(fields[f]).trim());
  if (missing.length) throw new Error(`dm-lite: ${kind} requires: ${missing.join(", ")} (missing: ${missing.join(", ")})`);
}

// --- the seam: forward a typed record to ma8e as the backing store -----------
function ma8eRemember(body, dedupKey) {
  const out = execFileSync(
    ma8eBin(),
    ["remember", body, "--global", "--dedup-key", dedupKey],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  );
  return out.trim();
}

function ma8eRecall(query) {
  return execFileSync(ma8eBin(), ["recall", query], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
}

// --- the dm-lite tool surface (what a sidecar MCP server would expose) -------
function logDecision(fields) {
  validate("decision", fields);
  const { uri, body } = buildRecord("decision", fields);
  ma8eRemember(body, uri);
  return uri;
}

// --- CLI ---------------------------------------------------------------------
function parseFlags(argv) {
  const f = {};
  for (let i = 0; i < argv.length; i++) {
    if (argv[i].startsWith("--")) { f[argv[i].slice(2)] = argv[i + 1]; i++; }
  }
  return f;
}

const [cmd, ...rest] = process.argv.slice(2);
try {
  if (cmd === "log_decision") {
    const uri = logDecision(parseFlags(rest));
    console.log(`dm-lite: stored typed decision -> ${uri}`);
    console.log(`         (ma8e holds the bytes; dm-lite owns the type + URI)`);
  } else if (cmd === "recall") {
    const query = rest.join(" ");
    const out = ma8eRecall(query);
    // dm-lite's read-side touch: flag which ma8e results are daimon-typed.
    const annotated = out.split("\n").map((l) =>
      /\[daimon:kind=/.test(l) ? `  >> [daimon-typed] ${l}` : l).join("\n");
    console.log(annotated);
  } else {
    console.error("usage: dm-lite.mjs log_decision --title T --context C --rationale R [--namespace NS]");
    console.error("       dm-lite.mjs recall \"<query>\"");
    process.exit(2);
  }
} catch (e) {
  console.error(String(e.message || e));
  process.exit(1);
}
