"""Compile packaging/update-rules.json (version ranges) into signed v2 build ranges.

Builds encode versions as major*10^7 + minor*10^5 + patch*10^3 + n (n 1..=999).
The publisher appends nothing: the file must end with a catch-all rule.
"""
from pathlib import Path
import re
import sys
import feed

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from identity import encode_build

MAX_BUILD = 2**31 - 1
KINDS = ('none', 'normal', 'critical', 'manual')
MAX_RULES = 64


def version_base(version):
    """Lowest build of a version (n = 0); its builds are base+1 ..= base+999."""
    try:
        return encode_build(version, 999) - 999
    except ValueError:
        raise ValueError('rule_version') from None


def compile_range(text):
    feed.require(isinstance(text, str), 'rule_range')
    text = text.strip()
    if text == '*':
        return [0, MAX_BUILD]
    match = re.fullmatch(r'(<=|>=|<|>)\s*(\S+)', text)
    if match:
        base = version_base(match[2])
        low, high = {'<': (0, base - 1), '<=': (0, base + 999),
                     '>': (base + 1000, MAX_BUILD), '>=': (base, MAX_BUILD)}[match[1]]
    else:
        match = re.fullmatch(r'(\S+)\s+-\s+(\S+)', text)
        feed.require(match is not None, 'rule_range')
        low, high = version_base(match[1]), version_base(match[2]) + 999
    feed.require(0 <= low <= high <= MAX_BUILD, 'rule_range')
    return [low, high]


def compile_rules(source):
    """Version rules from the repository -> build rules, before any release check."""
    feed.require(type(source) is list and 1 <= len(source) <= MAX_RULES, 'rule_count')
    rules = []
    for item in source:
        feed.require(type(item) is dict and {'installed', 'kind'} <= set(item) <=
                     {'installed', 'kind', 'target', 'notice'} and item['kind'] in KINDS, 'rule_shape')
        rule = {'installed': compile_range(item['installed']), 'kind': item['kind']}
        if item['kind'] != 'none':
            target = item.get('target', 'latest')
            feed.require(target == 'latest' or (type(target) is int and 0 < target <= MAX_BUILD), 'rule_target')
            rule['target'] = target
        else:
            feed.require('target' not in item, 'rule_target')
        if 'notice' in item:
            feed.require(item['kind'] == 'critical', 'rule_notice')
            rule['notice'] = feed.validate_notes(item['notice'])
        rules.append(rule)
    validate(rules)
    return rules


def validate(rules, builds=None):
    """Signed-rule invariants; `builds` (releases of one manifest) checks explicit targets."""
    feed.require(type(rules) is list and 1 <= len(rules) <= MAX_RULES, 'rule_count')
    for rule in rules:
        feed.require(type(rule) is dict and rule.get('kind') in KINDS, 'rule_shape')
        expected = {'installed', 'kind'} | ({'target'} if rule['kind'] != 'none' else set()) | \
                   ({'notice'} if 'notice' in rule else set())
        feed.require(set(rule) == expected, 'rule_shape')
        low, high = rule['installed'] if type(rule['installed']) is list and len(rule['installed']) == 2 else (None, None)
        feed.require(type(low) is int and type(high) is int and 0 <= low <= high <= MAX_BUILD, 'rule_range')
        if 'notice' in rule:
            feed.require(rule['kind'] == 'critical', 'rule_notice')
            feed.validate_notes(rule['notice'])
        if 'target' in rule:
            target = rule['target']
            feed.require(target == 'latest' or (type(target) is int and (builds is None or target in builds)), 'rule_target')
    feed.require(rules[-1]['installed'] == [0, MAX_BUILD], 'rule_catch_all')
    return rules


def load(path=Path(__file__).resolve().parents[2] / 'packaging/update-rules.json'):
    # Trusted default-branch checkout; renewal re-signs when only rules change.
    return compile_rules(feed.read(path))
