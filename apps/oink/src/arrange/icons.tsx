import { Icon } from 'oink-ui';

/*
 * Office-style document glyphs for the saved-layout actions. oink-ui 1.0.5 has no
 * floppy or open-folder icon, so these are drawn here in its icon grammar: the
 * kit's 20×20 <Icon> (currentColor stroke, its stroke width, round caps and joins),
 * 1.5 px corner radii, and the 3–17 live area its `folder`/`download` icons use.
 */

/** Save: a floppy disk with its shutter and label. */
export const SaveIcon = () => <Icon>
  <path d="M5 3.5h8.4l3.1 3.1V15a1.5 1.5 0 0 1-1.5 1.5H5A1.5 1.5 0 0 1 3.5 15V5A1.5 1.5 0 0 1 5 3.5z"/>
  <path d="M7 3.5V7h5.5V3.5M6.5 16.5v-4.3a.7.7 0 0 1 .7-.7h5.6a.7.7 0 0 1 .7.7v4.3"/>
</Icon>;

/** Save as: a smaller floppy with a pencil over its corner. */
export const SaveAsIcon = () => <Icon>
  <path d="M9 14.5H4A1.5 1.5 0 0 1 2.5 13V4.3A1.5 1.5 0 0 1 4 2.8h6.6l2.9 2.9V8"/>
  <path d="M5.5 2.8v3h4.5v-3"/>
  <path d="M10.8 17.2l.6-2.5 4.7-4.7a1.25 1.25 0 0 1 1.8 0l.1.1a1.25 1.25 0 0 1 0 1.8l-4.7 4.7-2.5.6z"/>
</Icon>;

/** Open: a folder with its front flap tilted open. */
export const OpenIcon = () => <Icon>
  <path d="M3.5 15V5.5A1.5 1.5 0 0 1 5 4h3l1.8 2H14a1.5 1.5 0 0 1 1.5 1.5v1"/>
  <path d="M3.5 15l1.9-5.1a1.5 1.5 0 0 1 1.4-1h9.6a.9.9 0 0 1 .85 1.2l-1.7 5a1.5 1.5 0 0 1-1.4 1.05H4.9A1.4 1.4 0 0 1 3.5 15z"/>
</Icon>;
