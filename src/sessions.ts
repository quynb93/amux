import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import type { ProviderSpec } from "./providers/types.js";

export interface SessionInfo {
  provider: string;
  id: string;
  cwd?: string;
  title: string;
  updatedAt: Date;
  file: string;
}

export interface Turn {
  role: "user" | "assistant";
  text: string;
}

const HEAD_BYTES = 256 * 1024;

function readHead(file: string): string {
  const fd = fs.openSync(file, "r");
  try {
    const buf = Buffer.alloc(HEAD_BYTES);
    const n = fs.readSync(fd, buf, 0, HEAD_BYTES, 0);
    return buf.subarray(0, n).toString("utf8");
  } finally {
    fs.closeSync(fd);
  }
}

function* jsonLines(text: string): Generator<any> {
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    try {
      yield JSON.parse(line);
    } catch {
      // Partial last line from a head read, or a corrupt line.
    }
  }
}

function walk(dir: string, match: (name: string) => boolean, out: string[] = []): string[] {
  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const e of entries) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, match, out);
    else if (match(e.name)) out.push(p);
  }
  return out;
}

const oneLine = (s: string, max = 80) => {
  const t = s.replace(/\s+/g, " ").trim();
  return t.length > max ? t.slice(0, max - 1) + "…" : t;
};

/** Skips injected context (AGENTS.md, environment blocks, command wrappers) when picking a title. */
const isRealPrompt = (t: string) => t.trim() !== "" && !/^\s*(<|# AGENTS\.md|Caveat:)/.test(t);

function textOf(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((c) => (typeof c === "string" ? c : c?.type === "text" || c?.type === "input_text" || c?.type === "output_text" ? c.text : ""))
      .filter(Boolean)
      .join("\n");
  }
  return "";
}

// ---- Claude Code: <home>/projects/<cwd-slug>/<session-id>.jsonl

function claudeTurns(text: string): { cwd?: string; turns: Turn[] } {
  let cwd: string | undefined;
  const turns: Turn[] = [];
  for (const rec of jsonLines(text)) {
    cwd ??= rec.cwd;
    if (rec.isSidechain || rec.isMeta) continue;
    if (rec.type === "user" && rec.message?.role === "user") {
      const t = textOf(rec.message.content);
      if (isRealPrompt(t)) turns.push({ role: "user", text: t });
    } else if (rec.type === "assistant") {
      const t = textOf(rec.message?.content);
      if (t) turns.push({ role: "assistant", text: t });
    }
  }
  return { cwd, turns };
}

function listClaude(spec: ProviderSpec): SessionInfo[] {
  return walk(path.join(spec.sharedHome, "projects"), (n) => n.endsWith(".jsonl")).flatMap((file) => {
    if (file.includes(`${path.sep}subagents${path.sep}`)) return [];
    const { cwd, turns } = claudeTurns(readHead(file));
    const first = turns.find((t) => t.role === "user");
    if (!first) return [];
    return [{ provider: spec.id, id: path.basename(file, ".jsonl"), cwd, title: oneLine(first.text), updatedAt: fs.statSync(file).mtime, file }];
  });
}

// ---- Codex: <home>/sessions/YYYY/MM/DD/rollout-*.jsonl

function codexTurns(text: string): { id?: string; cwd?: string; turns: Turn[] } {
  let id: string | undefined;
  let cwd: string | undefined;
  const turns: Turn[] = [];
  for (const rec of jsonLines(text)) {
    const p = rec.payload ?? {};
    if (rec.type === "session_meta") {
      id ??= p.id ?? p.session_id;
      cwd ??= p.cwd;
    } else if (rec.type === "response_item" && p.type === "message" && (p.role === "user" || p.role === "assistant")) {
      const t = textOf(p.content);
      const prev = turns[turns.length - 1];
      // Codex can record the same prompt twice in a row (raw and after context injection).
      if (prev?.role === p.role && prev.text === t) continue;
      if (p.role === "assistant" ? t : isRealPrompt(t)) turns.push({ role: p.role, text: t });
    }
  }
  return { id, cwd, turns };
}

