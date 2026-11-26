// How an event is described to a person. The rule throughout: say what the
// source reported, never more. Agent file events are "reported", a missing
// outcome is "not reported" (never "succeeded"), and nothing here infers
// causation between events.

import type { StoredEvent, SessionStatus, HistoryCoverage, CoverageLevel } from './types.ts'

export type Category =
  | 'session' | 'tool' | 'command' | 'file' | 'subagent' | 'turn'
  | 'prompt' | 'model' | 'runtime' | 'system' | 'git' | 'other'

export type Tone = 'ok' | 'warn' | 'error' | 'info' | 'muted' | 'accent'

const AGENT_NAMES: Record<string, string> = {
  'claude-code': 'Claude Code',
  codex: 'Codex',
}

export function agentLabel(name: string | null | undefined): string {
  if (!name) return 'Runtime'
  return AGENT_NAMES[name] ?? name
}

export function category(type: string): Category {
  if (type.startsWith('agent.session.')) return 'session'
  if (type.startsWith('agent.tool.')) return 'tool'
  if (type.startsWith('agent.subagent.')) return 'subagent'
  if (type.startsWith('agent.turn.')) return 'turn'
  if (type.startsWith('agent.prompt.')) return 'prompt'
  if (type.startsWith('agent.model.')) return 'model'
  if (type.startsWith('command.')) return 'command'
  if (type.startsWith('file.')) return 'file'
  if (type.startsWith('runtime.')) return 'runtime'
  if (type.startsWith('system.')) return 'system'
  if (type.startsWith('git.')) return 'git'
  return 'other'
}

function attr(e: StoredEvent, key: string): unknown {
  return e.attributes ? e.attributes[key] : undefined
}

export function attrString(e: StoredEvent, key: string): string | undefined {
  const v = attr(e, key)
  return typeof v === 'string' ? v : typeof v === 'number' ? String(v) : undefined
}

export function exitCode(e: StoredEvent): number | undefined {
  const v = attr(e, 'command.exit_code')
  return typeof v === 'number' ? v : undefined
}

/** A path relative to its project when it lies inside it (full path otherwise). */
export function displayPath(path: string, project: string | null | undefined): string {
  if (!project) return path
  const root = project.endsWith('/') ? project : `${project}/`
  return path.startsWith(root) ? path.slice(root.length) : path
}

const FILE_VERB: Record<string, string> = {
  'file.read': 'Read',
  'file.created': 'Created',
  'file.modified': 'Edited',
  'file.deleted': 'Deleted',
  'file.renamed': 'Renamed',
}

/** One-line summary of what happened, built from structured fields. */
export function summary(e: StoredEvent): string {
  const t = e.event_type
  switch (category(t)) {
    case 'file': {
      const verb = FILE_VERB[t] ?? 'File'
      return `${verb} ${e.path ? displayPath(e.path, e.project_id) : '(path not reported)'}`
    }
    case 'command': {
      const program = attrString(e, 'command.program') ?? 'command'
      if (t === 'command.started') return `Ran ${program}`
      if (t === 'command.completed') return exitCode(e) === 0 ? `${program} exited 0` : `${program} completed`
      const code = exitCode(e)
      if (e.status === 'cancelled') return `${program} interrupted`
      return code !== undefined ? `${program} exited ${code}` : `${program} failed`
    }
    case 'tool': {
      const tool = attrString(e, 'tool.name') ?? 'tool'
      if (t === 'agent.tool.started') return `${tool} started`
      if (t === 'agent.tool.failed') return e.status === 'cancelled' ? `${tool} interrupted` : `${tool} failed`
      return e.status === 'succeeded' ? `${tool} completed` : `${tool} returned`
    }
    case 'session': {
      if (t === 'agent.session.started') {
        const src = attrString(e, 'session.source')
        return src && src !== 'startup' ? `Session ${src}` : 'Session started'
      }
      const reason = attrString(e, 'session.end_reason')
      return reason ? `Session ended (${reason})` : 'Session ended'
    }
    case 'subagent': {
      const verb = t.endsWith('.started') ? 'started' : 'stopped'
      return e.message ?? `Subagent ${verb}`
    }
    case 'turn': return t === 'agent.turn.failed' ? 'Turn failed' : 'Turn completed'
    case 'prompt': {
      const chars = attr(e, 'prompt.chars')
      return typeof chars === 'number' ? `Prompt submitted (${chars} chars, text not stored)` : 'Prompt submitted (text not stored)'
    }
    case 'model': {
      const tokens = attr(e, 'tokens.total')
      return typeof tokens === 'number' ? `Model request (${tokens.toLocaleString('en-US')} tokens)` : 'Model request'
    }
    default:
      return e.message ?? t
  }
}

export interface StatusInfo {
  label: string
  tone: Tone
  /** Longer explanation for a tooltip / inspector. */
  detail: string
}

