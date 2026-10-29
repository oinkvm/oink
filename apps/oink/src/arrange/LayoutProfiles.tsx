import { useEffect, useState } from 'react';
import { Button, Dialog, DropdownMenu, Field, Icon, Input, MenuItem, MenuLabel, MenuRadioGroup, MenuRadioItem, MenuSeparator, Tooltip, confirm } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import type { LayoutProfilesState, ProfileSummary } from '../api/layoutProfiles';
import { tr } from '../i18n';
import { nameProblem, profileName } from './snapshot';

const contained = { contained: true } as Partial<DialogProps>;

/**
 * The current layout profile in the arrange toolbar: a dropdown of profiles in the
 * backend's most-recent-use order and a `+` that creates one. There is no Save:
 * manual changes auto-save into the current profile. The backend decides which
 * actions are enabled; this only presents them.
 */
export function ProfileTools({ state, busy, onLoad, onCreate, onManage }: {
  state: LayoutProfilesState | null;
  busy: boolean;
  onLoad: (profile: ProfileSummary) => void;
  onCreate: () => void;
  onManage: () => void;
}) {
  const ready = state?.readiness === 'ready' && !busy;
  const active = state?.profiles.find(p => p.id === state.active_profile_id);
  const name = active ? profileName(active.name) : '';
  const [open, setOpen] = useState(false);
  return <span className="canvas-tools__group" role="group" aria-label={tr('布局')}>
    <DropdownMenu placement="bottom-start" matchTriggerWidth={false} className="lp-menu" open={open} onOpenChange={setOpen}
        trigger={<Button variant="ghost" size="sm" className="lp-current" aria-label={tr('当前布局：{0}', name)} disabled={!ready || !state?.profiles.length}>
          <span className="lp-current__name">{name}</span><Icon className="canvas-tools__chevron" name="chevron-down"/>
        </Button>}>
        <MenuLabel>{tr('布局')}</MenuLabel>
        <MenuRadioGroup value={state?.active_profile_id ?? null} aria-label={tr('布局')}>
          {state?.profiles.map(p => <MenuRadioItem key={p.id} value={p.id} closeOnSelect title={profileName(p.name)} onSelect={() => { if (!p.active) onLoad(p); }}>
            <span className="lp-menu__name">{profileName(p.name)}</span>
          </MenuRadioItem>)}
        </MenuRadioGroup>
        <MenuSeparator/>
        <MenuItem onSelect={onManage}>{tr('管理布局…')}</MenuItem>
      </DropdownMenu>
    <Tooltip interactive={false} placement="bottom" content={state && !state.create_enabled && state.profiles.length >= 16 ? tr('最多保存 16 个布局') : tr('新建布局')}>
      <Button variant="ghost" size="sm" iconOnly aria-label={tr('新建布局')} disabled={!ready || !state?.create_enabled} onClick={onCreate}><Icon name="plus"/></Button>
    </Tooltip>
  </span>;
}

/** The names a new or renamed profile must differ from, as the backend compares them. */
const takenNames = (state: LayoutProfilesState | null, except?: string) =>
  (state?.profiles ?? []).filter(p => p.id !== except).map(p => p.name.kind === 'default' ? 'Default layout' : p.name.value);

/** Asks for a new profile's name; the reply is a refusal text, kept with the name, or null. */
export function CreateProfileDialog({ open, state, container, onClose, onSubmit }: {
  open: boolean;
  state: LayoutProfilesState | null;
  container: HTMLElement | null;
  onClose: () => void;
  onSubmit: (name: string) => Promise<string | null>;
}) {
  const [name, setName] = useState('');
  const [typed, setTyped] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);
  const [pending, setPending] = useState(false);
  const [created, setCreated] = useState(false);
  useEffect(() => { if (open) { setName(''); setTyped(false); setRefusal(null); setCreated(false); } }, [open]);
  // The created profile reaches `state` before the dialog finishes closing; its own
  // name must not then read as a duplicate. Validate only while the name is editable.
  const problem = pending || created ? null : nameProblem(name, takenNames(state));
  const submit = async () => {
    setTyped(true);
    if (problem || pending || created) return;
    setPending(true);
    try {
      const refused = await onSubmit(name.trim());
      if (refused) setRefusal(refused); else { setCreated(true); onClose(); }
    } finally { setPending(false); }
  };
  return <Dialog {...contained} open={open} onOpenChange={o => { if (!o) onClose(); }} container={container} size={400} as="form" onSubmit={() => { void submit(); }}
    title={tr('新建布局')} description={tr('保存当前排列为新布局，并切换到它')}
    actions={<><Button onClick={onClose}>{tr('取消')}</Button><Button type="submit" variant="accent" loading={pending} disabled={!!problem}>{tr('新建')}</Button></>}>
    <Field label={tr('布局名称')} error={refusal ?? (typed ? problem : null)}>
      <Input value={name} autoFocus placeholder={tr('例如：办公室')} onChange={v => { setName(v); setTyped(true); setRefusal(null); }}/>
    </Field>
  </Dialog>;
}