function listCodex(spec: ProviderSpec): SessionInfo[] {
  return walk(path.join(spec.sharedHome, "sessions"), (n) => n.startsWith("rollout-") && n.endsWith(".jsonl")).flatMap((file) => {
    const { id, cwd, turns } = codexTurns(readHead(file));
    const first = turns.find((t) => t.role === "user");
    if (!id || !first) return [];
    return [{ provider: spec.id, id, cwd, title: oneLine(first.text), updatedAt: fs.statSync(file).mtime, file }];
  });
}

// ---- Gemini: <home>/tmp/<project-hash-or-name>/chats/session-*.json

function geminiProjectDirs(spec: ProviderSpec): Map<string, string> {
  const map = new Map<string, string>();
  try {
    const projects = JSON.parse(fs.readFileSync(path.join(spec.sharedHome, "projects.json"), "utf8")).projects ?? {};
    for (const [cwd, short] of Object.entries<string>(projects)) {
      map.set(short, cwd);
      map.set(crypto.createHash("sha256").update(cwd).digest("hex"), cwd);
    }
  } catch {
    // No project registry; cwd stays unknown.
  }
  return map;
}

function geminiTurns(data: any): Turn[] {
  return (data.messages ?? []).flatMap((m: any): Turn[] => {
    const t = textOf(m.content);
    if (m.type === "user" && isRealPrompt(t)) return [{ role: "user", text: t }];
    if (m.type === "gemini" && t) return [{ role: "assistant", text: t }];
    return [];
  });
}

function listGemini(spec: ProviderSpec): SessionInfo[] {
  const dirs = geminiProjectDirs(spec);
  return walk(path.join(spec.sharedHome, "tmp"), (n) => n.startsWith("session-") && n.endsWith(".json")).flatMap((file) => {
    try {
      const data = JSON.parse(fs.readFileSync(file, "utf8"));
      const first = geminiTurns(data).find((t) => t.role === "user");
      if (!data.sessionId || !first) return [];
      const projectKey = path.basename(path.dirname(path.dirname(file)));
      return [
        {
          provider: spec.id,
          id: data.sessionId,
          cwd: dirs.get(projectKey),
          title: oneLine(first.text),
          updatedAt: new Date(data.lastUpdated ?? fs.statSync(file).mtime),
          file,
        },
      ];
    } catch {
      return [];
    }
  });
}

export function listSessions(spec: ProviderSpec): SessionInfo[] {
  switch (spec.sessions) {
    case "claude":
      return listClaude(spec);
    case "codex":
      return listCodex(spec);
    case "gemini":
      return listGemini(spec);
    default:
      return [];
  }
}

export function readTurns(s: SessionInfo, format: ProviderSpec["sessions"]): Turn[] {
  if (format === "gemini") return geminiTurns(JSON.parse(fs.readFileSync(s.file, "utf8")));
  const text = fs.readFileSync(s.file, "utf8");
  return format === "codex" ? codexTurns(text).turns : claudeTurns(text).turns;
}

/**
 * Builds a Markdown handoff for continuing a session in another provider:
 * the original request, then the most recent turns that fit in the budget.
 */
export function buildHandoff(s: SessionInfo, turns: Turn[], budget = 60_000): string {
  const head = [
    `# Handoff from ${s.provider} session ${s.id}`,
    "",
    `- Working directory: ${s.cwd ?? "unknown"}`,
    `- Last active: ${s.updatedAt.toISOString()}`,
    `- Source transcript: ${s.file}`,
    "",
    "You are continuing work started in another agent session. Read the transcript below,",
    "check the current state of the working directory, then continue where it left off.",
    "",
  ];
  const fmt = (t: Turn) => `## ${t.role === "user" ? "User" : "Assistant"}\n\n${t.text.trim()}\n`;
  const first = turns.find((t) => t.role === "user");
  const body: string[] = [];
  let used = 0;
  for (let i = turns.length - 1; i >= 0; i--) {
    if (turns[i] === first) break;
    const block = fmt(turns[i]);
    if (used + block.length > budget) {
      // Keep the tail of an oversized turn rather than dropping it entirely.
      const room = budget - used;
      if (room > 1000) body.unshift(`## ${turns[i].role === "user" ? "User" : "Assistant"}\n\n…${turns[i].text.trim().slice(-room)}\n`);
      body.unshift("_…earlier turns omitted…_\n");
      break;
    }
    body.unshift(block);
    used += block.length;
  }
  return [...head, "# Transcript", "", ...(first ? [fmt(first)] : []), ...body].join("\n");
}
