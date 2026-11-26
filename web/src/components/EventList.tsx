// Virtualized activity rows: `time | agent | icon | activity | status | duration`.

import type { StoredEvent } from '../lib/types.ts'
import { category, PROVENANCE_TEXT, statusOf, summary, type Category } from '../lib/events.ts'
import { clock, duration } from '../lib/format.ts'
import { useSettings } from '../lib/hooks.ts'
import { Agent, Badge, StatusBadge } from './ui'
import { Icon, type IconName } from './icons'
import { VirtualList } from './VirtualList'
import type { ComponentChildren } from 'preact'

const CAT_ICON: Record<Category, IconName> = {
  session: 'sessions', tool: 'tool', command: 'commands', file: 'files', subagent: 'agent', turn: 'message',
  prompt: 'message', model: 'cpu', runtime: 'logs', system: 'database', git: 'branch', other: 'info',
}

export function CategoryIcon({ e }: { e: StoredEvent }) {
  const c = category(e.event_type)
  const tone = statusOf(e).tone
  return (
    <span class={`cat${tone === 'error' ? ' error' : tone === 'warn' ? ' warn' : ''}`} title={c} aria-hidden="true">
      <Icon name={CAT_ICON[c]} />
    </span>
  )
}

export function EventRow({ e, selected, onSelect }: { e: StoredEvent; selected: boolean; onSelect: (seq: number) => void }) {
  const { settings } = useSettings()
  const text = summary(e)
  return (
    <div class="act-row" role="button" tabIndex={0} aria-selected={selected} data-seq={e.seq}
      onClick={() => onSelect(e.seq)}
      onKeyDown={ev => { if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); onSelect(e.seq) } }}>
      <span class="t" title={e.timestamp}>{clock(e.timestamp, settings.tz)}</span>
      <span class="who">
        {e.provenance === 'filesystem_observed'
          ? <Badge tone="outline" icon="files" title={PROVENANCE_TEXT.filesystem_observed.detail}>{e.source_type === 'git' ? 'git' : 'on disk'}</Badge>
          : <Agent name={e.agent_name ?? (category(e.event_type) === 'runtime' ? null : e.source_id)} />}
      </span>
      <CategoryIcon e={e} />
      <span class="what" title={e.message ?? text}>
        {text}
        {category(e.event_type) === 'runtime' && <small>{e.source_id}</small>}
      </span>
      <StatusBadge info={statusOf(e)} />
      <span class="dur">{e.duration_ms !== undefined ? duration(e.duration_ms) : ''}</span>
    </div>
  )
}

export function EventList({ events, selected, onSelect, footer, label }: {
  events: readonly StoredEvent[]; selected: number | null; onSelect: (seq: number) => void
  footer?: ComponentChildren; label: string
}) {
  const { settings } = useSettings()
  return (
    <VirtualList
      items={events}
      keyOf={e => e.seq}
      estimate={settings.density === 'compact' ? 29 : 35}
      overscan={10}
      label={label}
      footer={footer}
      render={e => <EventRow e={e} selected={e.seq === selected} onSelect={onSelect} />}
    />
  )
}

/** Up/Down move the selection through `events` (keyboard navigation). */
export function moveSelection(events: readonly StoredEvent[], current: number | null, dir: 1 | -1): number | null {
  if (events.length === 0) return null
  const i = current === null ? -1 : events.findIndex(e => e.seq === current)
  const next = Math.min(events.length - 1, Math.max(0, i + dir))
  return events[next]?.seq ?? null
}