/** Status wording that never overstates what was reported. */
export function statusOf(e: StoredEvent): StatusInfo {
  const t = e.event_type
  const cat = category(t)
  if (cat === 'file') {
    const verb = (FILE_VERB[t] ?? 'changed').toLowerCase()
    return {
      label: `Reported ${verb === 'edited' ? 'modified' : verb}`,
      tone: 'info',
      detail: 'Agent-reported. TailFlow did not observe this file on disk; not independently verified.',
    }
  }
  if (cat === 'runtime') {
    if (t === 'runtime.error') return { label: 'Error', tone: 'error', detail: 'Log line detected as error level.' }
    if (t === 'runtime.warning') return { label: 'Warning', tone: 'warn', detail: 'Log line detected as warn level.' }
    return { label: 'Log', tone: 'muted', detail: 'Runtime log line.' }
  }
  if (cat === 'system') {
    if (t === 'system.events.dropped') return { label: 'Loss', tone: 'error', detail: 'TailFlow recorded lost events here.' }
    if (t === 'system.collector.error') return { label: 'Error', tone: 'error', detail: 'A collector failed.' }
    return { label: 'System', tone: 'muted', detail: 'Generated by TailFlow.' }
  }
  switch (e.status) {
    case 'failed': return { label: 'Failed', tone: 'error', detail: 'The agent reported a failure.' }
    case 'cancelled': return { label: 'Interrupted', tone: 'warn', detail: 'The agent reported an interruption.' }
    case 'succeeded': return { label: 'Succeeded', tone: 'ok', detail: 'The agent reported success.' }
    case 'started': return { label: 'Started', tone: 'muted', detail: 'Start reported; the end is a separate event.' }
  }
  if (attrString(e, 'tool.outcome') === 'not_reported' || attrString(e, 'command.outcome') === 'unknown') {
    return {
      label: 'Outcome not reported',
      tone: 'muted',
      detail: 'The integration does not report whether this succeeded. Do not read it as success.',
    }
  }
  return { label: 'Reported', tone: 'muted', detail: 'Agent-reported event without an outcome field.' }
}

export function isFailure(e: StoredEvent): boolean {
  return e.status === 'failed' || e.event_type === 'runtime.error'
    || e.event_type === 'system.collector.error' || e.event_type === 'system.events.dropped'
}

export const PROVENANCE_TEXT: Record<string, { label: string; detail: string }> = {
  agent_reported: {
    label: 'Agent-reported',
    detail: 'The agent (through its hook) said this happened. TailFlow did not independently verify it.',
  },
  filesystem_observed: {
    label: 'Filesystem-observed',
    detail: 'Seen by a filesystem watcher. Says nothing about who made the change.',
  },
  runtime_observed: {
    label: 'Runtime-observed',
    detail: 'Captured from a running process, container, file or stdin.',
  },
  system_observed: {
    label: 'TailFlow',
    detail: 'Generated by TailFlow itself (collector lifecycle, loss markers).',
  },
}

export const SESSION_STATUS: Record<SessionStatus, StatusInfo> = {
  active: { label: 'Active', tone: 'ok', detail: 'No end observed after the last start, and activity in the last 30 minutes.' },
  ended: { label: 'Ended', tone: 'muted', detail: 'An end event was observed.' },
  unterminated: {
    label: 'No end observed',
    tone: 'warn',
    detail: 'No end event and no activity for 30 minutes: crashed, killed or idle. TailFlow cannot tell which.',
  },
}

export const HISTORY_TEXT: Record<HistoryCoverage, StatusInfo> = {
  complete: {
    label: 'Complete',
    tone: 'ok',
    detail: 'Start and end observed, no recorded loss, nothing expired — within what this integration can see.',
  },
  partial: { label: 'Partial', tone: 'warn', detail: 'Some of this session was not captured or is no longer retained.' },
  unknown: { label: 'Unknown', tone: 'muted', detail: 'Neither the start nor the end of this session was observed.' },
}

export const COVERAGE_REASON: Record<string, string> = {
  start_not_observed: 'Session start was not observed (hooks installed later, or events lost).',
  end_not_observed: 'No end was observed.',
  session_in_progress: 'Session still in progress.',
  retention_truncated: 'Older events of this session were removed by retention.',
  loss_recorded: 'TailFlow recorded event loss overlapping this session.',
  summary_sampled_newest_events: 'Very large session: summaries cover the newest 50,000 events.',
}

export const LEVEL_TEXT: Record<CoverageLevel, string> = {
  direct: 'Observed',
  partial: 'Partly observable',
  none: 'Not observable',
}

/** Human name for a capability row (`tool.duration` → "Tool durations"). */
export const ACTIVITY_NAME: Record<string, string> = {
  'session.lifecycle': 'Session start / end',
  turns: 'Turns',
  'tool.calls': 'Tool calls',
  'tool.duration': 'Tool durations',
  commands: 'Shell commands',
  'file.read': 'File reads',
  'file.write': 'File writes',
  subagents: 'Subagents',
  'tokens.cost': 'Tokens / cost',
  'agent.version': 'Agent version',
}

export const GAP_REASON: Record<string, string> = {
  ingest_lag: 'Live ingestion fell behind; lines were dropped',
  queue_full: 'Storage queue was full; events were dropped',
  storage_error: 'Storage error; events were not written',
  writer_stopped: 'Storage writer stopped; events were not written',
  unclean_shutdown: 'A previous daemon run ended uncleanly',
  session_deleted: 'Events were deleted on request',
  invalid_event: 'Invalid events were rejected',
  agent_spool_overflow: 'Agent spool was full; hook calls were counted and dropped',
  agent_input_rejected: 'Hook input was rejected (malformed or oversized)',
  agent_spool_corrupt: 'A spool file could not be read',
}

export const COMPLETENESS_REASON: Record<string, string> = {
  loss_in_range: 'Recorded loss overlaps this range',
  pending_loss: 'Loss detected in this run is not written yet',
  deleted_in_range: 'Events in this range were deleted on request',
  retention_truncated: 'The range reaches back before retained history',
  cursor_behind_retention: 'The cursor is older than retained history',
  text_index_incomplete: 'The full-text index does not cover all of this range',
}
