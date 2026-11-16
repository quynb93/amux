import fs from "node:fs";
import path from "node:path";
import { HOME } from "../paths.js";
import type { ProviderSpec } from "./types.js";

export const BUILTIN_PROVIDERS: ProviderSpec[] = [
  {
    id: "claude",
    label: "Claude Code",
    bin: "claude",
    sharedHome: "~/.claude",
    homeEnv: "CLAUDE_CONFIG_DIR",
    homeEnvPoints: "home",
    // On macOS the OAuth credentials live in the Keychain under a service name
    // derived from CLAUDE_CONFIG_DIR, so each overlay dir gets its own login
    // automatically. .claude.json holds the signed-in account and per-account state.
    privateEntries: [
      ".claude.json",
      ".claude.json.backup*",
      ".credentials.json",
      ".last-update-result.json",
      ".last-cleanup",
      "policy-limits.json*",
      // Includes its .signature/.synced-at sidecars: they must match this account's copy.
      "remote-settings.json*",
      "stats-cache.json",
      "statsig",
      "telemetry",
      "daemon",
      "backups",
    ],
    sharedDirs: ["projects", "skills", "plugins", "agents", "commands", "hooks", "todos", "file-history", "sessions"],
    loginArgs: ["auth", "login"],
    statusArgs: ["auth", "status"],
    resumeArgs: ["--resume", "{id}"],
    promptArgs: ["{prompt}"],
    tokenEnv: "CLAUDE_CODE_OAUTH_TOKEN",
    baseUrlEnv: "ANTHROPIC_BASE_URL",
    // Documented user opt-outs. CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC also turns
    // off auto-update and /bug. Managed (enterprise) settings still take precedence.
    privacyEnv: {
      DISABLE_TELEMETRY: "1",
      DISABLE_ERROR_REPORTING: "1",
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
    },
    sessions: "claude",
  },
  {
    id: "codex",
    label: "Codex CLI",
    bin: "codex",
    sharedHome: "~/.codex",
    homeEnv: "CODEX_HOME",
    homeEnvPoints: "home",
    privateEntries: ["auth.json"],
    sharedDirs: ["sessions", "skills", "prompts", "plugins", "memories", "rules"],
    loginArgs: ["login"],
    statusArgs: ["login", "status"],
    resumeArgs: ["resume", "{id}"],
    promptArgs: ["{prompt}"],
    tokenEnv: "OPENAI_API_KEY",
    baseUrlEnv: "OPENAI_BASE_URL",
    sessions: "codex",
  },
  {
    id: "gemini",
    label: "Gemini CLI",
    bin: "gemini",
    sharedHome: "~/.gemini",
    homeEnv: "GEMINI_CLI_HOME",
    homeEnvPoints: "parent",
    privateEntries: ["oauth_creds.json", "google_accounts.json"],
    sharedDirs: ["tmp", "history", "extensions", "commands"],
    // Gemini has no separate login command; it asks on first interactive run.
    resumeArgs: ["--resume", "{id}"],
    promptArgs: ["-i", "{prompt}"],
    tokenEnv: "GEMINI_API_KEY",
    sessions: "gemini",
  },
];

/**
 * Called once when an account overlay is created, before first launch.
 * Seeds per-account files that would otherwise force a fresh onboarding.
 */
export function seedOverlay(spec: ProviderSpec, sharedHome: string, overlayHome: string): void {
  if (spec.id !== "claude") return;
  // Copy settings like onboarding state, project trust and user-scope MCP
  // servers, but drop the signed-in account so the new one logs in cleanly.
  const candidates = [path.join(sharedHome, ".claude.json"), path.join(HOME, ".claude.json")];
  const src = candidates.find((p) => fs.existsSync(p));
  const dest = path.join(overlayHome, ".claude.json");
  if (!src || fs.existsSync(dest)) return;
  try {
    const data = JSON.parse(fs.readFileSync(src, "utf8"));
    for (const key of ["oauthAccount", "primaryApiKey", "customApiKeyResponses", "userID", "s1mAccessCache"]) {
      delete data[key];
    }
    fs.writeFileSync(dest, JSON.stringify(data, null, 2), { mode: 0o600 });
  } catch {
    // Unreadable source: let Claude Code create its own.
  }
}
