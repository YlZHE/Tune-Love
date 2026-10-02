"""Real IPC/process regression using only the independent reference fixture.

Run after reference/build.ps1 -Target all. Each case owns one child, verifies
its executable/creation time, sends quit and requires natural exit zero.
No commercial plugin or existing DAW is opened by this suite.
"""
from __future__ import annotations

import argparse
import copy
import json
import os
from pathlib import Path
import queue
import struct
import subprocess
import sys
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT.parent))
from reference import client

BUILD = ROOT / 'build' / 'x64'
HOST = BUILD / 'reference_host.exe'
MAIN_PLUGIN = BUILD / 'ReferenceFixture.vst3'
ALT_PLUGIN = BUILD / 'ReferenceFixtureAlt.vst3'
AGENT = BUILD / 'reference_agent.dll'
FAULT_AGENT = BUILD / 'reference_agent_fault.dll'
EVIDENCE = []
RESIDUAL = []
TITLES = ['Retune Speed', 'Flex-Tune', 'Natural Vibrato', 'Humanize', 'Key', 'Modern Scale']


def make_profile(path, klass, name):
    if klass['titles'] != TITLES or len(klass['ids']) != 6:
        raise AssertionError(f'Unexpected fixture metadata: {klass}')
    return {
        'schema_version': 1, 'profile_id': name, 'architecture': 'x64',
        'plugin_file': str(path), 'sha256': client.file_sha256(path), 'cid': klass['cid'],
        'free_component': True,
        'valid_params': [{'id': identifier, 'title': title} for identifier, title in zip(klass['ids'], klass['titles'])],
        'roles': {role: {'id': identifier, 'title': title, 'domain': 'normalized'}
                  for role, identifier, title in zip(client.ROLES, klass['ids'], klass['titles'])},
    }


def float32(value):
    return struct.unpack('<f', struct.pack('<f', value))[0]


class FixtureHost:
    def __init__(self, preload=True, env=None):
        if RESIDUAL:
            raise RuntimeError(f'Prior fixture did not exit; refusing another child: {RESIDUAL}')
        command = [str(HOST)] + ([str(MAIN_PLUGIN)] if preload else [])
        child_env = dict(os.environ)
        child_env.update(env or {})
        self.process = subprocess.Popen(command, cwd=BUILD, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, text=True, encoding='utf-8', bufsize=1,
                                        creationflags=subprocess.CREATE_NO_WINDOW, env=child_env)
        self.lines = queue.Queue()
        self.reader = threading.Thread(target=self._read_lines, daemon=True)
        self.reader.start()
        self.ready = self.read()
        self.identity = client.process_identity(self.process.pid)
        if Path(self.identity['executable']).resolve() != HOST.resolve() or self.ready['pid'] != self.process.pid:
            raise RuntimeError('Fixture child identity did not match the launched executable')
        self.record = {'pid': self.process.pid, 'identity': self.identity, 'ready': self.ready, 'snapshots': []}
        self.record['environment_overrides'] = dict(env or {})
        self.record['build_sha256_at_start'] = {path.name: client.file_sha256(path) for path in (HOST, MAIN_PLUGIN, ALT_PLUGIN, AGENT, FAULT_AGENT) if path.is_file()}
        EVIDENCE.append(self.record)

    def _read_lines(self):
        for line in self.process.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def read(self, timeout=10):
        try:
            line = self.lines.get(timeout=timeout)
        except queue.Empty as exc:
            raise TimeoutError(f'Fixture PID {self.process.pid} did not reply within {timeout}s') from exc
        if line is None:
            raise RuntimeError(f'Fixture exited unexpectedly: {self.process.poll()}')
        result = json.loads(line)
        if result.get('ok') is not True:
            raise RuntimeError(f'Fixture rejected command: {result}')
        return result

    def command(self, text):
        client._same_identity(self.identity, client.process_identity(self.process.pid))
        self.process.stdin.write(text + '\n')
        self.process.stdin.flush()
        return self.read()

    def snapshot(self, label, controller):
        result = {'label': label, 'host': self.command('status'), 'agent': controller.status()}
        self.record['snapshots'].append(result)
        return result

    def wait_values(self, expected, timeout=12):
        deadline = time.monotonic() + timeout
        latest = None
        while time.monotonic() < deadline:
            latest = self.command('status')
            by_index = {item['index']: item for item in latest['instances']}
            if all(index in by_index and all(abs(actual - float32(wanted)) < 1e-7 for actual, wanted in zip(by_index[index]['values'], values))
                   for index, values in expected.items()):
                return latest
            time.sleep(.05)
        raise AssertionError(f'Fixture values did not reach {expected}: {latest}')

    def wait_agent(self, controller, predicate, timeout=12):
        deadline = time.monotonic() + timeout
        latest = None
        while time.monotonic() < deadline:
            latest = controller.status()
            if predicate(latest):
                return latest
            time.sleep(.05)
        raise AssertionError(f'Agent did not reach expected state: {latest}')

    def close(self):
        try:
            if self.process.poll() is None:
                result = self.command('quit')
                self.record['quit'] = result
            self.process.wait(timeout=10)
            self.record['exit_code'] = self.process.returncode
            if self.process.returncode != 0:
                raise AssertionError(f'Fixture did not exit naturally with code zero: {self.process.returncode}')
        except Exception as exc:
            self.record['cleanup_error'] = str(exc)
            if self.process.poll() is None:
                RESIDUAL.append(self.identity)
            raise
        finally:
            if self.process.poll() is not None:
                self.record['stderr'] = self.process.stderr.read()
                self.process.stdin.close()
                self.process.stdout.close()
                self.process.stderr.close()