/** Settings › Layout: the profiles in MRU order with the current one marked; rename
 * inline and delete after confirmation (never the last one). */
export function ProfileList({ state, onRename, onDelete }: {
  state: LayoutProfilesState | null;
  onRename: (profile: ProfileSummary, name: string) => Promise<string | null>;
  onDelete: (profile: ProfileSummary) => Promise<void>;
}) {
  const [editing, setEditing] = useState<{ id: string; name: string; refusal: string | null } | null>(null);
  const [pending, setPending] = useState<string | null>(null);
  const profiles = state?.profiles ?? [];
  const ready = state?.readiness === 'ready';
  const rename = async (p: ProfileSummary) => {
    if (!editing || nameProblem(editing.name, takenNames(state, p.id)) || pending) return;
    setPending(p.id);
    try {
      const refusal = await onRename(p, editing.name.trim());
      setEditing(refusal ? { ...editing, refusal } : null);
    } finally { setPending(null); }
  };
  const remove = async (p: ProfileSummary) => {
    const name = profileName(p.name);
    if (!await confirm({ title: tr('删除布局「{0}」？', name), description: tr('删除后无法恢复，当前的屏幕排列不会改变。'), confirmText: tr('删除'), tone: 'danger' })) return;
    setPending(p.id);
    try { await onDelete(p); } finally { setPending(null); }
  };
  const last = profiles.length <= 1;
  return <div className="lpm-panel">
    <ul className="lpm" aria-label={tr('布局')}>
      {profiles.map(p => {
        const name = profileName(p.name);
        const edit = editing?.id === p.id ? editing : null;
        const problem = edit && (edit.refusal ?? nameProblem(edit.name, takenNames(state, p.id)));
        return <li key={p.id} className="lpm__item" data-active={p.active ? '' : undefined}>
          {edit ? <form className="lpm__edit" onSubmit={e => { e.preventDefault(); void rename(p); }}>
            <Field className="lpm__field" error={problem}>
              <Input size="sm" value={edit.name} autoFocus aria-label={tr('布局名称')}
                onChange={v => setEditing({ ...edit, name: v, refusal: null })}
                onKeyDown={e => { if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); setEditing(null); } }}/>
            </Field>
            <Button size="sm" onClick={() => setEditing(null)}>{tr('取消')}</Button>
            <Button size="sm" type="submit" variant="accent" loading={pending === p.id} disabled={!!nameProblem(edit.name, takenNames(state, p.id))}>{tr('保存')}</Button>
          </form> : <>
            <span className="lpm__text">
              <span className="lpm__name" title={name}>{name}</span>
              {p.active && <span className="lpm__note">{tr('当前布局')}</span>}
            </span>
            <Button size="sm" variant="ghost" iconOnly aria-label={tr('重命名「{0}」', name)} disabled={!ready || !!pending}
              onClick={() => setEditing({ id: p.id, name: p.name.kind === 'custom' ? p.name.value : name, refusal: null })}><Icon name="edit"/></Button>
            <Tooltip interactive={false} placement="top" content={last ? tr('至少保留一个布局') : tr('删除「{0}」', name)}>
              <Button size="sm" variant="ghost" iconOnly aria-label={tr('删除「{0}」', name)} loading={pending === p.id} disabled={!ready || !!pending || last}
                onClick={() => { void remove(p); }}><Icon name="trash"/></Button>
            </Tooltip>
          </>}
        </li>;
      })}
    </ul>
  </div>;
}
