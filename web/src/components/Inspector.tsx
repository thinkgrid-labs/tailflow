// Event inspector: every field of one event, its provenance, explicit
// relationships (shared operation ids), and runtime events near it in time.
// Relationships are labelled by how they are known; time proximity is
// never presented as cause.

import type { ComponentChildren } from 'preact'
import { useMemo } from 'preact/hooks'
import type { StoredEvent } from '../lib/types.ts'
import { api } from '../lib/api.ts'
import { useAsync, useSettings } from '../lib/hooks.ts'
import { dateTime, duration, between, clock } from '../lib/format.ts'
import { agentLabel, PROVENANCE_TEXT, statusOf, summary, category } from '../lib/events.ts'
import { href } from '../lib/router.ts'
import { Agent, Badge, Callout, CopyButton, StatusBadge } from './ui'
import { Icon } from './icons'

const WINDOW_MS = 60_000

function Field({ k, children }: { k: string; children: ComponentChildren }) {
  return (<><dt>{k}</dt><dd>{children}</dd></>)
}

function valueText(v: unknown): string {
  if (typeof v === 'string') return v
  try { return JSON.stringify(v) } catch { return String(v) }
}

export function Inspector({ seq, onClose, onSelect }: { seq: number; onClose: () => void; onSelect?: (seq: number) => void }) {
  const { settings } = useSettings()
  const ev = useAsync(s => api.event(seq, s), [seq])
  const e = ev.data

  return (
    <aside class="inspector" aria-label="Event details">
      <div class="inspector-head">
        <h3>{e ? summary(e) : `Event ${seq}`}</h3>
        <button class="btn sm icon-only ghost" onClick={onClose} aria-label="Close details" title="Close (Esc)"><Icon name="x" /></button>
      </div>
      {ev.error ? (
        <section><Callout tone="warn">{(ev.error as Error).message || 'This event could not be loaded.'}</Callout></section>
      ) : !e ? (
        <section><div class="skeleton" /></section>
      ) : (
        <EventBody e={e} tz={settings.tz} onSelect={onSelect} />
      )}
    </aside>
  )
}

function EventBody({ e, tz, onSelect }: { e: StoredEvent; tz: 'local' | 'utc'; onSelect?: (seq: number) => void }) {
  const st = statusOf(e)
  const prov = PROVENANCE_TEXT[e.provenance] ?? { label: e.provenance, detail: '' }
  const lag = between(e.timestamp, e.observed_at)
  const ref = `tailflow event ${e.seq} (${e.event_id})`
  const link = `${location.origin}${location.pathname}${href('activity', { event: e.seq })}`
  const attrs = Object.entries(e.attributes ?? {})
  const red = e.redaction ?? {}
  return (
    <>
      <section>
        <div class="toolbar">
          <StatusBadge info={st} />
          <Badge tone="outline" title={prov.detail}>{prov.label}</Badge>
          <span class="mono dim" style={{ fontSize: '12px' }}>{e.event_type}</span>
        </div>
        {e.provenance === 'agent_reported' && category(e.event_type) === 'file' && (
          <div style={{ marginTop: '10px' }}>
            <Callout tone="muted">Reported by {agentLabel(e.agent_name)} through its hook. TailFlow did not watch the file system: this change is <strong>not independently verified</strong>.</Callout>
          </div>
        )}
        {st.label === 'Outcome not reported' && (
          <div style={{ marginTop: '10px' }}>
            <Callout tone="muted">{st.detail}</Callout>
          </div>
        )}
      </section>
      <section>
        <h4>Event</h4>
        <dl class="kv">
          <Field k="Timestamp">{dateTime(e.timestamp, tz)} <span class="dim">.{clock(e.timestamp, tz, true).slice(-3)}</span></Field>
          <Field k="Observed at">
            {dateTime(e.observed_at, tz)}
            {lag !== null && lag > 2000 && <> <Badge tone="warn" title="TailFlow received this event later than it happened (e.g. spooled while no daemon ran).">arrived {duration(lag)} later</Badge></>}
          </Field>
          {e.duration_ms !== undefined && <Field k="Duration">{duration(e.duration_ms)} <span class="dim">(reported)</span></Field>}
          <Field k="Severity">{e.severity}</Field>
          <Field k="Seq">{e.seq}</Field>
          <Field k="Event ID"><span class="mono">{e.event_id}</span></Field>
        </dl>
      </section>
      <section>
        <h4>Context</h4>
        <dl class="kv">
          <Field k="Agent">{e.agent_name ? <Agent name={e.agent_name} /> : <span class="dim">— (not an agent event)</span>}</Field>
          <Field k="Session">{e.session_id ? <a href={href('session', {}, e.session_id)}>{e.session_id}</a> : <span class="dim">—</span>}</Field>
          <Field k="Project">{e.project_id ?? <span class="dim">—</span>}</Field>
          <Field k="Source">{e.source_type} / {e.source_id}</Field>
          {e.path && <Field k={category(e.event_type) === 'file' ? 'Path' : 'Directory'}><span class="mono">{e.path}</span></Field>}
        </dl>
      </section>
      <Relationships e={e} onSelect={onSelect} />
      <Nearby e={e} onSelect={onSelect} />
      <section>
        <h4>Attributes</h4>
        {attrs.length === 0 ? <p class="note">None.</p> : (
          <dl class="kv">
            {attrs.map(([k, v]) => <Field key={k} k={k}><span class="mono">{valueText(v)}</span></Field>)}
          </dl>
        )}
      </section>
      <section>
        <h4>Privacy</h4>
        <dl class="kv">
          <Field k="Redaction">{red.replacements ? `${red.replacements} replacement(s): ${(red.rules ?? []).join(', ')}` : 'none applied'}</Field>
          <Field k="Payload">{red.payload_dropped ? 'dropped by capture mode' : e.payload ? 'stored (below)' : 'none'}</Field>
          {red.truncated && <Field k="Truncated">message or payload cut to the size bound</Field>}
        </dl>
        <p class="note" style={{ marginTop: '8px' }}>Prompt text and tool output are never stored.</p>
      </section>
      {e.payload && (
        <section>
          <h4>Payload</h4>
          <pre class="json-block">{e.payload}</pre>
        </section>
      )}
      <section class="toolbar">
        <CopyButton text={ref} label="Copy reference" />
        <CopyButton text={link} label="Copy link" />
      </section>
    </>
  )
}

