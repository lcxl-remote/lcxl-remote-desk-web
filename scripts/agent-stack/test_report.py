import json
import unittest
import report


def frame_family(name, pattern, **extra):
    return dict(name=name, patterns=[pattern],
                evidence='nested preparation and execution frames in the same reviewed call path', **extra)


def record(size, symbol='_ZN18desk_diagnose_core10agent_loop14run_inner_impl17h0123456789abcdefE'):
    return json.dumps({'message': {'message': f"{size} stack bytes in function '{symbol}'"}})


class Reports(unittest.TestCase):
    def test_portable_ceiling_also_requires_its_critical_families(self):
        result = report.collect([record(65537)])
        self.assertTrue(report.check_ceiling(result, 65536, ['run_inner_impl']))
        self.assertTrue(report.check_ceiling(result, 131072, ['missing']))
        self.assertEqual(report.check_ceiling(result, 131072, ['run_inner_impl']), [])

    def test_llvm_optimized_symbol_suffix(self):
        symbol = '_ZN18desk_diagnose_core10agent_loop14run_inner_impl17h0123456789abcdefE.llvm.123456789'
        self.assertEqual(report.collect([record(32, symbol)])['frames'],
                         {'desk_diagnose_core::agent_loop::run_inner_impl': 32})

    def test_raw_rustc_diagnostics_are_parsed(self):
        diagnostic = json.loads(record(131136))['message']
        self.assertEqual(report.collect([json.dumps(diagnostic)])['records'], 1)

    def test_large_frames_and_duplicate_monomorphizations(self):
        result = report.collect([record(64), 'not JSON', record(131136)])
        self.assertEqual(result['records'], 2)
        self.assertEqual(list(result['frames'].values()), [131136])
        self.assertEqual(list(result['frames']), ['desk_diagnose_core::agent_loop::run_inner_impl'])

    def test_empty_cached_or_changed_format_fails_closed(self):
        for lines in ([], ['{"reason":"compiler-artifact","fresh":true}'], ['{"message":"unknown format"}']):
            with self.assertRaises(ValueError):
                report.collect(lines)

    def test_unknown_symbol_format_fails_closed(self):
        with self.assertRaises(ValueError):
            report.collect([record(123, '_Ragent_loop')])

    def test_new_function_limit_and_existing_ratchet(self):
        result = report.collect([record(131136)])
        metadata = dict(toolchain='rustc test', package='core', profile='debug', target='test')
        result.update(metadata)
        baseline = dict(metadata, required=['run_inner_impl'], frames={}, new_function_limit=65536)
        self.assertTrue(report.check(result, baseline))
        baseline['frames'] = dict(result['frames'])
        self.assertEqual(report.check(result, baseline), [])
        baseline['frames'][next(iter(baseline['frames']))] -= 1
        self.assertTrue(report.check(result, baseline))

    def test_missing_family_and_wrong_profile(self):
        result = report.collect([record(32)])
        metadata = dict(toolchain='rustc test', package='core', profile='debug', target='test')
        result.update(metadata)
        baseline = dict(metadata, required=['missing_family'], frames={}, new_function_limit=65536)
        self.assertTrue(report.check(result, baseline))
        baseline['profile'] = 'release'
        with self.assertRaises(ValueError):
            report.check(result, baseline)

    def test_sequential_extraction_does_not_duplicate_source_budget(self):
        baseline = dict(frames={'mother': 40000, 'one': 180000, 'two': 180000}, milestone_limit=262144,
                        lineages=[dict(source='mother', source_limit=300000,
                            members=['mother', 'one', 'two'], evidence='driver awaits each phase after the previous poll returned',
                            tree={'kind':'sum','terms':[{'frame':'mother'},{'kind':'max','terms':[{'frame':'one'},{'frame':'two'}]}]})])
        self.assertEqual(report.lineage_budgets(baseline), {'mother': 220000})
        baseline['lineages'][0]['tree']['terms'][1]['kind'] = 'sum'
        with self.assertRaises(ValueError):
            report.lineage_budgets(baseline)

    def test_duplicate_path_or_undeclared_members_are_rejected(self):
        baseline = dict(frames={'one': 100}, lineages=[dict(source='old', source_limit=1000,
            members=['one'], evidence='test', tree={'kind':'sum','terms':[{'frame':'one'},{'frame':'one'}]})])
        with self.assertRaises(ValueError):
            report.lineage_budgets(baseline)
        baseline['lineages'][0]['tree'] = {'frame':'one'}
        baseline['lineages'][0]['members'] = ['one','unaccounted']
        with self.assertRaises(ValueError):
            report.lineage_budgets(baseline)

    def test_lineage_cannot_be_removed_to_reset_a_source_budget(self):
        metadata = dict(toolchain='test',target='test',profile='debug',package='test')
        previous = dict(metadata, frames={'one':100}, lineages=[dict(source='old', source_limit=200,
            members=['one'], evidence='test', tree={'frame':'one'})])
        current = dict(metadata, frames={'one':100})
        with self.assertRaises(ValueError):
            report.compare_baselines(current, previous)

    def test_ratchet_cannot_reset_build_options_coverage_or_ceiling(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        previous = dict(metadata, frames={'one': 100}, required=['critical'], new_function_limit=65536)
        for patch in [dict(required=[]), dict(new_function_limit=65537), dict(rustflags='-O'), dict(workspace_context='different-lockfile-context')]:
            with self.assertRaises(ValueError):
                report.compare_baselines(dict(previous, **patch), previous)

    def test_new_extraction_members_require_registration_even_below_the_hard_cap(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        baseline = dict(metadata, frames={'family::one': 100}, required=['family'],
            lineages=[dict(source='old', source_limit=1000, members=['family::one'],
                patterns=['^family::'], evidence='test', tree={'frame': 'family::one'})])
        actual = dict(metadata, frames={'family::one': 100, 'family::two': 200})
        self.assertIn('unregistered extraction member: family::two', report.check(actual, baseline))

    def test_existing_ratchet_cannot_be_raised_by_marking_it_derived(self):
        metadata = dict(toolchain='test',target='test',profile='debug',package='test')
        previous = dict(metadata,frames={'one':100})
        current = dict(metadata,frames={'one':101})
        with self.assertRaises(ValueError):
            report.compare_baselines(current, previous)

    def test_frame_family_allows_redistribution_but_not_growth_or_oversized_members(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        baseline = dict(metadata, required=['turn'], frames={'turn::prepare': 100, 'turn::run': 200},
                        frame_families=[frame_family('turn', '^turn::')])
        actual = dict(metadata, frames={'turn::prepare': 150, 'turn::run': 140})
        self.assertEqual(report.check(actual, baseline), [])
        actual['frames']['turn::run'] = 151
        self.assertTrue(report.check(actual, baseline))
        baseline['frames'] = {'turn::prepare': 65536, 'turn::run': 65536}
        actual['frames'] = {'turn::prepare': 65537, 'turn::run': 1}
        self.assertTrue(report.check(actual, baseline))

    def test_family_adoption_uses_the_exact_historical_scope_and_requires_a_decrease(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        previous = dict(metadata, frames={'turn::old': 300, 'other': 10000})
        current = dict(metadata, frames={'turn::prepare': 120, 'turn::run': 160, 'other': 10000},
                       frame_families=[frame_family('turn', '^turn::')])
        report.compare_baselines(current, previous)
        current['frames']['turn::run'] = 180
        with self.assertRaises(ValueError):
            report.compare_baselines(current, previous)

    def test_family_reallocation_does_not_allow_unrelated_frame_growth(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        previous = dict(metadata, frames={'turn::one': 100, 'turn::two': 200, 'other': 10})
        current = dict(metadata, frames={'turn::one': 150, 'turn::two': 140, 'other': 11},
                       frame_families=[frame_family('turn', '^turn::')])
        with self.assertRaises(ValueError):
            report.compare_baselines(current, previous)
        self.assertTrue(report.check(dict(metadata, frames=current['frames']),
                                     dict(current, required=[], frames=previous['frames'])))

    def test_new_family_members_still_require_registration(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        baseline = dict(metadata, required=[], frames={'turn::one': 200},
                        frame_families=[frame_family('turn', '^turn::')])
        actual = dict(metadata, frames={'turn::one': 100, 'turn::new': 1})
        self.assertIn('unregistered frame family member: turn::new', report.check(actual, baseline))

    def test_family_savings_cannot_hide_a_nested_extraction_lineage_regression(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        baseline = dict(metadata, required=[],
                        frames={'turn::one': 100, 'turn::two': 100, 'turn::other': 200},
                        frame_families=[frame_family('turn', '^turn::')],
                        lineages=[dict(source='old', source_limit=300, members=['turn::one', 'turn::two'],
                            evidence='nested source stages', tree={'kind': 'sum',
                                'terms': [{'frame': 'turn::one'}, {'frame': 'turn::two'}]})])
        actual = dict(metadata, frames={'turn::one': 150, 'turn::two': 80, 'turn::other': 160})
        self.assertTrue(any('extraction lineage budget exceeded' in error
                            for error in report.check(actual, baseline)))

    def test_family_scope_cannot_overlap_or_be_empty(self):
        baseline = dict(frames={'turn::one': 100},
                        frame_families=[frame_family('turn', '^missing::')])
        with self.assertRaises(ValueError):
            report.frame_family_budgets(baseline)
        baseline['frame_families'] = [frame_family('turn', '^turn::'),
                                      frame_family('duplicate', '^turn::')]
        with self.assertRaises(ValueError):
            report.frame_family_budgets(baseline)

    def test_existing_families_cannot_reset_scope_or_total_budget(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        previous = dict(metadata, frames={'turn::one': 100, 'turn::two': 200},
                        frame_families=[frame_family('turn', '^turn::')])
        current = json.loads(json.dumps(previous))
        current['frames'] = {'turn::one': 150, 'turn::two': 140}
        report.compare_baselines(current, previous)
        report.compare_baselines(previous, previous)
        for patch in [dict(frame_families=[]), dict(frames={'turn::one': 150, 'turn::two': 150}),
                      dict(frame_families=[frame_family('turn', '.*')])]:
            with self.assertRaises(ValueError):
                report.compare_baselines(dict(current, **patch), previous)

    def test_removed_old_symbols_cannot_fund_two_new_families(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        previous = dict(metadata, frames={'old': 1000})
        current = dict(metadata, frames={'one': 400, 'two': 400}, frame_families=[
            frame_family('one', '^(old|one)$'),
            frame_family('two', '^(old|two)$')])
        with self.assertRaises(ValueError):
            report.compare_baselines(current, previous)

    def test_excluded_scopes_have_independent_nonoverlapping_budgets(self):
        metadata = dict(toolchain='test', target='test', profile='debug', package='test')
        previous = dict(metadata, frames={'turn::one': 300, 'turn::provider::one': 200})
        current = dict(metadata, frames={'turn::one': 250, 'turn::provider::one': 190}, frame_families=[
            frame_family('turn', '^turn::', exclude_patterns=['::provider::']),
            frame_family('provider', '::provider::')])
        report.compare_baselines(current, previous)
        self.assertEqual(report.frame_family_budgets(current)[0], {'turn': 250, 'provider': 190})


if __name__ == '__main__':
    unittest.main()
