#!/usr/bin/env python3
"""Writes the unsigned 「Oink 复制」 shortcut sources, OinkCopy-<colour>.unsigned.plist (XML),
one per Shortcuts tile colour in COLORS.

    python3 -I apps/mobile/ios/Shortcuts/make_shortcut.py [--check]

--check exits 1 when the committed sources differ from what this script writes.
sign-shortcut.sh signs each source into OinkCopy-<colour>.shortcut (bundled by the app);
the app imports the one ACCENTS names for the current theme accent (ShortcutFile.swift).

Two actions: Oink's 「获取最新共享内容」 (App Intent CopyLatestSharedClipboardIntent)
and Apple's 「拷贝到剪贴板」 fed with its result. The action identifier is
<bundle id>.<intent type name>: renaming the Swift type or changing the bundle id
breaks every shortcut built on it ("unknown action"), so both are protocol
constants (packaging/tests/test_ios_shortcut.py checks them against the app).
Format confirmed against a shortcut built in the Shortcuts app on an iOS 26
simulator (2026-11-11), the setclipboard keys from the simulator's ToolKit
database (WFInput, WFLocalOnly).
"""
import plistlib
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
BUNDLE_ID = 'com.hachiwii.oink'
INTENT = 'CopyLatestSharedClipboardIntent'
TEAM = '893V37X2CW'
# Fixed so the source is reproducible; they only link the second action to the first.
GET = '6F2B5C1E-3A4D-4B8E-9C7F-0A1B2C3D4E5F'
SET = '8D9E0F1A-2B3C-4D5E-8F6A-7B8C9D0E1F2A'
INPUT_CLASSES = ['WFAppContentItem', 'WFAppStoreAppContentItem', 'WFArticleContentItem',
                 'WFContactContentItem', 'WFDateContentItem', 'WFEmailAddressContentItem', 'WFFolderContentItem',
                 'WFGenericFileContentItem', 'WFImageContentItem', 'WFiTunesProductContentItem',
                 'WFLocationContentItem', 'WFDCMapsLinkContentItem', 'WFAVAssetContentItem', 'WFPDFContentItem',
                 'WFPhoneNumberContentItem', 'WFRichTextContentItem', 'WFSafariWebPageContentItem',
                 'WFStringContentItem', 'WFURLContentItem']

# The clipboard glyph of Shortcuts' Choose Icon (search "clipboard"); the number is what
# the app stores for it (user choice 2026-11-12).
GLYPH = 61593
# WFWorkflowIconStartColor of the Shortcuts palette colours the accents use. These are
# the values Choose Icon stores, read back from the simulator's Shortcuts database; they
# are palette codes, not the colour drawn (0xFF4351FF draws a coral tile, and the
# palette's green is stored as 0xFFD426FF), so a colour can only be one of the 15
# palette values, never an accent's own RGB.
COLORS = {
    'red': 0xFF4351FF,
    'darkOrange': 0xFD6631FF,
    'violet': 0x7B72E9FF,
    'darkBlue': 0x3871DEFF,
    'teal': 0x19BD03FF,
}
# OinkPalette.Accent → the palette colour whose library tile (mid tone, iOS 27) is closest
# to the accent's base colour in OKLab (ΔE×100: pink 4.9, orange 7.5, violet 7.1, blue 5.7,
# teal 11.6; runners-up red 9.3 for orange, darkBlue 9.6 for violet, blue 7.7 for blue).
# Accent colour values used by the bundled shortcuts.
ACCENTS = {
    'pink': 'red',
    'orange': 'darkOrange',
    'violet': 'violet',
    'blue': 'darkBlue',
    'teal': 'teal',
}


def source_path(color):
    return HERE / f'OinkCopy-{color}.unsigned.plist'


def workflow(color):
    return {
        'WFWorkflowClientVersion': '4018.0.4',
        'WFWorkflowMinimumClientVersion': 900,
        'WFWorkflowMinimumClientVersionString': '900',
        # An imported shortcut always shows this glyph, never Oink's app icon. Shortcuts
        # draws the app icon from a per-shortcut "associated app" in its database, which
        # the file format has no key for (WFWorkflowFile reads only the WFWorkflow* keys
        # here) and which importing never fills in. Only the editor sets it: adding an
        # action when all app actions come from one app, or Choose Icon > Apps.
        # Dropping WFWorkflowIcon (a blank grey tile), a single Oink action, an old
        # client version and a skipped import question all still import without the
        # app icon (iOS 27.0 simulator, 2026-11-12).
        'WFWorkflowIcon': {
            'WFWorkflowIconStartColor': COLORS[color],
            'WFWorkflowIconGlyphNumber': GLYPH,
        },
        'WFWorkflowImportQuestions': [],
        'WFWorkflowInputContentItemClasses': INPUT_CLASSES,
        'WFWorkflowOutputContentItemClasses': [],
        'WFWorkflowTypes': [],
        'WFQuickActionSurfaces': [],
        'WFWorkflowHasOutputFallback': False,
        'WFWorkflowHasShortcutInputVariables': False,
        'WFWorkflowActions': [
            {
                'WFWorkflowActionIdentifier': f'{BUNDLE_ID}.{INTENT}',
                'WFWorkflowActionParameters': {
                    'UUID': GET,
                    'AppIntentDescriptor': {
                        'AppIntentIdentifier': INTENT,
                        'BundleIdentifier': BUNDLE_ID,
                        'Name': 'Oink',
                        # A different team (an ad-hoc build) still resolves the action.
                        'TeamIdentifier': TEAM,
                    },
                },
            },
            {
                'WFWorkflowActionIdentifier': 'is.workflow.actions.setclipboard',
                'WFWorkflowActionParameters': {
                    'UUID': SET,
                    'WFInput': {
                        'Value': {'OutputName': '最新共享内容', 'OutputUUID': GET, 'Type': 'ActionOutput'},
                        'WFSerializationType': 'WFTextTokenAttachment',
                    },
                    'WFLocalOnly': False,
                },
            },
        ],
    }


def source_bytes(color):
    return plistlib.dumps(workflow(color), fmt=plistlib.FMT_XML)


def main(argv):
    if argv == ['--check']:
        stale = [p.name for p in HERE.glob('OinkCopy*.unsigned.plist') if p not in map(source_path, COLORS)]
        changed = [source_path(c).name for c in COLORS
                   if not source_path(c).exists() or source_path(c).read_bytes() != source_bytes(c)]
        if stale or changed:
            print(f'out of date: {", ".join(changed + stale)}: run make_shortcut.py, then sign-shortcut.sh',
                  file=sys.stderr)
            return 1
        return 0
    if argv:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    for path in HERE.glob('OinkCopy*.unsigned.plist'):
        if path not in map(source_path, COLORS):
            path.unlink()
    for color in COLORS:
        source_path(color).write_bytes(source_bytes(color))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
