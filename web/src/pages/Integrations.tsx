import type { ComponentChildren } from 'preact'
import { api } from '../lib/api.ts'
import { useAsync } from '../lib/hooks.ts'
import { bytes, count } from '../lib/format.ts'
import { ACTIVITY_NAME, LEVEL_TEXT } from '../lib/events.ts'
import type { AgentsStatus } from '../lib/types.ts'
import { Agent, Badge, Callout, Card, CopyButton, ErrorState, Loading, Time } from '../components/ui'
import { INSTALL_TEXT } from './Overview'

function Row({ k, children, warn }: { k: string; children: ComponentChildren; warn?: boolean }) {
  return (<><dt>{k}</dt><dd style={warn ? { color: 'var(--warn)' } : undefined}>{children}</dd></>)
}

function Cmd({ cmd }: { cmd: string }) {
  return <div class="cmd-copy"><code>{cmd}</code><CopyButton text={cmd} iconOnly label={`Copy: ${cmd}`} /></div>
}

export function Integrations() {
  const agents = useAsync(s => api.agents(s), [], 10_000)
  const storage = useAsync(s => api.storage(s), [], 10_000)
  const collectors = useAsync(s => api.collectors(s), [], 10_000)
  const privacy = useAsync(s => api.privacy(s), [], 60_000)
  const a = agents.data
  const st = storage.data
  const h = st?.health
  const db = st?.db
  const liveLoss = st?.ingest?.records_dropped ?? 0
  const loss = liveLoss + (h?.events_dropped_total ?? 0)

  return (
    <div class="page">
      <div class="page-head">
        <div>
          <h2>Integrations &amp; storage</h2>
          <p>Whether agent hooks are configured, whether events are arriving, and whether history is complete. Read-only: install and remove hooks with the CLI.</p>
        </div>
      </div>

      {agents.error ? <Card><ErrorState error={agents.error} onRetry={agents.reload} /></Card>
        : !a ? <Card><Loading /></Card>
        : <AgentCards a={a} />}

      <Card title="Setup" sub="Run in a terminal. TailFlow changes agent config only through these commands, and shows what it will write first.">
        <div style={{ display: 'grid', gap: '8px' }}>
          <Cmd cmd="tailflow agent install claude" />
          <Cmd cmd="tailflow agent install codex" />
          <Cmd cmd="tailflow agent status" />
          <Cmd cmd="tailflow agent uninstall claude" />
        </div>
        <p class="note" style={{ marginTop: '10px' }}>After installing, this page updates once the config is re-read; an agent appears as active only when its first session event is ingested.</p>
      </Card>

      <div class="grid two">
        <Card title="Agent ingestion" sub="Hook calls → spool → this daemon">
          {!a ? <Loading /> : (
            <dl class="kv">
              <Row k="Ingestion">{a.ingest.enabled ? 'enabled' : `disabled${a.ingest.disabled_reason ? ` — ${a.ingest.disabled_reason}` : ''}`}</Row>
              <Row k="Last ingest pass"><Time at={a.ingest.last_pass_at} mode="relative" /></Row>
              <Row k="Events ingested (this run)">{count(a.ingest.events_submitted)}</Row>
              <Row k="Spool backlog" warn={(a.spool?.pending_files ?? 0) > 0}>
                {a.spool ? `${count(a.spool.pending_files)}${a.spool.pending_capped ? '+' : ''} file(s) waiting${a.spool.oldest_pending_at ? ' — oldest ' : ''}` : 'unavailable'}
                {a.spool?.oldest_pending_at && <Time at={a.spool.oldest_pending_at} mode="relative" />}
              </Row>
              <Row k="Spool full" warn={!!a.spool?.full}>{a.spool?.full ? 'yes — new hook calls are counted as lost' : 'no'}</Row>
              <Row k="Rejected inputs" warn={a.ingest.rejected > 0}>{count(a.ingest.rejected)}</Row>
              <Row k="Dropped" warn={a.ingest.events_dropped + a.ingest.overflow > 0}>{count(a.ingest.events_dropped)} events · {count(a.ingest.overflow)} overflowed hook calls</Row>
              <Row k="Corrupt spool files" warn={a.ingest.corrupt_files > 0}>{count(a.ingest.corrupt_files)}</Row>
              <Row k="Excluded by policy">{count(a.ingest.excluded)}</Row>
              {a.ingest.last_error && <Row k="Last error" warn>{a.ingest.last_error}</Row>}
            </dl>
          )}
        </Card>
        <Card title="Storage" sub="Durable history (SQLite)">
          {storage.error ? <ErrorState error={storage.error} /> : !st ? <Loading /> : (
            <dl class="kv">
              <Row k="State" warn={st.enabled && st.state !== 'ok'}>{st.enabled ? st.state : 'disabled'}</Row>
              {st.error && <Row k="Error" warn>{st.error}</Row>}
              {st.path && <Row k="Database"><span class="mono">{st.path}</span></Row>}
              <Row k="Size">{db ? `${bytes(db.file_bytes + db.wal_bytes)} (${bytes(db.used_bytes)} live)` : '—'}</Row>
              <Row k="Events">{db ? count(db.events) : '—'}</Row>
              <Row k="Oldest retained"><Time at={db?.oldest_event_at} mode="full" /></Row>
              <Row k="Retention">{st.retention ? `${st.retention.retention_days ?? 'unlimited'} d · payloads ${st.retention.payload_retention_days} d · cap ${st.retention.max_size_mb} MB` : '—'}</Row>
              <Row k="Last cleanup"><Time at={st.retention?.last_cleanup_at} mode="relative" /></Row>
              <Row k="Full-text search">{st.full_text_search === undefined ? '—' : st.full_text_search ? 'on' : 'off'}</Row>
              <Row k="Queue depth">{h ? count(h.ingestion_queue_depth) : '—'}</Row>
            </dl>
          )}
        </Card>
        <Card title="Completeness" sub="Loss is counted and recorded, never silent">
          {!st ? <Loading /> : (
            <>
              {(loss > 0 || h?.loss_detected) && <Callout tone="error">Events were lost in this run — history is incomplete. Views that cover the affected ranges say so.</Callout>}
              <dl class="kv" style={{ marginTop: '8px' }}>
                <Row k="Loss detected" warn={loss > 0 || !!h?.loss_detected}>{loss > 0 || h?.loss_detected ? 'yes' : 'no'}</Row>
                <Row k="Dropped at live ingestion" warn={liveLoss > 0}>{count(liveLoss)}</Row>
                <Row k="Dropped before disk" warn={(h?.events_dropped_total ?? 0) > 0}>{h ? count(h.events_dropped_total) : '—'}</Row>
                <Row k="Recorded gaps" warn={(db?.gaps ?? 0) > 0}>{db ? count(db.gaps) : '—'}</Row>
                <Row k="Unclean previous runs" warn={(st.recovered_unclean_runs ?? 0) > 0}>{count(st.recovered_unclean_runs ?? 0)}</Row>
                <Row k="Persistence errors" warn={(h?.storage_errors ?? 0) > 0}>{h ? count(h.storage_errors) : '—'}</Row>
                <Row k="Dashboard stream lag">{count(st.ingest?.sse_client_lag_records ?? 0)} lines dropped for slow live viewers</Row>
                {h?.last_error && <Row k="Last error" warn>{h.last_error}</Row>}
              </dl>
            </>
          )}
        </Card>
        <Card title="Privacy" sub="What is captured and stored">
          {!privacy.data || !st ? <Loading /> : (
            <>
              <dl class="kv">
                <Row k="Capture mode">{privacy.data.capture_mode}</Row>
                <Row k="Capture enabled">{privacy.data.capture_enabled ? 'yes' : 'no'}</Row>
                <Row k="Redaction">{privacy.data.redaction_active ? `on (${privacy.data.redaction_rules.length} rules)` : 'off'}</Row>
                <Row k="Lines redacted">{count(st.ingest?.records_redacted ?? 0)}</Row>
                <Row k="Excluded by policy">{count(h?.events_excluded ?? 0)}</Row>
              </dl>
              <p class="note" style={{ marginTop: '10px' }}>Prompt text and tool output are never stored. Redaction is heuristic: {privacy.data.guarantee.replace(/^heuristic: /, '')}. Nothing leaves this machine — the dashboard talks only to this daemon.</p>
            </>
          )}
        </Card>
      </div>
      <Card title="Collectors" sub="Runtime sources this daemon reads" flush>
        {!collectors.data ? <Loading /> : (collectors.data.collectors ?? []).length === 0
          ? <p class="note" style={{ padding: '16px' }}>No collectors reported.</p>
          : (
            <table class="table">
              <thead><tr><th>Name</th><th>Kind</th><th>Status</th><th class="num">Events</th><th>Last event</th></tr></thead>
              <tbody>
                {(collectors.data.collectors ?? []).map(c => (
                  <tr key={c.name}>
                    <td>{c.name}</td><td class="dim">{c.kind}</td>
                    <td><Badge tone={c.collector_status === 'failed' ? 'error' : c.collector_status === 'running' ? 'ok' : 'muted'}>{c.collector_status}</Badge></td>
                    <td class="num">{count(c.events_received)}</td>
                    <td class="dim"><Time at={c.last_event_at} mode="relative" /></td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
      </Card>
    </div>
  )
}

function AgentCards({ a }: { a: AgentsStatus }) {
  return (
    <div class="grid two">
      {a.agents.map(agent => {
        const inst = agent.installation
        const info = inst ? INSTALL_TEXT[inst.state] : INSTALL_TEXT.unknown
        return (
          <Card key={agent.agent} title={<Agent name={agent.agent} />}
            actions={<Badge tone={info.tone} title={info.detail}>{info.label}</Badge>}>
            {inst?.policy_block && <Callout tone="error">Managed policy blocks user hooks: {inst.policy_block}. Nothing will be captured.</Callout>}
            {inst?.warnings.map(w => <Callout key={w} tone="warn">{w}</Callout>)}
            {inst && inst.records.length === 0 && inst.state === 'not_installed' && (
              <p class="note">No install record. Run <code>tailflow agent install {agent.agent === 'codex' ? 'codex' : 'claude'}</code>. Hooks added by hand are not detected here; <code>tailflow agent status</code> checks the default config paths.</p>
            )}
            {inst?.records.map(r => (
              <dl class="kv" key={r.config_path} style={{ marginBottom: '10px' }}>
                <Row k="Config"><span class="mono">{r.config_path}</span></Row>
                <Row k="Installed"><Time at={r.installed_at} mode="full" /> · v{r.tailflow_version}</Row>
                <Row k="Hooks present" warn={r.hooks_missing.length > 0}>{r.hooks_installed.length} of {r.hooks_installed.length + r.hooks_missing.length}{r.hooks_missing.length > 0 && ` — missing: ${r.hooks_missing.join(', ')}`}</Row>
                {r.hooks_disabled && <Row k="Hooks disabled" warn>disableAllHooks is set in this file</Row>}
                {r.hooks_duplicated.length > 0 && <Row k="Duplicated" warn>{r.hooks_duplicated.join(', ')}</Row>}
                <Row k="Hook binary" warn={!r.binary_exists}><span class="mono">{r.binary}</span>{!r.binary_exists && ' — missing: hooks fail to start'}</Row>
                {r.error && <Row k="Error" warn>{r.error}</Row>}
              </dl>
            ))}
            <h4 style={{ fontSize: '12px', margin: '12px 0 4px', color: 'var(--text-muted)' }}>What this integration can see</h4>
            {agent.capabilities.map(c => (
              <div class="cap-row" key={c.activity}>
                <span>{ACTIVITY_NAME[c.activity] ?? c.activity}</span>
                <span><Badge tone={c.level === 'direct' ? 'ok' : c.level === 'partial' ? 'warn' : 'muted'} icon={c.level === 'direct' ? 'check' : c.level === 'partial' ? 'dashed' : 'eyeOff'}>{LEVEL_TEXT[c.level]}</Badge></span>
                <span class="note">{c.note}</span>
              </div>
            ))}
          </Card>
        )
      })}
    </div>
  )
}
