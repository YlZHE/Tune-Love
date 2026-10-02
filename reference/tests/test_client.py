"""Protocol/profile tests use real encoders and files; only OS boundaries are supplied."""
import copy
import ctypes
from ctypes import wintypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import struct
import tempfile
import unittest
import subprocess
import sys
import threading


SOURCE = Path(__file__).resolve().parents[1] / 'client.py'


def load_client():
    if not SOURCE.exists():
        return None
    spec = importlib.util.spec_from_file_location('reference_client_tested', SOURCE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


client = load_client()


def pe_bytes(machine=0x8664):
    data = bytearray(128)
    data[:2] = b'MZ'
    struct.pack_into('<I', data, 60, 64)
    data[64:68] = b'PE\0\0'
    struct.pack_into('<H', data, 68, machine)
    return bytes(data)


def make_junction(link, target):
    """Directory junction (no admin needed) so a path differs from its resolved form."""
    subprocess.run(['cmd', '/c', 'mklink', '/J', str(link), str(target)],
                   check=True, capture_output=True)


def fixture_profile(path, offset=0):
    return {
        'schema_version': 1, 'profile_id': f'fixture-{offset}',
        'architecture': 'x64', 'plugin_file': str(path),
        'sha256': hashlib.sha256(path.read_bytes()).hexdigest(),
        'cid': '00112233445566778899aabbccddeeff',
        'valid_params': [{'id': 4 + offset, 'title': 'Retune Speed'}, {'id': 90 + offset, 'title': 'Flex-Tune'}],
        'roles': {
            'retune': {'id': 4 + offset}, 'flex': {'id': 90 + offset},
            'vibrato': {'id': 62 + offset}, 'humanize': {'id': 61 + offset},
            'key': {'id': 2 + offset, 'options': {'C': 0.0, 'F#': 6 / 11, 'B': 1.0}},
            'scale': {'id': 162 + offset, 'options': {'Chromatic': 0.0, 'Minor': 2 / 14}},
        },
    }


class ClientTests(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(client, 'reference/client.py must implement the public client protocol')
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.plugin = Path(self.temp.name) / 'fixture.vst3'
        self.plugin.write_bytes(pe_bytes())
        self.profile = fixture_profile(self.plugin)

    def test_apply_wire_uses_uint32_id_float32_and_serial(self):
        packet = client.encode_request('apply', group=7, serial=19, values=[(4, .5), (90, 1)])
        self.assertEqual(packet.hex(), '415452320100030007000000100000001300000000000000040000000000003f5a0000000000803f')

    def test_prepare_uses_raw_cid_then_utf16_path_without_terminator(self):
        packet = client.encode_request('prepare', cid=self.profile['cid'], path='C:\\音色\\a.vst3')
        self.assertEqual(struct.unpack('<IHHIIQ', packet[:24]), (0x32525441, 1, 2, 0, 40, 0))
        self.assertEqual(packet[24:40].hex(), self.profile['cid'])
        self.assertEqual(packet[40:].decode('utf-16-le'), 'C:\\音色\\a.vst3')

    def test_prepare_profile_encodes_instance_signature_and_release_policy(self):
        packet = client.encode_request('prepare_profile', cid=self.profile['cid'], path='C:\\a.vst3',
                                       valid_params=[{'id': 4, 'title': 'Retune Speed'}], free_component=False)
        self.assertEqual(struct.unpack('<IHHIIQ', packet[:24]), (0x32525441, 1, 5, 0, 306, 0))
        self.assertEqual(packet[24:40].hex(), self.profile['cid'])
        self.assertEqual(struct.unpack('<III', packet[40:52]), (0, 18, 1))
        self.assertEqual(packet[52:70].decode('utf-16-le'), 'C:\\a.vst3')
        self.assertEqual(struct.unpack('<I', packet[70:74])[0], 4)
        self.assertEqual(packet[74:330].decode('utf-16-le').rstrip('\0'), 'Retune Speed')

    def test_prepare_profile_context_flag_is_independent_of_free_component(self):
        for free_component, context, expected in ((False, False, 0), (True, False, 1),
                                                   (False, True, 2), (True, True, 3)):
            with self.subTest(free_component=free_component, context=context):
                packet = client.encode_request('prepare_profile', cid=self.profile['cid'], path='C:\\a.vst3',
                                               valid_params=[], free_component=free_component,
                                               vst3_context_not_null=context)
                self.assertEqual(struct.unpack_from('<I', packet, 40)[0], expected)

    def test_profile_context_defaults_false_and_rejects_non_boolean(self):
        self.assertFalse(client.Profile(self.profile).vst3_context_not_null)
        for value in (0, 1, 'false', None):
            data = copy.deepcopy(self.profile)
            data['vst3_context_not_null'] = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, 'vst3_context_not_null'):
                client.Profile(data)
            with self.subTest(wire_value=value), self.assertRaisesRegex(ValueError, 'vst3_context_not_null'):
                client.encode_request('prepare_profile', cid=self.profile['cid'], path='C:\\a.vst3',
                                      valid_params=[], vst3_context_not_null=value)

    def test_attach_passes_profile_context_policy_and_other_ops_reject_it(self):
        control = self.make_client()
        data = copy.deepcopy(self.profile)
        data['vst3_context_not_null'] = True
        control.profile = client.Profile(data)
        control.attach()
        self.assertEqual(struct.unpack_from('<I', self.packets[0], 40)[0], 3)
        for op, kwargs in (('prepare', {'cid': self.profile['cid'], 'path': 'C:\\a.vst3'}),
                           ('apply', {'serial': 1, 'values': [(4, .5)]}), ('status', {}), ('clear', {})):
            with self.subTest(op=op), self.assertRaises(ValueError):
                client.encode_request(op, vst3_context_not_null=True, **kwargs)

    def test_profile_refuses_missing_or_invalid_instance_signature(self):
        for params in ([{'id': 4, 'title': ''}], [{'id': 4, 'title': 'A\0B'}], [{'id': 4, 'title': 'x' * 128}], [{'id': 4, 'title': 'x'}] * 17):
            data = copy.deepcopy(self.profile)
            data['valid_params'] = params
            with self.subTest(params=params), self.assertRaises(ValueError):
                client.Profile(data)
        data = copy.deepcopy(self.profile)
        del data['valid_params']
        with self.assertRaisesRegex(ValueError, 'valid_params'):
            client.Profile(data)

    def test_explicit_empty_signature_supports_separate_controller_components(self):
        data = copy.deepcopy(self.profile)
        data['valid_params'] = []
        profile = client.Profile(data)
        packet = client.encode_request('prepare_profile', cid=profile.cid, path='C:\\a.vst3', valid_params=profile.valid_params)
        self.assertEqual(struct.unpack('<III', packet[40:52]), (1, 18, 0))
        self.assertEqual(len(packet), 70)

    def test_request_rejects_invalid_payloads_before_transport(self):
        cases = [
            ('apply', {'values': []}),
            ('apply', {'serial': 1, 'values': [(4, float('nan'))]}),
            ('apply', {'serial': 1, 'values': [(4, -0.01)]}),
            ('apply', {'serial': 1, 'values': [(4, 1.01)]}),
            ('apply', {'serial': 0, 'values': [(4, .5)]}),
            ('apply', {'serial': 2**64, 'values': [(4, .5)]}),
            ('apply', {'serial': 1, 'values': [(-1, .5)]}),
            ('apply', {'serial': 1, 'values': [(4, True)]}),
            ('apply', {'serial': 1, 'values': [(i, .5) for i in range(257)]}),
            ('clear', {'values': [(4, .5)]}),
            ('status', {'serial': 1}),
            ('prepare', {'cid': self.profile['cid'], 'path': 'relative.vst3'}),
            ('prepare', {'cid': self.profile['cid'], 'path': 'C:\\a\0.vst3'}),
            ('prepare', {'cid': '12', 'path': 'C:\\a.vst3'}),
        ]
        for op, kwargs in cases:
            with self.subTest(op=op, kwargs=kwargs), self.assertRaises(ValueError):
                client.encode_request(op, **kwargs)

    def test_two_profiles_map_same_roles_to_different_ids(self):
        one = client.Profile(self.profile)
        two = client.Profile(fixture_profile(self.plugin, 1000))
        self.assertEqual(one.map_values({'retune': .5, 'key': 'F#'}), [(4, .5), (2, 6 / 11)])
        self.assertEqual(two.map_values({'retune': .5, 'key': 'F#'}), [(1004, .5), (1002, 6 / 11)])

    def test_options_require_known_labels_or_exact_discrete_values(self):
        profile = client.Profile(self.profile)
        for values in ({'key': 'H'}, {'key': .25}, {'retune': '20 ms'}, {'flex': True}, {'retune': float('inf')}):
            with self.subTest(values=values), self.assertRaises(ValueError):
                profile.map_values(values)
        self.assertEqual(profile.map_values({'key': 6 / 11}), [(2, 6 / 11)])

    def test_options_reports_profile_labels_and_normalized_values_without_plugin_file(self):
        profile = client.Profile(self.profile)
        self.plugin.unlink()
        self.assertEqual(profile.options('key'), {
            'ok': True, 'source': 'profile', 'profile_id': 'fixture-0', 'role': 'key', 'id': 2,
            'options': [{'label': 'C', 'normalized': 0.0}, {'label': 'F#', 'normalized': 6 / 11}, {'label': 'B', 'normalized': 1.0}],
        })
        for role in ('retune', 'unknown'):
            with self.subTest(role=role), self.assertRaises(ValueError):
                profile.options(role)

    def test_cli_options_needs_no_pid_or_loaded_plugin(self):
        profile_path = Path(self.temp.name) / 'profile.json'
        profile_path.write_text(json.dumps(self.profile), encoding='utf-8')
        self.plugin.unlink()
        result = subprocess.run([sys.executable, str(SOURCE), '--profile', str(profile_path), 'options', 'scale'],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads(result.stdout), {
            'ok': True, 'source': 'profile', 'profile_id': 'fixture-0', 'role': 'scale', 'id': 162,
            'options': [{'label': 'Chromatic', 'normalized': 0.0}, {'label': 'Minor', 'normalized': 2 / 14}],
        })

    def test_missing_role_is_an_explicit_unsupported_capability(self):
        del self.profile['roles']['flex']
        profile = client.Profile(self.profile)
        with self.assertRaisesRegex(ValueError, 'unsupported.*flex'):
            profile.map_values({'flex': .5})

    def test_profile_rejects_schema_architecture_fingerprint_cid_and_duplicate_ids(self):
        mutations = [('schema_version', 2), ('architecture', 'arm64'), ('sha256', 'abc'), ('cid', '0' * 31)]
        for key, value in mutations:
            data = copy.deepcopy(self.profile)
            data[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                client.Profile(data)
        data = copy.deepcopy(self.profile)
        data['roles']['flex']['id'] = 4
        with self.assertRaises(ValueError):
            client.Profile(data)

    def test_verify_file_rejects_fingerprint_and_pe_architecture_mismatch(self):
        profile = client.Profile(self.profile)
        self.assertEqual(profile.verify_file()['architecture'], 'x64')
        self.plugin.write_bytes(pe_bytes() + b'changed')
        with self.assertRaisesRegex(ValueError, 'SHA-256'):
            profile.verify_file()
        self.plugin.write_bytes(pe_bytes(0x14c))
        self.profile['sha256'] = hashlib.sha256(self.plugin.read_bytes()).hexdigest()
        with self.assertRaisesRegex(ValueError, 'architecture'):
            client.Profile(self.profile).verify_file()

    def test_checked_local_profile_loads_exact_hash_and_raw_cid(self):
        path = SOURCE.parent / 'profiles' / 'autotune-pro-38c42d0b-x64.json'
        profile = client.Profile.load(path)
        self.assertEqual(profile.sha256, '38c42d0b4b260fa72e7ed4af58cb9e271cc48c9fc72372463254f040bbbf76f3')
        self.assertEqual(bytes.fromhex(profile.cid), bytes.fromhex('415453563854413175746f2d54756e65'))
        self.assertEqual(profile.valid_params, [])
        self.assertEqual(profile.map_values({'retune': .5, 'key': 'F#'}), [(4, .5), (2, 6 / 11)])

    def test_profile_selection_requires_exact_hash_and_rejects_ambiguity(self):
        wanted = client.Profile(self.profile)
        wrong_data = copy.deepcopy(self.profile)
        wrong_data['sha256'] = '1' * 64
        wrong = client.Profile(wrong_data)
        self.assertEqual(client.select_profile([wrong, wanted], self.plugin).profile_id, 'fixture-0')
        with self.assertRaisesRegex(ValueError, 'No matching profile'):
            client.select_profile([wrong], self.plugin)
        with self.assertRaisesRegex(ValueError, 'Ambiguous'):
            client.select_profile([wanted, wanted], self.plugin)

    def test_profile_selection_keeps_the_hosts_path_spelling(self):
        # Hosts report the path they loaded (junction, symlink or 8.3 short name);
        # attach and the agent compare against that spelling, so it must not be resolved away.
        real = Path(self.temp.name) / 'real-plugins'
        real.mkdir()
        (real / 'fixture.vst3').write_bytes(pe_bytes())
        link = Path(self.temp.name) / 'linked-plugins'
        make_junction(link, real)
        selected = client.select_profile([client.Profile(self.profile)], link / 'fixture.vst3')
        self.assertEqual(selected.plugin_file, str(link / 'fixture.vst3'))

    def make_client(self, responses=None):
        self.packets = []
        self.identity = {'pid': 4242, 'created_filetime': 12345, 'executable': 'C:\\fixture.exe', 'architecture': 'x64', 'same_user': True}
        def transport(pid, packet):
            self.packets.append(packet)
            op = struct.unpack_from('<H', packet, 6)[0]
            return responses.pop(0) if responses else {'ok': True, 'group': 3, 'op': op}
        return client.Client(4242, self.profile, transport=transport,
                             identity_reader=lambda pid: dict(self.identity),
                             module_reader=lambda pid: [{'name': self.plugin.name, 'path': str(self.plugin)}])

    def test_attach_binds_group_and_apply_sends_no_implicit_clear(self):
        control = self.make_client()
        self.assertEqual(control.attach()['group'], 3)
        control.apply({'retune': .5, 'scale': 'Minor'}, serial=45)
        control.apply({'retune': .5}, serial=45)
        control.clear()
        self.assertEqual([struct.unpack_from('<H', p, 6)[0] for p in self.packets], [5, 3, 3, 4])
        self.assertEqual(struct.unpack('<IHHIIQ', self.packets[1][:24]), (0x32525441, 1, 3, 3, 16, 45))
        self.assertEqual(struct.unpack('<IfIf', self.packets[1][24:]), (4, .5, 162, struct.unpack('<f', struct.pack('<f', 2 / 14))[0]))
        self.assertEqual(len(self.packets[-1]), 24)

    def test_apply_requires_attach_and_new_auto_serial_for_same_value(self):
        control = self.make_client()
        with self.assertRaisesRegex(RuntimeError, 'attach'):
            control.apply({'retune': .5})
        control.attach()
        control.apply({'retune': .5})
        control.apply({'retune': .5})
        one, two = [struct.unpack_from('<Q', p, 16)[0] for p in self.packets[1:]]
        self.assertGreater(one, 0)
        self.assertGreater(two, one)

    def test_pid_reuse_is_rejected_before_any_new_message(self):
        control = self.make_client()
        control.attach()
        self.identity['created_filetime'] += 1
        with self.assertRaisesRegex(RuntimeError, 'identity'):
            control.apply({'retune': .5})
        self.assertEqual(len(self.packets), 1)

    def test_protected_pid_and_wrong_architecture_never_attach(self):
        for pid in (0, -1, 2**32, 12584, 23532, True):
            with self.subTest(pid=pid), self.assertRaises(ValueError):
                client.Client(pid, self.profile)
        control = self.make_client()
        self.identity['architecture'] = 'x86'
        with self.assertRaisesRegex(ValueError, 'architecture'):
            control.attach()
        self.assertEqual(self.packets, [])

    def test_response_decoder_rejects_malformed_oversized_and_remote_error(self):
        for response in (b'[]', b'not-json', b' ' * 65537, b'{"ok":false,"error":"bad group"}'):
            with self.subTest(response=response[:50]), self.assertRaises((ValueError, RuntimeError)):
                client.decode_response(response)

    def test_remote_error_does_not_mark_client_attached(self):
        control = self.make_client([{'ok': False, 'error': 'unsupported class'}])
        with self.assertRaisesRegex(RuntimeError, 'unsupported class'):
            control.attach()
        with self.assertRaisesRegex(RuntimeError, 'attach'):
            control.clear()

    def test_early_attach_waits_for_module_and_later_registers_same_identity(self):
        control = self.make_client()
        loaded = []
        control._module_reader = lambda pid: list(loaded)
        self.assertEqual(control.attach()['pending'], True)
        self.assertEqual(self.packets, [])
        loaded.append({'name': self.plugin.name, 'path': str(self.plugin)})
        self.assertEqual(control.attach()['group'], 3)
        self.assertEqual(len(self.packets), 1)

    def test_module_unloaded_during_prepare_returns_pending_without_group(self):
        control = self.make_client([{'ok': True, 'pending': True, 'reason': 'plugin_not_loaded'}])
        self.assertTrue(control.attach()['pending'])
        with self.assertRaisesRegex(RuntimeError, 'attach'):
            control.apply({'retune': .4})

    def test_x86_without_matching_loader_is_explicitly_unsupported_even_before_load(self):
        control = self.make_client()
        self.plugin.write_bytes(pe_bytes(0x14c))
        data = fixture_profile(self.plugin)
        data['architecture'] = 'x86'
        control.profile = client.Profile(data)
        control._transport = None
        control._module_reader = lambda pid: []
        self.identity['architecture'] = 'x86'
        with self.assertRaisesRegex(ValueError, 'x86.*unsupported'):
            control.attach()

    def test_existing_prototype_hook_is_rejected_before_injection(self):
        with self.assertRaisesRegex(ValueError, 'hook'):
            client.validate_target(4242, [{'name': 'autotune_agent.dll', 'path': 'E:\\prototype\\autotune_agent.dll'}])

    @unittest.skipUnless(os.name == 'nt', 'Windows read-only checks')
    def test_real_current_process_identity_is_stable_and_modules_are_read_only(self):
        first = client.process_identity(os.getpid())
        second = client.process_identity(os.getpid())
        self.assertEqual(first, second)
        self.assertEqual(first['pid'], os.getpid())
        self.assertGreater(first['created_filetime'], 0)
        self.assertTrue(first['same_user'])
        self.assertTrue(any(m['name'].lower().endswith('.exe') for m in client.modules(os.getpid())))

    @unittest.skipUnless(os.name == 'nt', 'Windows local IPC check, no injection')
    def test_real_named_pipe_transfers_encoded_request_and_checks_response(self):
        k = ctypes.WinDLL('kernel32', use_last_error=True)
        signatures = {
            'CreateNamedPipeW': ([wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
                                  wintypes.DWORD, wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p], wintypes.HANDLE),
            'ConnectNamedPipe': ([wintypes.HANDLE, ctypes.c_void_p], wintypes.BOOL),
            'ReadFile': ([wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p], wintypes.BOOL),
            'WriteFile': ([wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p], wintypes.BOOL),
            'FlushFileBuffers': ([wintypes.HANDLE], wintypes.BOOL),
            'CloseHandle': ([wintypes.HANDLE], wintypes.BOOL),
        }
        for name, (args, ret) in signatures.items():
            getattr(k, name).argtypes, getattr(k, name).restype = args, ret
        pipe = k.CreateNamedPipeW(rf'\\.\pipe\autotune-reference-{os.getpid()}', 3, 6, 1, 65536, 8192, 1000, None)
        self.assertNotEqual(pipe, ctypes.c_void_p(-1).value)
        packets = []
        errors = []
        def serve():
            try:
                if not k.ConnectNamedPipe(pipe, None) and ctypes.get_last_error() != 535:
                    raise ctypes.WinError(ctypes.get_last_error())
                data = ctypes.create_string_buffer(8192)
                read = wintypes.DWORD()
                if not k.ReadFile(pipe, data, len(data), ctypes.byref(read), None):
                    raise ctypes.WinError(ctypes.get_last_error())
                packets.append(data.raw[:read.value])
                reply = b'{"ok":true,"fixture":"python-local-pipe"}'
                written = wintypes.DWORD()
                if not k.WriteFile(pipe, reply, len(reply), ctypes.byref(written), None):
                    raise ctypes.WinError(ctypes.get_last_error())
                k.FlushFileBuffers(pipe)
            except Exception as exc:
                errors.append(exc)
            finally:
                k.CloseHandle(pipe)
        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        response = client.request(os.getpid(), client.encode_request('status'), expected_identity=client.process_identity(os.getpid()))
        thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(response, {'ok': True, 'fixture': 'python-local-pipe'})
        self.assertEqual(packets, [bytes.fromhex('415452320100010000000000000000000000000000000000')])

    def test_cli_has_help_and_invalid_role_values_return_json(self):
        result = subprocess.run([sys.executable, str(SOURCE), '--help'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0)
        self.assertIn('scan', result.stdout)
        result = subprocess.run([sys.executable, str(SOURCE), '--pid', '12584', '--profile', 'missing.json', 'set', 'retune', '.5'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertFalse(json.loads(result.stdout)['ok'])

    def test_cli_rejects_bad_values_before_attempting_attachment(self):
        profile_path = Path(self.temp.name) / 'profile.json'
        profile_path.write_text(json.dumps(self.profile), encoding='utf-8')
        result = subprocess.run([sys.executable, str(SOURCE), '--pid', str(os.getpid()), '--profile', str(profile_path),
                                 'apply', '{"retune":20}'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn('normalized', json.loads(result.stdout)['error'])


if __name__ == '__main__':
    unittest.main()
