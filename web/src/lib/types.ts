// Types for the daemon's JSON contract. Only fields the daemon actually
// sends are declared; anything optional on the wire is optional here.

export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error' | 'unknown'

/** A live runtime log line from `/events` or `/api/records`. */
export interface LogRecord {
  seq: number
  timestamp: string
  source: string
  level: LogLevel
  payload: string
}

export type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'

export type Provenance =
  | 'agent_reported'
  | 'filesystem_observed'
  | 'runtime_observed'
  | 'system_observed'

export type EventStatus = 'started' | 'succeeded' | 'failed' | 'cancelled'

export interface RedactionInfo {
  rules?: string[]
  replacements?: number
  payload_dropped?: boolean
  truncated?: boolean
}

/** A persisted event (`/api/v1/events`). */
export interface StoredEvent {
  seq: number
  event_id: string
  stable_id?: boolean
  schema_version: number
  timestamp: string
  observed_at: string
  event_type: string
  provenance: Provenance
  source_type: string
  source_id: string
  project_id?: string
  session_id?: string
  operation_id?: string
  parent_operation_id?: string
  agent_name?: string
  severity: LogLevel
  status?: EventStatus
  duration_ms?: number
  path?: string
  message?: string
  attributes?: Record<string, unknown>
  payload?: string
  redaction?: RedactionInfo
}

export interface GapView {
  reason: string
  count: number | null
  first_at: string | null
  last_at: string | null
  from_seq: number
  to_seq: number
}

export interface Completeness {
  complete: boolean
  reasons: string[]
  gaps: GapView[]
  gaps_total: number
  pending_loss_unrecorded: number
  retention_truncated: boolean
  history_starts_at: string | null
  oldest_retained_seq: number | null
  evicted_through_seq: number
  payload_expired_through_seq: number
}

export interface EventPage {
  events: StoredEvent[]
  next_cursor: number | null
  has_more: boolean
  scanned_from_seq: number | null
  scanned_to_seq: number | null
  scan_budget_exhausted: boolean
  newest_seq: number | null
  completeness: Completeness
}

export type CoverageLevel = 'direct' | 'partial' | 'none'

export interface Capability {
  activity: string
  level: CoverageLevel
  note: string
}

export type SessionStatus = 'active' | 'ended' | 'unterminated'
export type HistoryCoverage = 'complete' | 'partial' | 'unknown'

export interface SessionView {
  session_id: string
  agent_name: string
  source_session_id: string
  project_id: string | null
  working_directory: string | null
  status: SessionStatus
  started_at: string | null
  ended_at: string | null
  end_reason: string | null
  start_source: string | null
  restarts: number
  first_event_at: string | null
  last_event_at: string | null
  model: string | null
  agent_version: string | null
  git: {
    root: string | null
    worktree: string | null
    branch: string | null
    head_at_start: string | null
    head_at_end: string | null
    inferred_head_changed: boolean | null
  }
  counts: {
    events: number
    tool_calls: number
    tool_failures: number
    commands: number
    command_failures: number
    file_events: number
    subagents: number
    turns: number
    failed_operations: number
    tokens_total: number | null
  }
  first_seq: number
  last_seq: number
  coverage: {
    history: HistoryCoverage
    reasons: string[]
    start_observed: boolean
    end_observed: boolean
    retention_truncated: boolean
    gaps: GapView[]
    integration_limits: Capability[]
  }
}

export interface SessionPage {
  sessions: SessionView[]
  evicted_through_seq: number
  has_more: boolean
  next_cursor: string | null
}

export interface NameCount { name: string; count: number }

export interface SessionDetail extends SessionView {
  tools: NameCount[]
  programs: NameCount[]
  failures: {
    seq: number
    event_type: string
    timestamp: string | null
    message: string | null
    exit_code: number | null
  }[]
  files: { path: string; last_event_type: string; events: number }[]
}

export interface ActivityTotals {
  tool_calls: number
  tool_failures: number
  commands: number
  command_failures: number
  turn_failures: number
  subagents: number
  file_events: number
  file_reads: number
  file_changes: number
  failures: number
}

export interface AgentSummary extends ActivityTotals {
  agent: string
  sessions: number
  active_sessions: number
  latest_session_id: string | null
  latest_session_status: SessionStatus | null
  latest_activity_at: string | null
}

export interface StatsBucket {
  start: string | null
  ingested: number
  runtime_errors: number
  runtime_warnings: number
  agent_events: number
  failures: number
}

export interface Stats {
  since: string | null
  until: string | null
  sessions: { total: number; active: number; ended: number; unterminated: number; capped: boolean }
  activity: ActivityTotals
  agents: AgentSummary[]
  events_ingested: number
  runtime_errors: number
  runtime_warnings: number
  bucket_secs: number
  buckets: StatsBucket[]
  retention_truncated: boolean
  history_starts_at: string | null
  gaps_in_range: number
}

export interface FileActivity {
  path: string
  project_id: string | null
  events: number
  reads: number
  created: number
  modified: number
  deleted: number
  renamed: number
  agents: string[]
  sessions: number
  last_seq: number
  last_event_type: string
  last_at: string | null
  provenance: 'agent_reported'
}

