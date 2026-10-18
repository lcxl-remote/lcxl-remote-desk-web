#!/usr/bin/env python3
"""Stream rustc stack remarks into a compact, deterministic report."""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

REMARK = re.compile(r"(\d+) stack bytes in function '([^']+)'")
ESCAPES = {'SP': '@', 'BP': '*', 'RF': '&', 'LT': '<', 'GT': '>', 'LP': '(', 'RP': ')', 'C': ','}


def symbol_key(symbol):
    if not symbol.startswith(('_ZN', '__ZN')):
        raise ValueError(f'unsupported Rust symbol format: {symbol[:1000]}')
    text = symbol[symbol.index('ZN') + 2:]
    parts = []
    while text and not re.fullmatch(r'E(?:\.llvm\.[0-9A-Fa-f]+)?', text):
        n = re.match(r'\d+', text)
        if n is None:
            raise ValueError(f'invalid legacy symbol: {symbol[:1000]}')
        length = int(n[0])
        text = text[len(n[0]):]
        if len(text) < length:
            raise ValueError('truncated legacy symbol')
        parts.append(text[:length])
        text = text[length:]
    if parts and re.fullmatch(r'h[0-9a-f]{16}', parts[-1]):
        parts.pop()
    name = '::'.join(parts)
    name = re.sub(r'\$([^$]+)\$', lambda m: ESCAPES.get(m[1], chr(int(m[1][1:], 16)) if re.fullmatch(r'u[0-9a-f]+', m[1]) else m[0]), name)
    name = name.replace('..', '::')
    # Generic monomorphizations share one budget, using their largest frame.
    name = re.sub(r'::h[0-9a-f]{16}\b', '', name)
    return name


def protected(symbol):
    return ('desk_diagnose_core' in symbol and 'future' in symbol) or any(word in symbol for word in (
        'agent_loop', 'ai_assistant_orchestrator', 'remote_tool_edge',
        'agent_model', 'assistant_model', 'SignalModelSeam', 'capability_provider_work', 'capability_grant_store',
        'agent_approval_concrete', 'approval_reviewer', 'manager_provider_tools', 'agent_subagent_store',
        'PersistedAgentSession', 'CommandConfirmation', 'ExecInput',
        'agent_stack_probe', 'for_agent_turn',
    ))


def collect(lines):
    frames = {}
    records = 0
    format_errors = []
    for line in lines:
        try:
            item = json.loads(line)
        except ValueError:
            continue
        diagnostic = item['message'] if isinstance(item.get('message'), dict) else item
        if not isinstance(diagnostic, dict):
            continue
        match = REMARK.search(diagnostic.get('message', ''))
        if not match:
            continue
        records += 1
        count, symbol = int(match[1]), match[2]
        if not protected(symbol):
            continue
        try:
            key = symbol_key(symbol)
        except ValueError as error:
            if not format_errors:
                format_errors.append(str(error))
            continue
        frames[key] = max(count, frames.get(key, 0))
    if format_errors:
        raise ValueError(format_errors[0])
    if records == 0 or not frames:
        raise ValueError('no stack records: require fresh codegen and a supported remark format')
    return {'records': records, 'frames': dict(sorted(frames.items()))}


def check_ceiling(report, ceiling, required):
    if ceiling <= 0:
        raise ValueError('stack ceiling must be positive')
    errors = [f'{actual} > {ceiling}: {key}' for key, actual in report['frames'].items() if actual > ceiling]
    for pattern in required:
        if not any(re.search(pattern, key) for key in report['frames']):
            errors.append(f'missing required family: {pattern}')
    return errors


def check(report, baseline):
    for field in ('toolchain', 'target', 'profile', 'package', 'workspace_context'):
        if report.get(field) != baseline.get(field):
            raise ValueError(f'baseline mismatch: {field}')
    errors = []
    frames = report['frames']
    for pattern in baseline['required']:
        if not any(re.search(pattern, key) for key in frames):
            errors.append(f'missing required family: {pattern}')
    limits = baseline['frames']
    lineage_budgets(baseline)
    for lineage in baseline.get('lineages', []):
        members = set(lineage['members'])
        for key in frames:
            if any(re.search(pattern, key) for pattern in lineage.get('patterns', [])) and key not in members:
                errors.append(f'unregistered extraction member: {key}')
    for field, default in (('features', ''), ('no_default_features', False), ('rustflags', ''), ('encoded_rustflags', ''), ('cargo_profile', {}), ('resolved_features', []), ('build_configuration', {})):
        if report.get(field, default) != baseline.get(field, default):
            raise ValueError(f'baseline build options changed: {field}')
    for key, actual in frames.items():
        limit = limits.get(key, baseline.get('new_function_limit', 65536))
        if actual > limit:
            errors.append(f'{actual} > {limit}: {key}')
    return errors


