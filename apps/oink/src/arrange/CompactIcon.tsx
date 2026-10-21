import { Icon } from 'oink-ui';

/*
 * Auto-compact: two arrows folding in on a dashed centre line.
 * Adapted from Lucide's "fold-horizontal" icon (lucide-react 1.31.0, ISC,
 * Copyright (c) 2026 Lucide Icons and Contributors; https://lucide.dev),
 * scaled from its 24px grid to oink-ui's 20px <Icon> (1.8 stroke, round caps and joins)
 * with the ends snapped to the grid. Attribution: licenses/native-inventory.json
 * (asset "lucide-fold-horizontal"), which feeds NOTICE, THIRD_PARTY_LICENSES.md and About.
 * The inventory pins this file's SHA-256: regenerate the attribution after editing it.
 */
export const CompactIcon = () => <Icon>
  <path d="M2 10h5M18 10h-5"/>
  <path d="M10 2v1.2M10 6.9v1.2M10 11.9v1.2M10 16.8v1.2"/>
  <path d="M15.5 7.5 13 10l2.5 2.5M4.5 12.5 7 10 4.5 7.5"/>
</Icon>;
