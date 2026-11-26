// Event inspector: every field of one event, its provenance, and related
// events graded by evidence tier (GET /api/v1/events/{seq}/related).
// Relationships are labelled by how they are known; time proximity is
// never presented as cause.

import type { ComponentChildren } from 'preact'
import type { StoredEvent } from '../lib/types.ts'
import { api } from '../lib/api.ts'
import { useAsync, useSettings } from '../lib/hooks.ts'
import { dateTime, duration, between, clock } from '../lib/format.ts'
import { agentLabel, PROVENANCE_TEXT, statusOf, summary, category } from '../lib/events.ts'
import { href } from '../lib/router.ts'
import { Agent, Badge, Callout, CopyButton, StatusBadge } from './ui'
import { Icon } from './icons'

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
      <Relationships e={e} />
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

/** The operation ids the source reported (links are graded below). */
function Relationships({ e }: { e: StoredEvent }) {
  const op = e.operation_id
  const parent = e.parent_operation_id
  if (!op && !parent) return null
  return (
    <section>
      <h4>Operation</h4>
      <dl class="kv">
        {op && <Field k="Operation">{<span class="mono">{op}</span>}</Field>}
        {parent && <Field k="Parent">{<span class="mono">{parent}</span>}</Field>}
      </dl>
    </section>
  )
}

const TIER_TONE: Record<string, 'ok' | 'info' | 'muted' | 'warn'> = {
  'recorded link': 'ok', observed: 'ok', context: 'info', nearby: 'muted', inferred: 'warn',
}

/** Related events graded by evidence (direct ids, independent observation,
 *  shared context, time proximity, stated heuristics). Never causation. */
function Nearby({ e, onSelect }: { e: StoredEvent; onSelect?: (seq: number) => void }) {
  const r = useAsync(s => api.related(e.seq, s), [e.seq])
  return (
    <section>
      <h4>Related evidence</h4>
      {r.error ? <p class="note">Unavailable: {(r.error as Error).message}</p>
        : !r.data ? <div class="skeleton" />
        : (
          <>
            {r.data.tiers.map(t => (
              <div key={t.tier} class="tier">
                <div class="tier-head">
                  <Badge tone={TIER_TONE[t.strength] ?? 'muted'} title={t.description}>{`${t.tier}. ${t.name}`}</Badge>
                  <span class="note">{t.total === 0 ? 'none' : `${t.total}${t.total > t.items.length ? ` · showing ${t.items.length}` : ''}`}</span>
                </div>
                {t.items.length > 0 && (
                  <ul style={{ listStyle: 'none', display: 'grid', gap: '2px' }}>
                    {t.items.map(x => (
                      <li key={x.seq}>
                        <button class="btn sm ghost" style={{ maxWidth: '100%' }} onClick={() => onSelect?.(x.seq)} title={x.explanation}>
                          <span class="mono dim">{x.delta_ms >= 0 ? '+' : '−'}{duration(Math.abs(x.delta_ms))}</span>
                          <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{x.summary ?? x.event_type}</span>
                        </button>
                        <div class="note tier-why">{x.explanation}</div>
                      </li>
                    ))}
                  </ul>
                )}
                {t.note && <p class="note tier-why">{t.note}</p>}
              </div>
            ))}
            <p class="note" style={{ marginTop: '6px' }}>{r.data.notice} Window ±{Math.round(r.data.window_ms / 1000)} s.</p>
          </>
        )}
    </section>
  )
}