/** Events sharing this event's operation or its parent: explicit ids only. */
function Relationships({ e, onSelect }: { e: StoredEvent; onSelect?: (seq: number) => void }) {
  const op = e.operation_id
  const parent = e.parent_operation_id
  const rel = useAsync(async s => {
    if (!e.session_id || (!op && !parent)) return []
    const page = await api.events({ session: e.session_id, after: Math.max(0, e.seq - 400), before: e.seq + 400, limit: 800 }, s)
    return page.events.filter(x => x.seq !== e.seq && (
      (op && (x.operation_id === op || x.parent_operation_id === op)) ||
      (parent && (x.operation_id === parent || x.parent_operation_id === parent))))
  }, [e.seq])
  if (!op && !parent) return null
  return (
    <section>
      <h4>Operation</h4>
      <dl class="kv">
        {op && <Field k="Operation">{<span class="mono">{op}</span>}</Field>}
        {parent && <Field k="Parent">{<span class="mono">{parent}</span>}</Field>}
      </dl>
      <p class="note" style={{ margin: '8px 0 4px' }}>Linked by ids the agent reported.</p>
      {rel.data && rel.data.length > 0 && (
        <ul style={{ listStyle: 'none', display: 'grid', gap: '4px' }}>
          {rel.data.slice(0, 12).map(x => (
            <li key={x.seq}><button class="btn sm ghost" onClick={() => onSelect?.(x.seq)}>{summary(x)} <span class="dim">· {statusOf(x).label}</span></button></li>
          ))}
        </ul>
      )}
    </section>
  )
}

/** Runtime warnings/errors within ±60 s. Chronology, not causation. */
function Nearby({ e, onSelect }: { e: StoredEvent; onSelect?: (seq: number) => void }) {
  const isRuntime = category(e.event_type) === 'runtime'
  const t = Date.parse(e.timestamp)
  const range = useMemo(() => ({
    since: new Date(t - WINDOW_MS).toISOString(),
    until: new Date(t + WINDOW_MS).toISOString(),
  }), [t])
  const near = useAsync(s => api.events({
    type: isRuntime ? 'agent.*,command.*,file.*' : 'runtime.error,runtime.warning',
    since: range.since, until: range.until, limit: 25,
  }, s), [e.seq])
  if (Number.isNaN(t)) return null
  const items = (near.data?.events ?? []).filter(x => x.seq !== e.seq)
  return (
    <section>
      <h4>{isRuntime ? 'Agent activity within ±60 s' : 'Runtime warnings and errors within ±60 s'}</h4>
      {near.error ? <p class="note">Unavailable: {(near.error as Error).message}</p>
        : !near.data ? <div class="skeleton" />
        : items.length === 0 ? <p class="note">None in this window.</p>
        : (
          <ul style={{ listStyle: 'none', display: 'grid', gap: '2px' }}>
            {items.map(x => {
              const dt = Date.parse(x.timestamp) - t
              const sameProject = !!x.project_id && x.project_id === e.project_id
              return (
                <li key={x.seq}>
                  <button class="btn sm ghost" style={{ maxWidth: '100%' }} onClick={() => onSelect?.(x.seq)} title={x.message ?? summary(x)}>
                    <span class="mono dim">{dt >= 0 ? '+' : '−'}{duration(Math.abs(dt))}</span>
                    <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{x.message ?? summary(x)}</span>
                    {sameProject && <Badge tone="outline">same project</Badge>}
                  </button>
                </li>
              )
            })}
          </ul>
        )}
      <p class="note" style={{ marginTop: '6px' }}>
        {dt0(isRuntime)} Shown because they are close in time — that alone does not mean they are related.
      </p>
    </section>
  )
}

const dt0 = (rt: boolean) => (rt ? 'Agent operations near this log line.' : 'Occurred before (−) or after (+) this event.')
