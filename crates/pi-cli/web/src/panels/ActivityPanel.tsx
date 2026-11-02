import { safeText } from '../redact';
import {
  activityEntryMatchesPage,
  type ActivityEntry,
  type ActivityNavigationState,
  type ActivityPanelPage,
} from './todoActivity';

export type {
  ActivityEntry,
  ActivityNavigationState,
  ActivityPanelPage,
} from './todoActivity';

export interface ActivityPanelProps {
  activity: ActivityEntry[];
  navigation?: ActivityNavigationState;
  onNavigationChange?: (navigation: ActivityNavigationState) => void;
  onClose: () => void;
}

export function ActivityPanel({
  activity,
  navigation = { page: 'recent' },
  onNavigationChange,
  onClose,
}: ActivityPanelProps) {
  const entries = activity.filter((entry) => activityEntryMatchesPage(entry, navigation.page)).slice().reverse();
  const selectPage = (page: ActivityPanelPage) => onNavigationChange?.({ page });
  return (
    <aside id="activity-panel" className="todo-panel" aria-label="Activity panel">
      <div className="todo-panel__head">
        <span className="todo-panel__title">Activity</span>
        <span className="todo-panel__counts" title="bounded ordinary activity">
          {activity.length} event{activity.length === 1 ? '' : 's'}
        </span>
        <button
          id="activity-close-btn"
          type="button"
          className="todo-panel__close"
          title="Close activity panel"
          aria-label="Close activity panel"
          onClick={onClose}
        >
          ×
        </button>
      </div>
      <div className="subagents-panel__filter" role="tablist" aria-label="Activity filter">
        <button
          type="button"
          role="tab"
          aria-selected={navigation.page === 'recent'}
          className={`subagents-panel__filter-btn${navigation.page === 'recent' ? ' is-active' : ''}`}
          onClick={() => selectPage('recent')}
        >
          Recent
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={navigation.page === 'settled'}
          className={`subagents-panel__filter-btn${navigation.page === 'settled' ? ' is-active' : ''}`}
          onClick={() => selectPage('settled')}
        >
          Settled
        </button>
      </div>
      <div className="todo-panel__phases" role="tabpanel">
        {entries.length === 0 ? (
          <div className="todo-panel__empty">
            {navigation.page === 'settled'
              ? 'No settled ordinary activity yet.'
              : 'No ordinary activity yet.'}
          </div>
        ) : (
          entries.map((entry) => (
            <section className="todo-phase" key={entry.id} data-sequence={entry.sequence}>
              <h4 className="todo-phase__name">
                {safeText(entry.actor)}
                <span className="todo-phase__counts">{safeText(entry.status)} · event {entry.sequence}</span>
              </h4>
              <div className="todo-detail__content">{safeText(entry.summary)}</div>
            </section>
          ))
        )}
      </div>
    </aside>
  );
}
