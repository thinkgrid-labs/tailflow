// Wording for evidence statuses (M4). Kept here so every view says the same
// thing, and nothing implies more than the evidence supports.

import type { ChangeEvidence, EvidenceStatus } from './types.ts'
import type { Tone } from './events.ts'

export const EVIDENCE: Record<EvidenceStatus, { label: string; tone: Tone; detail: string }> = {
  matched_observation: {
    label: 'Reported + observed',
    tone: 'ok',
    detail: 'An agent reported this change and the file watcher saw the path change within the window. Association, not proof of authorship.',
  },
  reported_only: {
    label: 'Reported, not seen',
    tone: 'warn',
    detail: 'The agent reported a change, the path was watched with full coverage and no loss, but no change was seen (a no-op write, a failed edit, or outside the window).',
  },
  unverified: {
    label: 'Unverified',
    tone: 'muted',
    detail: 'Reported, but it could not be checked: the path was not watched, coverage was partial, or the watcher lost notifications.',
  },
  filesystem_observed: {
    label: 'Observed, unreported',
    tone: 'info',
    detail: 'The file watcher saw this change and no agent reported it: a person, a build or tool, or an agent without hooks.',
  },
  ambiguous: {
    label: 'Ambiguous',
    tone: 'warn',
    detail: 'One observed change matches reports from more than one session; it cannot be credited to either.',
  },
}

export const CHANGE_EVIDENCE: Record<ChangeEvidence, { label: string; tone: Tone; detail: string }> = {
  reported_by_this_session: { label: 'Reported by this session', tone: 'ok', detail: 'This session reported changing the path between the snapshots. Not proof it made the change.' },
  reported_by_other_sessions: { label: 'Reported by another session', tone: 'info', detail: 'Another session reported changing this path between the snapshots.' },
  reported_by_several_sessions: { label: 'Several sessions', tone: 'warn', detail: 'This and other sessions reported changing the path; it cannot be credited to one.' },
  unreported: { label: 'Unreported', tone: 'muted', detail: 'The path changed but no session reported it: a person, a build or tool, or an agent without hooks.' },
}

export const CHANGE_KIND: Record<string, string> = {
  appeared: 'became different from HEAD',
  changed: 'changed again (was already different)',
  cleared: 'no longer differs from HEAD (reverted or committed)',
}
