// Small presentational components shared by every page. All text content
// is rendered as text nodes (never as HTML), so event data cannot inject
// markup.

import type { ComponentChildren } from 'preact'
import { useState } from 'preact/hooks'
import { Icon, type IconName } from './icons'
import { agentLabel, HISTORY_TEXT, SESSION_STATUS, type Tone, type StatusInfo } from '../lib/events.ts'
import type { HistoryCoverage, SessionStatus } from '../lib/types.ts'
import { copyText, useSettings } from '../lib/hooks.ts'
import { dateTime, relative, shortTime } from '../lib/format.ts'
import { describeError } from '../lib/api.ts'

const TONE_ICON: Record<Tone, IconName> = {
  ok: 'check', warn: 'alert', error: 'alert', info: 'info', muted: 'dashed', accent: 'info',
}

export function Badge({ tone = 'muted', icon, children, title }: {
  tone?: Tone | 'outline'; icon?: IconName | false; children: ComponentChildren; title?: string
}) {
  const ic = icon === false ? null : icon ?? (tone === 'outline' ? null : TONE_ICON[tone])
  return (
    <span class={`badge ${tone}`} title={title}>
      {ic && <Icon name={ic} />}
      {children}
    </span>
  )
}

export function StatusBadge({ info }: { info: StatusInfo }) {
  return <Badge tone={info.tone} title={info.detail}>{info.label}</Badge>
}

export function SessionStatusBadge({ status }: { status: SessionStatus }) {
  const info = SESSION_STATUS[status]
  return <Badge tone={info.tone} icon={status === 'active' ? 'play2' : status === 'ended' ? 'stop' : 'dashed'} title={info.detail}>{info.label}</Badge>
}

export function HistoryBadge({ history }: { history: HistoryCoverage }) {
  const info = HISTORY_TEXT[history]
  return <Badge tone={info.tone} title={info.detail}>{`History ${info.label.toLowerCase()}`}</Badge>
}

export function Agent({ name }: { name: string | null | undefined }) {
  const key = name ?? 'runtime'
  return (
    <span class="agent">
      <span class={`agent-dot ${key}`} aria-hidden="true" />
      {agentLabel(name)}
    </span>
  )
}

export function Card({ title, sub, actions, children, flush, id }: {
  title?: ComponentChildren; sub?: ComponentChildren; actions?: ComponentChildren
  children: ComponentChildren; flush?: boolean; id?: string
}) {
  return (
    <section class="card" aria-labelledby={id}>
      {(title || actions) && (
        <div class="card-head">
          <div>
            {title && <h3 id={id}>{title}</h3>}
            {sub && <div class="sub">{sub}</div>}
          </div>
          {actions && <div class="toolbar">{actions}</div>}
        </div>
      )}
      <div class={`card-body${flush ? ' flush' : ''}`}>{children}</div>
    </section>
  )
}

export function Stat({ label, value, foot, tone, icon, href }: {
  label: string; value: ComponentChildren; foot?: ComponentChildren; tone?: 'error' | 'ok'
  icon?: IconName; href?: string
}) {
  const body = (
    <>
      <span class="label">{icon && <Icon name={icon} size="sm" />}{label}</span>
      <span class="value">{value}</span>
      <span class="foot">{foot}</span>
    </>
  )
  return href
    ? <a class={`card stat${tone ? ` ${tone}` : ''}`} href={href}>{body}</a>
    : <div class={`card stat${tone ? ` ${tone}` : ''}`}>{body}</div>
}

export function Empty({ icon = 'info', title, children }: { icon?: IconName; title: string; children?: ComponentChildren }) {
  return (
    <div class="empty" role="status">
      <Icon name={icon} />
      <h4>{title}</h4>
      {children && <p>{children}</p>}
    </div>
  )
}

export function ErrorState({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  return (
    <div class="empty" role="alert">
      <Icon name="alert" />
      <h4>Could not load this view</h4>
      <p>{describeError(error)}</p>
      {onRetry && <button class="btn sm" onClick={onRetry}><Icon name="refresh" size="sm" />Retry</button>}
    </div>
  )
}

export function Loading({ rows = 4 }: { rows?: number }) {
  return (
    <div class="card-body" aria-busy="true" aria-label="Loading">
      {Array.from({ length: rows }, (_, i) => (
        <div class="skeleton" style={{ width: `${90 - i * 12}%`, marginBottom: '10px' }} key={i} />
      ))}
    </div>
  )
}

export function Callout({ tone = 'info', children }: { tone?: 'info' | 'warn' | 'error' | 'muted'; children: ComponentChildren }) {
  return (
    <div class={`callout ${tone}`} role={tone === 'error' ? 'alert' : undefined}>
      <Icon name={tone === 'info' || tone === 'muted' ? 'info' : 'alert'} size="sm" />
      <div>{children}</div>
    </div>
  )
}

/** A timestamp in the chosen zone, with full date-time and relative age on hover. */
export function Time({ at, mode = 'short' }: { at: string | null | undefined; mode?: 'short' | 'full' | 'relative' }) {
  const { settings } = useSettings()
  if (!at) return <span class="dim">—</span>
  const full = dateTime(at, settings.tz)
  const text = mode === 'full' ? full : mode === 'relative' ? relative(at) : shortTime(at, settings.tz)
  return <time dateTime={at} title={`${full} (${relative(at)})`}>{text}</time>
}

export function CopyButton({ text, label = 'Copy', iconOnly }: { text: string; label?: string; iconOnly?: boolean }) {
  const [done, setDone] = useState<null | boolean>(null)
  const onClick = async () => {
    const ok = await copyText(text)
    setDone(ok)
    setTimeout(() => setDone(null), 1500)
  }
  const state = done === true ? 'Copied' : done === false ? 'Copy failed' : label
  return (
    <button class={`btn sm${iconOnly ? ' icon-only' : ''}`} onClick={onClick} title={state} aria-label={state}>
      <Icon name={done ? 'check' : 'copy'} size="sm" />
      {!iconOnly && <span class={done ? 'copy-ok' : undefined}>{state}</span>}
      <span class="sr-only" aria-live="polite">{done === null ? '' : state}</span>
    </button>
  )
}

export function Segmented<T extends string>({ value, options, onChange, label }: {
  value: T; options: readonly { value: T; label: string }[]; onChange: (v: T) => void; label: string
}) {
  return (
    <div class="seg" role="group" aria-label={label}>
      {options.map(o => (
        <button key={o.value} aria-pressed={o.value === value} onClick={() => onChange(o.value)}>{o.label}</button>
      ))}
    </div>
  )
}

export function Num({ n, bad }: { n: number | null | undefined; bad?: boolean }) {
  const v = n ?? 0
  return <td class={`num${v === 0 ? ' zero' : bad ? ' bad' : ''}`}>{n === null || n === undefined ? '—' : v.toLocaleString('en-US')}</td>
}

export const RANGES = [
  { value: '1h', label: '1h' },
  { value: '6h', label: '6h' },
  { value: '24h', label: '24h' },
  { value: '7d', label: '7d' },
  { value: '30d', label: '30d' },
] as const