@unittest.skipUnless(os.name == 'nt', 'Windows required')
class RuntimeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        missing = [str(path) for path in (HOST, MAIN_PLUGIN, ALT_PLUGIN, AGENT) if not path.is_file()]
        if missing:
            raise unittest.SkipTest('Build the self-owned fixture first: ' + ', '.join(missing))

    def start(self, preload=True, env=None):
        host = FixtureHost(preload, env=env)
        self.addCleanup(host.close)
        return host

    def attach(self, host, module=0, klass=0, ready=None):
        data = ready or host.ready
        info = next(item for item in data['modules'] if item['index'] == module)
        profile = make_profile(info['path'], info['classes'][klass], f'fixture-{module}-{klass}')
        controller = client.Client(host.process.pid, profile, agent_path=AGENT)
        result = controller.attach()
        self.assertFalse(result.get('pending', False))
        return controller

    def assert_host_integrity(self, state):
        for instance in state['instances']:
            self.assertEqual(instance['bad_offsets'], 0)
            self.assertEqual(instance['restoration_errors'], 0)
            self.assertGreater(instance['host_blocks'], 0)

    def test_existing_instances_cache_duplicate_clear_failure_and_late_instance(self):
        host = self.start()
        control = self.attach(host)
        values = [.2, .3, .4, .5, 6 / 11, 5 / 14]
        first = control.apply(dict(zip(client.ROLES, values)), serial=101)
        self.assertEqual(first['stage'], 'cached')
        baseline = host.wait_values({0: values, 1: values})
        self.assert_host_integrity(baseline)
        self.assertTrue(all(item['control_blocks'] == 1 and item['energy'] > 0 for item in baseline['instances']))
        host.snapshot('all_six_existing_instances', control)

        duplicate = control.apply({'retune': .99}, serial=101)
        self.assertTrue(duplicate['duplicate'])
        self.assertEqual(duplicate['cache_revision'], first['cache_revision'])
        unchanged = host.command('status')
        self.assertEqual([item['values'] for item in unchanged['instances']], [item['values'] for item in baseline['instances']])
        replay = control.apply(dict(zip(client.ROLES, values)), serial=102)
        self.assertFalse(replay['duplicate'])
        host.wait_agent(control, lambda state: len(state['instances']) == 2 and all(item['consumed_revision'] == replay['cache_revision'] for item in state['instances']))
        self.assertEqual([item['control_blocks'] for item in host.command('status')['instances']], [2, 2])

        values[0] = .7
        control.apply({'retune': .7}, serial=103)
        host.wait_values({0: values, 1: values})
        group = control.status()['groups'][0]
        self.assertEqual(group['cached_parameters'], 6)
        cleared = control.clear()
        self.assertTrue(cleared['cleared'])
        self.assertFalse(cleared['dsp_reset'])
        self.assertEqual(control.status()['groups'][0]['cached_parameters'], 0)
        values[1] = .6
        control.apply({'flex': .6}, serial=104)
        host.wait_values({0: values, 1: values})
        self.assertEqual(control.status()['groups'][0]['cached_parameters'], 1)

        new_index = host.command('add 0 0')['index']
        self.assertEqual(new_index, 2)
        host.wait_values({new_index: [0, .6, 0, 0, 0, 0]})
        host.snapshot('new_instance_receives_current_cache', control)

        host.command('fail 0 1')
        failed_batch = control.apply({'retune': .9}, serial=105)
        host.wait_agent(control, lambda state: any(item['consumed_revision'] == failed_batch['cache_revision'] and item['last_process_result'] != 0 for item in state['instances']))
        failure_state = host.command('status')
        failed_instance = next(item for item in failure_state['instances'] if item['index'] == 0)
        self.assertGreater(failed_instance['failures'], 0)
        self.assertAlmostEqual(failed_instance['values'][0], float32(.7))
        host.command('fail 0 0')
        after_recovery = host.command('status')
        self.assertAlmostEqual(next(item for item in after_recovery['instances'] if item['index'] == 0)['values'][0], float32(.7))
        control.apply({'retune': .9}, serial=106)
        values[0] = .9
        host.wait_values({0: values, 1: values, 2: [.9, .6, 0, 0, 0, 0]})
        host.command('drop 1')
        host.wait_agent(control, lambda state: len(state['instances']) == 2)
        final = host.snapshot('failure_consumed_without_retry_and_drop', control)
        self.assert_host_integrity(final['host'])

    def test_shared_process_entry_and_two_modules_keep_separate_profiles(self):
        host = self.start()
        main = self.attach(host)
        sibling_index = host.command('add 0 1')['index']
        sibling = self.attach(host, klass=1)
        alt_ready = host.command(f'load {ALT_PLUGIN}')
        alt = self.attach(host, module=1, ready=alt_ready)
        self.assertEqual(len({main.group, sibling.group, alt.group}), 3)
        main.apply({'retune': .2}, serial=201)
        sibling.apply({'retune': .8}, serial=202)
        alt.apply({'retune': .6}, serial=203)
        observed = host.wait_values({0: [.2, 0, 0, 0, 0, 0], 1: [.2, 0, 0, 0, 0, 0],
                                     sibling_index: [.8, 0, 0, 0, 0, 0], 3: [.6, 0, 0, 0, 0, 0]})
        self.assert_host_integrity(observed)
        ready = host.wait_agent(main, lambda state: len(state['instances']) == 4 and all(item['state'] == 'matched' for item in state['instances']))
        self.assertEqual({item['group'] for item in ready['instances']}, {main.group, sibling.group, alt.group})
        host.snapshot('shared_entry_different_ids_and_second_module', main)
        changed = copy.deepcopy(main.profile.data)
        changed['valid_params'][0]['title'] = 'RetuneSpeed'
        repeated = client.Client(host.process.pid, changed, agent_path=AGENT)
        reused = repeated.attach()
        self.assertTrue(reused['already_prepared'])
        self.assertEqual(repeated.group, main.group)
        self.assertEqual(len(repeated.status()['groups']), 3)
        repeated.apply({'retune': .25}, serial=204)
        fresh = host.command('add 0 0')['index']
        host.wait_values({0: [.25, 0, 0, 0, 0, 0], 1: [.25, 0, 0, 0, 0, 0],
                          fresh: [.25, 0, 0, 0, 0, 0], sibling_index: [.8, 0, 0, 0, 0, 0],
                          3: [.6, 0, 0, 0, 0, 0]})
        host.snapshot('duplicate_registration_keeps_original_candidate_for_fresh_instance', main)

    def test_early_attach_waits_for_selected_module_then_registers(self):
        host = self.start(preload=False)
        known_class = {'cid': '0a000f161d242b323940474e555c636a', 'ids': [4, 90, 62, 61, 17, 162], 'titles': TITLES}
        profile = make_profile(MAIN_PLUGIN, known_class, 'fixture-pending')
        controller = client.Client(host.process.pid, profile, agent_path=AGENT)
        pending = controller.attach()
        self.assertTrue(pending['pending'])
        self.assertEqual(pending['reason'], 'plugin_not_loaded')
        self.assertFalse(any(item['name'].lower() == 'reference_agent.dll' for item in client.modules(host.process.pid)))
        ready = host.command(f'load {MAIN_PLUGIN}')
        self.assertEqual(ready['modules'][0]['classes'][0], known_class)
        attached = controller.attach()
        self.assertIn('group', attached)
        controller.apply({'humanize': .45}, serial=301)
        host.wait_values({0: [0, 0, 0, .45, 0, 0]})
        host.snapshot('explicit_target_early_attach', controller)

    def test_later_profile_preserves_unmatched_and_new_instance_can_match(self):
        host = self.start()
        main = self.attach(host)
        sibling_index = host.command('add 0 1')['index']
        before = host.wait_agent(main, lambda state: len(state['instances']) == 3 and any(item['state'] == 'unmatched' for item in state['instances']))
        self.assertEqual(sum(item['state'] == 'unmatched' for item in before['instances']), 1)
        sibling = self.attach(host, klass=1)
        sibling.apply({'vibrato': .65}, serial=401)
        state = main.status()
        self.assertEqual(sum(item['state'] == 'unmatched' for item in state['instances']), 1)
        fresh_index = host.command('add 0 1')['index']
        host.wait_values({fresh_index: [0, 0, .65, 0, 0, 0], sibling_index: [0, 0, 0, 0, 0, 0]})
        state = main.status()
        self.assertEqual(sum(item['state'] == 'matched' for item in state['instances']), 3)
        self.assertEqual(sum(item['state'] == 'unmatched' for item in state['instances']), 1)
        host.snapshot('late_profile_preserves_denied_and_matches_fresh_instance', main)

        fresh_index = host.command('add 0 0')['index']
        host.wait_agent(main, lambda state: len(state['instances']) == 5, timeout=2)
        host.command(f'drop {fresh_index}')
        retired = host.wait_agent(main, lambda state: len(state['instances']) == 4, timeout=2)
        self.assertEqual(len(retired['instances']), 4)
        host.snapshot('drop_before_four_second_identification_retires_record', main)

    def test_new_empty_signature_candidate_preserves_earlier_match(self):
        host = self.start()
        main = self.attach(host)
        main.apply({'retune': .3}, serial=501)
        host.wait_values({0: [.3, 0, 0, 0, 0, 0], 1: [.3, 0, 0, 0, 0, 0]})
        module = host.ready['modules'][0]
        broad = make_profile(module['path'], module['classes'][1], 'fixture-shared-entry-empty-signature')
        broad['valid_params'] = []
        other = client.Client(host.process.pid, broad, agent_path=AGENT)
        other.attach()
        main.apply({'retune': .9}, serial=502)
        state = host.wait_values({0: [.9, 0, 0, 0, 0, 0], 1: [.9, 0, 0, 0, 0, 0]})
        ready = main.status()
        self.assertEqual(len(ready['instances']), 2)
        self.assertTrue(all(item['state'] == 'matched' and item['group'] == main.group for item in ready['instances']))
        self.assert_host_integrity(state)
        host.snapshot('later_empty_signature_preserves_cached_identity', main)

    def no_controller_profile(self, host, empty_signature):
        host.command('drop 0')
        host.command('drop 1')
        index = host.command('add 0 2')['index']
        module = host.ready['modules'][0]
        klass = module['classes'][2]
        self.assertEqual(klass['cid'], '0a020f161d242b323940474e555c636a')
        self.assertEqual(klass['ids'], [4004, 4090, 4062, 4061, 4017, 4162])
        profile = make_profile(module['path'], klass, 'fixture-no-controller')
        if empty_signature:
            profile['valid_params'] = []
        return index, profile

    def test_no_controller_component_accepts_explicit_empty_signature(self):
        host = self.start()
        index, profile = self.no_controller_profile(host, empty_signature=True)
        control = client.Client(host.process.pid, profile, agent_path=AGENT)
        attached = control.attach()
        values = [.15, .25, .35, .45, 6 / 11, 5 / 14]
        control.apply(dict(zip(client.ROLES, values)), serial=601)
        observed = host.wait_values({index: values})
        self.assert_host_integrity(observed)
        matched = host.wait_agent(control, lambda state: len(state['instances']) == 1 and state['instances'][0]['state'] == 'matched')
        self.assertEqual(matched['instances'][0]['group'], attached['group'])
        self.assertEqual(matched['groups'][0]['valid_params'], 0)
        self.assertGreater(observed['instances'][0]['control_blocks'], 0)
        host.snapshot('no_controller_empty_signature_delivers_all_six', control)

    def test_no_controller_component_rejects_nonempty_signature(self):
        host = self.start()
        index, profile = self.no_controller_profile(host, empty_signature=False)
        control = client.Client(host.process.pid, profile, agent_path=AGENT)
        control.attach()
        control.apply(dict(zip(client.ROLES, [.15, .25, .35, .45, 6 / 11, 5 / 14])), serial=602)
        unmatched = host.wait_agent(control, lambda state: len(state['instances']) == 1 and state['instances'][0]['state'] == 'unmatched')
        self.assertEqual(unmatched['instances'][0]['submitted'], 0)
        observed = host.command('status')
        self.assertEqual(len(observed['instances']), 1)
        self.assertEqual(observed['instances'][0]['index'], index)
        self.assertEqual(observed['instances'][0]['values'], [0, 0, 0, 0, 0, 0])
        self.assertEqual(observed['instances'][0]['control_blocks'], 0)
        self.assert_host_integrity(observed)
        host.snapshot('no_controller_nonempty_signature_stays_unmatched', control)

    def test_partial_hook_failure_keeps_control_disabled_and_host_running(self):
        if not FAULT_AGENT.is_file():
            self.skipTest('Build -Target fault to exercise the non-release fault agent')
        host = self.start(env={'ATR_REFERENCE_FAIL_AFTER_HOOKS': '1'})
        module = host.ready['modules'][0]
        profile = client.Profile(make_profile(module['path'], module['classes'][0], 'fixture-hook-failure'))
        control = client.Client(host.process.pid, profile, agent_path=FAULT_AGENT)
        errors = []
        with self.assertRaisesRegex(RuntimeError, 'hook') as initial:
            control.attach()
        errors.append({'operation': 'initial_prepare', 'error': str(initial.exception)})
        requests = [
            ('status', client.encode_request('status')),
            ('prepare_profile', client.encode_request('prepare_profile', cid=profile.cid,
                path=profile.plugin_file, valid_params=profile.valid_params,
                free_component=profile.free_component, vst3_context_not_null=profile.vst3_context_not_null)),
            ('apply', client.encode_request('apply', group=1, serial=701, values=[(4, .9)])),
        ]
        baseline = host.command('status')
        self.assertEqual(len(baseline['instances']), 2)
        initial_blocks = {item['index']: item['blocks'] for item in baseline['instances']}
        for attempt in range(2):
            for operation, packet in requests:
                with self.subTest(attempt=attempt, operation=operation), self.assertRaisesRegex(RuntimeError, 'partial hook') as rejected:
                    client.request(host.process.pid, packet, expected_identity=host.identity)
                errors.append({'operation': operation, 'attempt': attempt, 'error': str(rejected.exception)})
        deadline = time.monotonic() + 2
        observed = baseline
        while time.monotonic() < deadline:
            observed = host.command('status')
            if all(item['blocks'] > initial_blocks[item['index']] + 10 for item in observed['instances']):
                break
            time.sleep(.02)
        self.assertTrue(all(item['blocks'] > initial_blocks[item['index']] + 10 for item in observed['instances']))
        self.assertTrue(all(item['values'] == [0, 0, 0, 0, 0, 0] and item['control_blocks'] == 0 for item in observed['instances']))
        self.assert_host_integrity(observed)
        host.record['snapshots'].append({'label': 'partial_hook_failure_stays_disabled', 'host': observed, 'agent_errors': errors})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--evidence', type=Path)
    args = parser.parse_args()
    if args.evidence and args.evidence.exists():
        parser.error(f'Evidence path already exists; choose a new filename: {args.evidence}')
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(RuntimeTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if args.evidence:
        output = {'ok': result.wasSuccessful(), 'tests_run': result.testsRun, 'failures': len(result.failures),
                  'errors': len(result.errors), 'skipped': len(result.skipped), 'fixtures': EVIDENCE,
                  'residual_processes': RESIDUAL,
                  'failure_details': [{'test': str(test), 'traceback': failure} for test, failure in result.failures + result.errors],
                  'build_sha256': {path.name: client.file_sha256(path) for path in (HOST, MAIN_PLUGIN, ALT_PLUGIN, AGENT, FAULT_AGENT) if path.is_file()}}
        args.evidence.parent.mkdir(parents=True, exist_ok=True)
        with args.evidence.open('x', encoding='utf-8') as evidence_file:
            json.dump(output, evidence_file, indent=2, ensure_ascii=False)
    return 0 if result.wasSuccessful() else 1


if __name__ == '__main__':
    sys.exit(main())
