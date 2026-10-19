import { tr } from '../i18n';
import { SearchInput } from 'oink-ui';
import type { Platform } from '../window/DesktopWindow';
import { searchSections, sectionsFor } from './schema';
import type { SectionId } from './schema';
export function SettingsNav({ platform, query, onQuery, active, onJump }: {
    platform: Platform;
    query: string;
    onQuery: (q: string) => void;
    active: SectionId;
    onJump: (id: SectionId) => void;
}) {
    const found = searchSections(platform, query);
    return (<div className="setnav">
      <SearchInput size="sm" className="setnav__search" placeholder={tr('搜索设置')} aria-label={tr('搜索设置')} value={query} onChange={onQuery} onSearch={() => { const first = sectionsFor(platform).find((s) => found.has(s.id)); if (first)
        onJump(first.id); }}/>
      <nav aria-label={tr('设置章节')}>
        <ul className="setnav__list">
          {sectionsFor(platform).map((s) => {
            const hit = found.has(s.id);
            return (<li key={s.id}>
                <button type="button" className="setnav__item ok-focusable" aria-current={active === s.id && hit ? 'true' : undefined} data-miss={hit ? undefined : ''} disabled={!hit} onClick={() => onJump(s.id)}>
                  <span className="setnav__icon">{s.icon}</span>
                  <span className="setnav__name">{s.title}</span>
                </button>
              </li>);
        })}
        </ul>
      </nav>
    </div>);
}