export interface FilesPage {
  files: FileActivity[]
  total_paths: number
  since: string | null
  until: string | null
}

export type InstallState = 'configured' | 'incomplete' | 'not_installed' | 'unknown'

export interface InstallRecord {
  config_path: string
  installed_at: string
  tailflow_version: string
  binary: string
  binary_exists: boolean
  hooks_installed: string[]
  hooks_missing: string[]
  hooks_duplicated: string[]
  hooks_disabled: boolean
  missing_binaries: string[]
  error: string | null
}

export interface AgentsStatus {
  ingest: {
    enabled: boolean
    spool_dir: string | null
    passes: number
    files_ingested: number
    events_submitted: number
    events_dropped: number
    excluded: number
    rejected: number
    corrupt_files: number
    overflow: number
    last_pass_at: string | null
    last_error: string | null
    last_error_at: string | null
    disabled_reason?: string | null
  }
  spool: {
    pending_files: number
    pending_capped: boolean
    oldest_pending_at: string | null
    claimed_files: number
    full: boolean
    overflow?: { count?: number }
  } | null
  agents: {
    agent: string
    name: string
    capabilities: Capability[]
    installation?: {
      state: InstallState
      records: InstallRecord[]
      policy_block: string | null
      warnings: string[]
    }
  }[]
}

export interface StorageStatus {
  enabled: boolean
  state: string
  kind?: string
  error?: string
  path?: string
  capture_mode?: string
  capture_enabled?: boolean
  full_text_search?: boolean
  recovered_unclean_runs?: number
  db?: {
    events: number
    oldest_event_at: string | null
    newest_event_at: string | null
    file_bytes: number
    wal_bytes: number
    used_bytes: number
    gaps: number
    evicted_through_seq: number
  }
  retention?: {
    retention_days: number | null
    payload_retention_days: number
    max_size_mb: number
    last_cleanup_at: string | null
  }
  health?: {
    events_persisted: number
    events_dropped_total: number
    events_dropped: Record<string, number>
    events_excluded: number
    storage_errors: number
    ingestion_queue_depth: number
    loss_detected: boolean
    pending_loss_unrecorded: number
    last_error: string | null
    last_commit_at: string | null
  }
  ingest?: {
    records_received: number
    records_dropped: number
    records_redacted: number
    sse_client_lag_records: number
  }
}

export interface Collector {
  name: string
  kind: string
  collector_status: string
  events_received: number
  last_event_at: string | null
  detail?: string
}

export interface Health {
  ok: boolean
  version: string
  instance_id?: string
  buffered: number
  cursor: number
  ingest_dropped: number
}

// ── M4: file evidence, Git changes, observation status ───────────────────────

export type EvidenceStatus = 'matched_observation' | 'reported_only' | 'unverified' | 'filesystem_observed' | 'ambiguous'

export interface EvidenceRef {
  seq: number
  at: string
  event_type: string
  session_id?: string | null
  agent_name?: string | null
}

export interface FileEvidence {
  path: string
  at: string
  status: EvidenceStatus
  explanation: string
  reported: EvidenceRef[]
  observed: EvidenceRef[]
}

export interface CoverageView {
  root: string
  started_at: string
  stopped_at: string | null
  backend: string
  watched_dirs: number
  unwatched_dirs: number
}

export interface FileEvidencePage {
  items: FileEvidence[]
  counts: Record<EvidenceStatus, number>
  window_ms: number
  coverage: CoverageView[]
  loss: { reason: string; count: number | null; first_at: string; last_at: string }[]
  truncated: boolean
}

export interface GitChange {
  path: string
  status: string
  staged: boolean
  unstaged: boolean
  from?: string
  size?: number
  lines_added?: number
  lines_deleted?: number
}

export interface SnapshotRef {
  seq: number
  at: string
  trigger: string | null
  head: string | null
  branch: string | null
  changes_total: number
  complete: boolean
}

export type ChangeEvidence = 'reported_by_this_session' | 'reported_by_other_sessions' | 'reported_by_several_sessions' | 'unreported'

export interface SessionChanges {
  session_id: string
  available: boolean
  reason?: string
  repository?: string
  baseline?: SnapshotRef
  after?: SnapshotRef
  head_moved?: boolean
  complete?: boolean
  changes?: {
    path: string
    kind: 'appeared' | 'changed' | 'cleared'
    before?: GitChange
    after?: GitChange
    evidence: ChangeEvidence
    reported_by_this_session: boolean
    other_sessions: string[]
    observations: number
  }[]
  pre_existing?: string[]
  concurrent_sessions?: { session_id: string; agent: string; status: string }[]
  limitations?: string[]
}

export interface ObserveStatus {
  enabled: boolean
  disabled_reason?: string
  roots: {
    root: string
    state: string
    backend?: string
    detail?: string
    observations: number
    excluded: number
    overflowed: number
    rescans: number
    watched_dirs: number
    unwatched_dirs: number
    pending_paths: number
    git_root?: string
  }[]
  git_enabled: boolean
  git_binary?: string
  repos: { worktree_root: string; snapshots: number; last_snapshot_at?: string; last_took_ms?: number; last_error?: string; head?: string; branch?: string }[]
  errors: string[]
  coalesce_ms: number
  git_min_interval_secs: number
}
