import { useEffect, useState } from 'preact/hooks'
import { matchesQuery } from '../lib/liveFilters.ts'
import { api } from '../lib/api.ts'
import { useAsync, navigate } from '../lib/hooks.ts'
import { href, toRecord, withParams } from '../lib/router.ts'
import { count, projectName, relativeTo } from '../lib/format.ts'
import type { EvidenceStatus, FileActivity, FileEvidencePage } from '../lib/types.ts'
import { EVIDENCE } from '../lib/evidence.ts'
import { GAP_REASON } from '../lib/events.ts'
import { Agent, Badge, Callout, Card, CopyButton, Empty, ErrorState, Loading, RANGES, Segmented, Time } from '../components/ui'
import { Icon } from '../components/icons'

type Group = 'path' | 'project' | 'agent'

export function Files({ params }: { params: URLSearchParams }) {
  // Two components, so each view's hooks run in a stable order.
  return toRecord(params).view === 'verified' ? <Verified params={params} /> : <Reported params={params} />
}

function Reported({ params }: { params: URLSearchParams }) {
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
  }, s), [since, p.agent, p.project, p.session, p.path_prefix, multi], 30_000,
  e => e.event_type.startsWith('file.') && matchesQuery(e, { agent: p.agent, project: p.project, session: p.session }))
  const files = data.data?.files ?? []

  return (
    <div class="page">
      <div class="page-head">
        <div>
          <h2>Files</h2>
          <p>File operations agents <strong>reported</strong> through their hooks, grouped by path. These are claims; the <strong>Verified</strong> view compares them with what the file watcher saw in observed projects.</p>
        </div>
        <CopyButton text={location.href} label="Copy filter URL" />
      </div>
      <ViewSwitch params={params} view="reported" />
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

function ViewSwitch({ params, view }: { params: URLSearchParams; view: 'reported' | 'verified' }) {
  return (
    <div class="toolbar">
      <Segmented label="Files view" value={view} onChange={v => navigate(href('files', withParams(params, { view: v === 'reported' ? null : v, status: null })), true)} options={[
        { value: 'reported', label: 'Reported by agents' },
        { value: 'verified', label: 'Verified on disk' },
      ]} />
    </div>
  )
}

const STATUS_ORDER: EvidenceStatus[] = ['matched_observation', 'reported_only', 'unverified', 'filesystem_observed', 'ambiguous']

/** Agent-reported changes compared with independent filesystem observations. */
function Verified({ params }: { params: URLSearchParams }) {
  const p = toRecord(params)
  const since = p.since ?? '24h'
  const set = (c: Record<string, string | null>) => navigate(href('files', withParams(params, c)), true)
  const status = p.status as EvidenceStatus | undefined
  const observe = useAsync(s => api.observe(s), [], 30_000)
  const data = useAsync<FileEvidencePage>(s => api.fileEvidence({
    since, session: p.session, agent: p.agent, path_prefix: p.path_prefix, limit: 300,
  }, s), [since, p.session, p.agent, p.path_prefix], 30_000, e => e.event_type.startsWith('file.'))
  const items = (data.data?.items ?? []).filter(i => !status || i.status === status)
  const off = observe.data && !observe.data.enabled
  return (
    <div class="page">
      <div class="page-head">
        <div>
          <h2>Files</h2>
          <p>What agents <strong>reported</strong> changing, compared with what TailFlow&apos;s file watcher <strong>observed</strong> on disk in the projects you chose to observe. A watcher cannot see who wrote a file: a match is an association, not proof of authorship.</p>
        </div>
        <CopyButton text={location.href} label="Copy filter URL" />
      </div>
      <ViewSwitch params={params} view="verified" />
      {off && (
        <Callout tone="info">
          File observation is off on this daemon ({observe.data?.disabled_reason}). Start it with <code>tailflow-daemon --observe .</code> or add
          <code> [observe] paths = [&quot;.&quot;]</code> to <code>tailflow.toml</code>. Until then every reported change is <strong>unverified</strong>.
        </Callout>
      )}
      <div class="toolbar">
        <Segmented label="Time range" value={since} options={RANGES} onChange={v => set({ since: v === '24h' ? null : v })} />
        <select class="select" aria-label="Evidence status" value={status ?? ''} onChange={e => set({ status: (e.target as HTMLSelectElement).value || null })}>
          <option value="">Every status</option>
          {STATUS_ORDER.map(s => <option key={s} value={s}>{EVIDENCE[s].label}</option>)}
        </select>
        {p.session && <span class="chip"><span>session: {p.session}</span><button aria-label="Remove session filter" onClick={() => set({ session: null })}><Icon name="x" size="sm" /></button></span>}
      </div>
      {data.data && (
        <div class="chips" role="list" aria-label="Counts by status">
          {STATUS_ORDER.map(s => (
            <span role="listitem" key={s}>
              <Badge tone={EVIDENCE[s].tone} title={EVIDENCE[s].detail}>{`${EVIDENCE[s].label} ${count(data.data!.counts[s] ?? 0)}`}</Badge>
            </span>
          ))}
          <span class="note">window ±{(data.data.window_ms / 1000).toFixed(0)} s</span>
        </div>
      )}
      {data.data && data.data.loss.length > 0 && (
        <Callout tone="warn">
          The file watcher lost notifications in this range ({data.data.loss.map(l => GAP_REASON[l.reason] ?? l.reason).join('; ')}).
          Reports near those times are marked unverified.
        </Callout>
      )}
      {data.data?.truncated && <Callout tone="muted">More changes exist in this range than were compared. Narrow the time range or filter by session.</Callout>}
      {data.error ? <Card><ErrorState error={data.error} onRetry={data.reload} /></Card>
        : !data.data ? <Card><Loading rows={6} /></Card>
        : items.length === 0 ? <Card><Empty icon="files" title="No file changes">No reported or observed file changes in this range{status ? ' with this status' : ''}.</Empty></Card>
        : (
          <Card flush>
            <div class="table-wrap">
              <table class="table">
                <thead><tr><th>Path</th><th>Evidence</th><th>Reported by</th><th class="num">Observed</th><th>When</th></tr></thead>
                <tbody>
                  {items.map(i => (
                    <tr key={`${i.status}|${i.path}|${i.at}`}>
                      <td class="mono" title={i.path} style={{ maxWidth: '460px' }}>
                        <a href={href('activity', { type: 'files', path: i.path, since: '30d' })}>{relativeTo(i.path, data.data!.coverage.find(c => i.path.startsWith(c.root))?.root)}</a>
                      </td>
                      <td><Badge tone={EVIDENCE[i.status].tone} title={`${EVIDENCE[i.status].detail} ${i.explanation}`}>{EVIDENCE[i.status].label}</Badge></td>
                      <td>
                        <span class="chips">
                          {i.reported.length === 0 ? <span class="dim">no report</span> : [...new Set(i.reported.map(r => r.session_id ?? '?'))].map(sid => (
                            <a key={sid} class="mono" href={href('session', {}, sid)} title={sid}>{sid.split(':').pop()?.slice(0, 8)}</a>
                          ))}
                        </span>
                      </td>
                      <td class="num">{count(i.observed.length)}</td>
                      <td class="dim"><Time at={i.at} /></td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </Card>
        )}
      {data.data && data.data.coverage.length > 0 && (
        <p class="note">Observed roots in this range: {[...new Set(data.data.coverage.map(c => c.root))].map(r => projectName(r)).join(', ')}
          {data.data.coverage.some(c => c.unwatched_dirs > 0) ? ' · coverage was partial for some (watch budget)' : ''}.</p>
      )}
    </div>
  )
}
