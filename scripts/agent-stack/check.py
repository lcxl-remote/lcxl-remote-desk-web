#!/usr/bin/env python3
"""Build one protected target with fresh LLVM stack remarks and enforce its baseline."""
import argparse
import hashlib
import re
import json
import os
import subprocess
import sys
import tempfile
import tomllib
import uuid
from pathlib import Path
import report

ROOT = Path(__file__).resolve().parents[2]


def known_overflow_probe(workspace=None):
    with tempfile.TemporaryDirectory(prefix='agent-stack-probe-') as directory:
        source = Path(directory, 'probe.rs')
        source.write_text('fn main() { let data = [0u8; 131072]; std::hint::black_box(&data); }\n')
        result = subprocess.run([
            'rustc', str(source), '--crate-name', 'agent_stack_probe', '--emit=obj',
            '-o', str(Path(directory, 'probe.o')), '-C', 'opt-level=0',
            '-C', 'remark=prologepilog', '--error-format=json',
        ], cwd=workspace, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        if result.returncode:
            raise ValueError('stack analysis positive control did not compile')
        parsed = report.collect(result.stdout.splitlines())
        if max(parsed['frames'].values()) <= 65536:
            raise ValueError('stack analysis did not detect its known oversized frame')


def collect_package(workspace, package, profile, target, features, no_defaults):
    args = ['cargo', 'rustc', '-p', package, '--lib', '--message-format=json']
    if target:
        args.extend(['--target', target])
    if profile == 'release':
        args.append('--release')
    if features:
        args.extend(['--features', features])
    if no_defaults:
        args.append('--no-default-features')
    # An unused, unique cfg changes only the selected rustc invocation, forcing
    # codegen without deleting package artifacts or invalidating dependencies.
    nonce = f'desk_stack_codegen_{uuid.uuid4().hex}'
    args.extend(['--', '--cfg', nonce, '-C', 'remark=prologepilog'])
    state = {'finished': False, 'fresh_artifact': False}
    with subprocess.Popen(args, cwd=workspace, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                          text=True, bufsize=1) as process:
        def lines():
            for line in process.stdout:
                try:
                    item = json.loads(line)
                except ValueError:
                    if 'stack bytes' not in line:
                        print(line.rstrip(), file=sys.stderr)
                    yield line
                    continue
                if item.get('reason') == 'build-finished':
                    state['finished'] = item.get('success') is True
                if item.get('reason') == 'compiler-artifact' and item.get('target', {}).get('name') == package.replace('-', '_'):
                    state['fresh_artifact'] = item.get('fresh') is False
                    state['cargo_profile'] = item.get('profile', {})
                    state['resolved_features'] = item.get('features', [])
                diagnostic = item.get('message')
                if isinstance(diagnostic, dict) and diagnostic.get('level') in ('error', 'warning'):
                    print(diagnostic.get('rendered', diagnostic.get('message', '')), file=sys.stderr)
                yield line
        parsed = report.collect(lines())
        code = process.wait()
    if code or not state['finished'] or not state['fresh_artifact']:
        raise ValueError('analysis requires a successful, freshly code-generated selected artifact')
    parsed.update(cargo_profile=state['cargo_profile'], resolved_features=state['resolved_features'])
    return parsed


def build_configuration(workspace):
    # Record configuration that can affect codegen. Registry-only user
    # configuration does not change this build's stack layout.
    configurations = []
    workspace = workspace.resolve()
    cargo_home = Path(os.environ.get('CARGO_HOME', str(Path.home() / '.cargo')))
    for directory in [cargo_home, *reversed([workspace, *workspace.parents])]:
        for name in ('config.toml', 'config'):
            path = directory / name if directory == cargo_home else directory / '.cargo' / name
            if not path.is_file():
                continue
            data = tomllib.loads(path.read_text())
            relevant = {key: data[key] for key in ('build', 'target', 'profile') if data.get(key)}
            if 'target' in relevant:
                relevant['target'] = {key: value for key, value in relevant['target'].items() if value}
                if not relevant['target']:
                    del relevant['target']
            # Ignore registry/network/env-only settings. The same complete
            # codegen table repeated in ancestor and child configs is recorded
            # once; neither path names nor unrelated auth settings are budgets.
            if relevant and relevant not in configurations:
                configurations.append(relevant)

    environment = {key: value for key, value in os.environ.items()
        if key.startswith(('CARGO_PROFILE_', 'CARGO_TARGET_')) and key.endswith(('OPT_LEVEL','LTO','CODEGEN_UNITS','PANIC','DEBUG','DEBUG_ASSERTIONS','OVERFLOW_CHECKS','RUSTFLAGS','RUSTDOCFLAGS','TARGET_FEATURES'))}
    manifest = tomllib.loads((workspace / "Cargo.toml").read_text())
    return dict(configurations=configurations, environment=environment, manifest_profiles=manifest.get("profile", {}))


def workspace_identity(workspace):
    workspace = workspace.resolve()
    return dict(workspace_context='manager-workspace' if (workspace / 'backend/manager').is_dir() else 'oss-workspace',
                lockfile_sha256=hashlib.sha256((workspace / 'Cargo.lock').read_bytes()).hexdigest())


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--workspace', type=Path, default=ROOT)
    p.add_argument('--package', required=True)
    p.add_argument('--profile', choices=('debug', 'release'), default='debug')
    p.add_argument('--target')
    p.add_argument('--features', default='')
    p.add_argument('--no-default-features', action='store_true')
    p.add_argument('--baseline', type=Path)
    p.add_argument('--ceiling', type=int, help='enforce a portable hard ceiling, including before a platform baseline is reviewed')
    p.add_argument('--required', action='append', default=[], help='required symbol family regex; repeat as needed')
    p.add_argument('--previous-baseline', type=Path, help='compare an edited baseline with the reviewed previous version')
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--report-only', action='store_true', help='collect evidence for review; does not enforce a baseline')
    a = p.parse_args()
    toolchain = subprocess.check_output(['rustc', '-Vv'], cwd=a.workspace, text=True).strip()
    target = a.target or next(line[6:] for line in toolchain.splitlines() if line.startswith('host: '))
    if a.baseline is None and a.ceiling is None and not a.report_only:
        p.error('--baseline or --ceiling is required for enforcement')
    known_overflow_probe(a.workspace)
    parsed = collect_package(a.workspace, a.package, a.profile, a.target, a.features, a.no_default_features)
    parsed.update(**workspace_identity(a.workspace))
    parsed.update(package=a.package, profile=a.profile, target=target, toolchain=toolchain,
                  features=a.features, no_default_features=a.no_default_features,
                  rustflags=os.environ.get('RUSTFLAGS', ''),
                  encoded_rustflags=os.environ.get('CARGO_ENCODED_RUSTFLAGS', ''),
                  build_configuration=build_configuration(a.workspace))
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(json.dumps(parsed, indent=2) + '\n')
    errors = []
    if not a.report_only and a.baseline:
        baseline = json.loads(a.baseline.read_text())
        if a.previous_baseline:
            report.compare_baselines(baseline, json.loads(a.previous_baseline.read_text()))
        errors.extend(report.check(parsed, baseline))
    if not a.report_only and a.ceiling is not None:
        errors.extend(report.check_ceiling(parsed, a.ceiling, a.required))
    if errors:
        print('\n'.join(errors), file=sys.stderr)
        return 1
    print(f'{a.package} {target} {a.profile}: {parsed["records"]} records, {len(parsed["frames"])} protected entries')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
