// Which committed agent events concern which view (M4-B).
//
// These predicates decide whether a page reloads its own bounded query;
// they never decide what the page shows (the daemon's query does). They are
// deliberately permissive: a false match costs one coalesced reload, while
// a false miss would leave the page stale until the next resync. Filters
// the stream does not carry (full text, path globs, programs) are treated
// as "may match".

import type { StreamEvent } from './agentStream.ts'

type Query = Record<string, string | number | boolean | null | undefined>

const FAILURE_TYPES = new Set(['agent.tool.failed', 'command.failed', 'agent.turn.failed', 'system.events.dropped'])

/** An event that can appear in a failure list. */
export function isFailureEvent(e: StreamEvent): boolean {
  return FAILURE_TYPES.has(e.event_type) || e.status === 'failed'
}

/** An event that changes a session's row (counts, status, last activity). */
export function isSessionChange(e: StreamEvent): boolean {
  return !!e.session_id || e.event_type === 'system.events.dropped'
}

function list(v: Query[string]): string[] {
  return typeof v === 'string' && v !== '' ? v.split(',').map(s => s.trim()).filter(Boolean) : []
}

/** `type` values are exact catalog names or `family.*`. */
function typeMatches(types: string[], t: string): boolean {
  return types.length === 0 || types.some(x => (x.endsWith('.*') ? t.startsWith(x.slice(0, -1)) : x === t))
}

/** Whether `e` may belong to the result of an `/api/v1/events` query. */
export function matchesQuery(e: StreamEvent, q: Query): boolean {
  // Loss markers affect every view's completeness.
  if (e.event_type === 'system.events.dropped') return true
  const eq = (key: string, value: string | null | undefined) => {
    const want = list(q[key])
    return want.length === 0 || (value != null && want.includes(value))
  }
  return typeMatches(list(q.type), e.event_type)
    && eq('agent', e.agent_name)
    && eq('session', e.session_id)
    && eq('project', e.project_id)
    && eq('status', e.status ?? null)
}
