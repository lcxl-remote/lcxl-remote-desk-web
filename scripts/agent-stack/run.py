#!/usr/bin/env python3
"""Run the agent stack gate with this platform's reviewed ratchet when available."""
import argparse
import json
import os
import subprocess
import sys
import report
from pathlib import Path

HERE = Path(__file__).resolve().parent
REQUIRED = {
    'desk-diagnose-core': ['agent_loop::phases::prepare_iteration', 'PersistedAgentSession', 'future::boxed'],
    'desk-signal': ['ai_assistant_orchestrator::prepare_composed_turn', 'remote_tool_edge::.*semantic',
                    'review_budget::reserve_review_attempt_on', 'approval_reviewer::call_claimed',
                    'assistant_model', 'SignalModelSeam', 'capability_grant_store::task_authority::match_current_on'],
    'lcxl-remote-desk-manager': ['agent_loop_driver::prepare_composed_turn', 'manager_provider_tools::.*prepare_execution',
                    'review_budget::reserve_review_attempt_on', 'agent_approval_reviewer::call_claimed',
                    'agent_model::.*ModelSeam.*call', 'capability_provider_work::dispatch',
                    'capability_grant_store::task_authority::match_current_on'],
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', type=Path, default=HERE.parents[1])
    parser.add_argument('--package', choices=REQUIRED, required=True)
    parser.add_argument('--profile', choices=['debug', 'release'], default='debug')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--previous-ref', help='reviewed baseline Git revision; CI passes the PR base or push-before SHA')
    args = parser.parse_args()
    version = subprocess.check_output(['rustc', '-Vv'], cwd=args.workspace, text=True)
    target = next(line[6:] for line in version.splitlines() if line.startswith('host: '))
    baseline = args.workspace / 'stack-budgets' / f'{args.package}-{target}-{args.profile}.json'
    command = [sys.executable, str(HERE / 'check.py'), '--workspace', str(args.workspace),
               '--package', args.package, '--profile', args.profile, '--ceiling', '65536',
               '--output', str(args.output)]
    for pattern in REQUIRED[args.package]:
        command.extend(['--required', pattern])
    # Compare edits against trusted repository history, before compiling. CI must
    # provide the complete change range so a multi-commit push cannot reset limits.
    previous_ref = args.previous_ref or os.environ.get('AGENT_STACK_BASE_REF') or 'HEAD'
    if set(previous_ref) == {'0'}:
        previous_ref = ''  # First repository push has no predecessor.
    previous = None
    if previous_ref:
        subprocess.run(['git', 'cat-file', '-e', previous_ref + '^{commit}'], cwd=args.workspace, check=True)
        relative = baseline.relative_to(args.workspace).as_posix()
        listed = subprocess.check_output(['git', 'ls-tree', '--name-only', previous_ref, '--', relative],
                                         cwd=args.workspace, text=True)
        if listed.strip():
            previous = json.loads(subprocess.check_output(['git', 'show', f'{previous_ref}:{relative}'],
                                                           cwd=args.workspace, text=True))
    if baseline.exists():
        current = json.loads(baseline.read_text())
        if previous is not None:
            report.compare_baselines(current, previous)
        command.extend(['--baseline', str(baseline)])
    elif previous is not None:
        raise ValueError('reviewed stack baseline was removed')
    else:
        print(f'{target}/{args.profile}: no reviewed ratchet yet; enforcing the 64 KiB ceiling and required families', flush=True)
    return subprocess.call(command)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
