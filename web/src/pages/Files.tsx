import { useEffect, useState } from 'preact/hooks'
import { api } from '../lib/api.ts'
import { useAsync, navigate } from '../lib/hooks.ts'
import { href, toRecord, withParams } from '../lib/router.ts'
import { count, projectName, relativeTo } from '../lib/format.ts'
import type { FileActivity } from '../lib/types.ts'
import { Agent, Badge, Callout, Card, CopyButton, Empty, ErrorState, Loading, RANGES, Segmented, Time } from '../components/ui'
import { Icon } from '../components/icons'

type Group = 'path' | 'project' | 'agent'

export function Files({ params }: { params: URLSearchParams }) {
  const p = toRecord(params)
  const since = p.since ?? '7d'
  const group = (p.group as Group) || 'path'
  const multi = p.multi === '1'
  const set = (c: Record<string, string | null>) => navigate(href('files', withParams(params, c)), true)
  const [prefix, setPrefix] = useState(p.path_prefix ?? '')
  useEffect(() => setPrefix(p.path_prefix ?? ''), [p.path_prefix])

  const data = useAsync(s => api.files({
    since, agent: p.agent, project: p.project, session: p.session, path_prefix: p.path_prefix,
    multi_agent: multi || undefined, limit: 500,
  }, s), [since, p.agent, p.project, p.session, p.path_prefix, multi], 30_000)
  const files = data.data?.files ?? []

  return (
    <div class="page">
      <div class="page-head">
        <div>
          <h2>Files</h2>
          <p>File operations agents <strong>reported</strong> through their hooks, grouped by path. TailFlow does not watch the file system: none of these are independently verified.</p>
        </div>
        <CopyButton text={location.href} label="Copy filter URL" />
      </div>
      <Callout tone="muted">
        Claude Code reports reads (Read tool) and writes (Write/Edit/MultiEdit/NotebookEdit); edits made by shell commands are invisible.
        Codex reports only <code>apply_patch</code> changes and no reads — a file with zero reported reads may still have been read.
      </Callout>
      <form class="toolbar" onSubmit={e => { e.preventDefault(); set({ path_prefix: prefix.trim() || null }) }}>
        <input class="input grow" type="search" value={prefix} placeholder="Path prefix, e.g. /work/app/src/" aria-label="Path prefix"
          onInput={e => setPrefix((e.target as HTMLInputElement).value)} />
        <select class="select" aria-label="Agent" value={p.agent ?? ''} onChange={e => set({ agent: (e.target as HTMLSelectElement).value || null })}>
          <option value="">All agents</option><option value="claude-code">Claude Code</option><option value="codex">Codex</option>
        </select>
        <Segmented label="Time range" value={since} options={RANGES} onChange={v => set({ since: v === '7d' ? null : v })} />
        <label class="btn"><input type="checkbox" checked={multi} onChange={e => set({ multi: (e.target as HTMLInputElement).checked ? '1' : null })} /> Touched by more than one agent</label>
      </form>
      <div class="toolbar">
        <Segmented label="Group by" value={group} onChange={v => set({ group: v === 'path' ? null : v })} options={[
          { value: 'path', label: 'By path' }, { value: 'project', label: 'By project' }, { value: 'agent', label: 'By agent' },
        ]} />
        {(p.project || p.session) && (
          <div class="chips">
            {p.project && <span class="chip"><span>project: {projectName(p.project)}</span><button aria-label="Remove project filter" onClick={() => set({ project: null })}><Icon name="x" size="sm" /></button></span>}
            {p.session && <span class="chip"><span>session: {p.session}</span><button aria-label="Remove session filter" onClick={() => set({ session: null })}><Icon name="x" size="sm" /></button></span>}
          </div>
        )}
        {data.data && <span class="note">{count(data.data.total_paths)} path{data.data.total_paths === 1 ? '' : 's'}{data.data.total_paths > files.length ? ` · showing the ${count(files.length)} most recent` : ''}</span>}
      </div>
      {data.error ? <Card><ErrorState error={data.error} onRetry={data.reload} /></Card>
        : !data.data ? <Card><Loading rows={6} /></Card>
        : files.length === 0 ? <Card><Empty icon="files" title="No reported file operations">{multi ? 'No path was reported by more than one agent in this range.' : 'No agent reported a file operation in this range.'}</Empty></Card>
        : <Grouped files={files} group={group} />}
    </div>
  )
}

function Grouped({ files, group }: { files: FileActivity[]; group: Group }) {
  if (group === 'path') return <Card flush><FileTable files={files} /></Card>
  const groups = new Map<string, FileActivity[]>()
  for (const f of files) {
    const keys = group === 'project' ? [f.project_id ?? '(no project)'] : f.agents
    for (const k of keys) groups.set(k, [...(groups.get(k) ?? []), f])
  }
  return (
    <>
      {[...groups.entries()].map(([k, list]) => (
        <Card key={k} flush title={group === 'agent' ? <Agent name={k} /> : <span title={k}>{projectName(k)}</span>}
          sub={`${count(list.length)} path${list.length === 1 ? '' : 's'}${group === 'project' ? ` · ${k}` : ''}`}
          actions={group === 'project' && k !== '(no project)' ? <a class="btn sm ghost" href={href('files', { project: k })}>Only this project</a> : undefined}>
          <FileTable files={list} root={group === 'project' ? k : undefined} />
        </Card>
      ))}
    </>
  )
}

function FileTable({ files, root }: { files: FileActivity[]; root?: string }) {
  const op = (n: number, label: string) => (n > 0 ? <Badge tone="info" icon={false}>{`${label} ${n}`}</Badge> : null)
  return (
    <div class="table-wrap">
      <table class="table">
        <thead><tr>
          <th>Path</th><th>Reported operations</th><th>Agents</th><th class="num hide-sm">Sessions</th><th>Last reported</th>
        </tr></thead>
        <tbody>
          {files.map(f => (
            <tr key={`${f.project_id}|${f.path}`}>
              <td class="mono" title={f.path} style={{ maxWidth: '520px' }}>
                <a href={href('activity', { type: 'files', path: f.path, since: '30d' })}>{relativeTo(f.path, root ?? f.project_id)}</a>
              </td>
              <td><span class="chips">{op(f.reads, 'read')}{op(f.created, 'created')}{op(f.modified, 'modified')}{op(f.deleted, 'deleted')}{op(f.renamed, 'renamed')}</span></td>
              <td><span class="chips">{f.agents.map(a => <Agent key={a} name={a} />)}{f.agents.length > 1 && <Badge tone="warn" title="More than one agent reported operations on this path">shared</Badge>}</span></td>
              <td class="num hide-sm">{count(f.sessions)}</td>
              <td class="dim"><Time at={f.last_at} /> <span class="hide-sm">· {f.last_event_type.replace('file.', 'reported ')}</span></td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