def lineage_budgets(baseline):
    """Account only declared extraction families; this is not a full call graph."""
    allocated = set()
    result = {}

    def cost(node):
        if 'frame' in node:
            key = node['frame']
            return baseline['frames'][key], [{key}]
        children = [cost(child) for child in node['terms']]
        if not children or node.get('kind') not in ('sum', 'max'):
            raise ValueError('invalid lineage budget expression')
        if node['kind'] == 'max':
            return max(value for value, _ in children), [path for _, paths in children for path in paths]
        paths = [set()]
        for _, alternatives in children:
            next_paths = []
            for path in paths:
                for alternative in alternatives:
                    if path & alternative:
                        raise ValueError('lineage budget repeated on the same nested path')
                    next_paths.append(path | alternative)
            paths = next_paths
        return sum(value for value, _ in children), paths

    for lineage in baseline.get('lineages', []):
        total, paths = cost(lineage['tree'])
        members = set().union(*paths)
        if set(lineage.get('members', [])) != members:
            raise ValueError('declared lineage members differ from its budget expression')
        if allocated & members or lineage['source'] in result:
            raise ValueError('source budget allocated more than once')
        allocated.update(members)
        if total > lineage['source_limit']:
            raise ValueError(f'lineage budget expanded: {lineage["source"]}')
        if any(baseline['frames'][key] > baseline.get('milestone_limit', 65536) for key in members):
            raise ValueError('derived frame exceeds its debug milestone')
        if len(members) > 1 and baseline['frames'].get(lineage['source'], 0) >= lineage['source_limit']:
            raise ValueError('mother frame did not decrease during extraction')
        if not lineage.get('evidence'):
            raise ValueError('extraction needs code and poll/drop boundary evidence')
        result[lineage['source']] = total
    return result


def compare_baselines(current, previous):
    for key in ('toolchain', 'target', 'profile', 'package', 'workspace_context', 'features', 'no_default_features',
                'rustflags', 'encoded_rustflags', 'cargo_profile', 'resolved_features', 'build_configuration'):
        if current.get(key) != previous.get(key):
            raise ValueError(f'ratchet configuration changed: {key}')
    if current.get('new_function_limit', 65536) > previous.get('new_function_limit', 65536):
        raise ValueError('new function ceiling increased')
    if not set(previous.get('required', [])).issubset(current.get('required', [])):
        raise ValueError('required symbol coverage removed')
    current_totals = lineage_budgets(current)
    previous_totals = lineage_budgets(previous)
    if not set(previous_totals).issubset(current_totals):
        raise ValueError('existing extraction lineage was removed')
    previous_lineages = {entry['source']: entry for entry in previous.get('lineages', [])}
    for entry in current.get('lineages', []):
        source = entry['source']
        if source in previous_lineages and not set(previous_lineages[source].get('patterns', [])).issubset(entry.get('patterns', [])):
            raise ValueError('extraction namespace coverage removed')
        allowed = previous_totals.get(source, previous['frames'].get(source))
        if allowed is None or current_totals[source] > allowed:
            raise ValueError(f'source family budget increased or was not registered: {source}')
    derived = {key for entry in current.get('lineages', []) for key in entry.get('members', [])}
    for key, maximum in current['frames'].items():
        if key in previous['frames'] and maximum > previous['frames'][key]:
            raise ValueError(f'ratchet budget increased: {key}')
        if key not in previous['frames'] and maximum > 65536 and key not in derived:
            raise ValueError(f'ratchet budget increased: {key}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, help='rustc/Cargo JSON log; stdin when omitted')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--package', required=True)
    parser.add_argument('--profile', choices=['debug', 'release'], required=True)
    parser.add_argument('--target', required=True)
    parser.add_argument('--baseline', type=Path)
    args = parser.parse_args()
    handle = args.input.open() if args.input else sys.stdin
    try:
        report = collect(handle)
    finally:
        if args.input:
            handle.close()
    report.update(package=args.package, profile=args.profile, target=args.target,
                  toolchain=subprocess.check_output(['rustc', '-Vv'], text=True).strip())
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    if args.baseline:
        errors = check(report, json.loads(args.baseline.read_text()))
        if errors:
            print('\n'.join(errors), file=sys.stderr)
            return 1
    print(f'{report["records"]} records; {len(report["frames"])} protected entries')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError) as error:
        sys.exit(str(error))
