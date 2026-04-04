import { sourceColor } from '../types'

interface Props {
  sources: Map<string, number>
  selected: Set<string> | null   // null = all selected
  onToggle: (source: string) => void
  onSelectAll: () => void
}

export function Sidebar({ sources, selected, onToggle, onSelectAll }: Props) {
  const allSelected = selected === null

  return (
    <aside class="sidebar">
      <div class="sidebar-header">Sources</div>
      <button
        class={`source-row ${allSelected ? 'source-row--active' : ''}`}
        onClick={onSelectAll}
      >
        <span class="source-dot" style={{ background: '#9ca3af' }} />
        <span class="source-name">all</span>
        <span class="source-count">
          {[...sources.values()].reduce((a, b) => a + b, 0)}
        </span>
      </button>

      {[...sources.entries()].map(([name, count]) => {
        const active = allSelected || selected!.has(name)
        const color  = sourceColor(name)
        return (
          <button
            key={name}
            class={`source-row ${active ? 'source-row--active' : 'source-row--dim'}`}
            onClick={() => onToggle(name)}
          >
            <span class="source-dot" style={{ background: color }} />
            <span class="source-name">{name}</span>
            <span class="source-count">{count}</span>
          </button>
        )
      })}
    </aside>
  )
}
