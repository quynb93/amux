export type SessionFormat = "claude" | "codex" | "gemini";

/**
 * Describes how a CLI keeps its home directory, so amux can give each account
 * its own credentials while sharing everything else.
 *
 * Built-in providers are defined in ./builtin.ts; extra providers can be
 * declared as plain JSON in ~/.amux/config.json under "providers".
 */
export interface ProviderSpec {
  id: string;
  label: string;
  /** Executable name or absolute path. */
  bin: string;
  /** The real, shared home directory, e.g. ~/.claude. */
  sharedHome: string;
  /** Env var that relocates the CLI's home, e.g. CLAUDE_CONFIG_DIR. */
  homeEnv: string;
  /**
   * "home": homeEnv points at the home dir itself (CLAUDE_CONFIG_DIR, CODEX_HOME).
   * "parent": homeEnv points at a dir that contains the home dir (GEMINI_CLI_HOME → <dir>/.gemini).
   */
  homeEnvPoints: "home" | "parent";
  /**
   * Entries in the home dir that belong to one account and are never shared.
   * Exact names, or a trailing "*" for a prefix match.
   */
  privateEntries: string[];
  /** Dirs created in the shared home up front so the CLI never makes a private copy. */
  sharedDirs: string[];
  /** Args that sign in, run inside the account's overlay. Omit to sign in on first run. */
  loginArgs?: string[];
  /** Args that print auth status. */
  statusArgs?: string[];
  /** Args to resume a session by id; "{id}" is substituted. */
  resumeArgs?: string[];
  /** Args to start an interactive session with an initial prompt; "{prompt}" is substituted. */
  promptArgs?: string[];
  /** Env var used when an account authenticates with a stored token instead of a login. */
  tokenEnv?: string;
  /**
   * Env var that points the CLI at a different API endpoint (a gateway, Bedrock/Vertex
   * proxy, or self-hosted relay), e.g. ANTHROPIC_BASE_URL. Set per account with a URL.
   * This only redirects where requests go; it never changes or overrides org-managed policy.
   */
  baseUrlEnv?: string;
  /** Env vars set when an account has privacy mode on: the CLI's own documented opt-outs. */
  privacyEnv?: Record<string, string>;
  /** Session file format, for listing and handoff. */
  sessions?: SessionFormat;
}
