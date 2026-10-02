"""Independent explicit-PID VST3 reference client (Python standard library only).

Profiles bind exact binary SHA-256, PE architecture, raw 16-byte VST3 CID and
role mappings. Values are normalized 0..1, never guessed display units. CID hex
is in raw TUID memory byte order; it is NOT a Windows UUID string conversion.
An apply acknowledgement is cache publication, not audible-result validation.
"""
from __future__ import annotations

import argparse
import copy
import ctypes as C
from ctypes import wintypes as W
import hashlib
import json
import math
import ntpath
import os
from pathlib import Path
import re
import struct
import sys
import threading
import time


ROLES = ('retune', 'flex', 'vibrato', 'humanize', 'key', 'scale')
OPS = {'status': 1, 'prepare': 2, 'apply': 3, 'clear': 4, 'prepare_profile': 5}
PROTECTED_PIDS = frozenset((12584, 23532))
HEADER = struct.Struct('<IHHIIQ')
MAX_RESPONSE = 65536
MAX_SERIAL = 2**64 - 1
MAX_PATH_BYTES = 2048


def _uint(value, bits, name, minimum=0):
    if type(value) is not int or not minimum <= value < 2**bits:
        raise ValueError(f'{name} must be an integer in {minimum}..{2**bits - 1}')
    return value


def _normalized(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError('Parameter value must be a finite normalized number in 0..1')
    if not math.isfinite(value) or not 0 <= value <= 1:
        raise ValueError('Parameter value must be a finite normalized number in 0..1')
    return float(value)


def _hex(value, length, name):
    if not isinstance(value, str) or not re.fullmatch(r'[0-9a-fA-F]{' + str(length) + '}', value):
        raise ValueError(f'{name} must contain exactly {length} hexadecimal digits')
    return value.lower()


def _absolute_path(path):
    if not isinstance(path, (str, os.PathLike)):
        raise ValueError('plugin_file must be an absolute file path')
    path = str(path)
    if not path or '\0' in path or not (ntpath.isabs(path) or Path(path).is_absolute()):
        raise ValueError('plugin_file must be an absolute file path without NUL')
    if len(path.encode('utf-16-le')) > MAX_PATH_BYTES:
        raise ValueError('Plugin path exceeds protocol limit')
    return path


def _valid_params(params):
    if not isinstance(params, list) or len(params) > 16:
        raise ValueError('valid_params must contain 0..16 verified parameter ID/title pairs')
    ids = set()
    for item in params:
        if not isinstance(item, dict):
            raise ValueError('valid_params entries must contain id and title')
        param_id = _uint(item.get('id'), 32, 'valid_params ID')
        title = item.get('title')
        if not isinstance(title, str) or not title or '\0' in title or len(title.encode('utf-16-le')) > 254:
            raise ValueError('valid_params title must fit 127 UTF-16 code units without NUL')
        if param_id in ids:
            raise ValueError('valid_params IDs must be distinct')
        ids.add(param_id)
    return copy.deepcopy(params)


def encode_request(op, *, group=0, serial=0, values=None, cid=None, path=None,
                   valid_params=None, free_component=True, vst3_context_not_null=False):
    """Encode one ATR2 request. Preserve caller order and duplicate serials."""
    if op not in OPS:
        raise ValueError(f'Unknown operation: {op}')
    if type(vst3_context_not_null) is not bool:
        raise ValueError('vst3_context_not_null must be boolean')
    if op != 'prepare_profile' and vst3_context_not_null:
        raise ValueError('vst3_context_not_null requires prepare_profile')
    _uint(group, 32, 'group')
    _uint(serial, 64, 'serial')
    payload = b''
    if op in ('prepare', 'prepare_profile'):
        if group or serial or values is not None:
            raise ValueError('prepare requires group=0, serial=0 and no parameter values')
        class_id = bytes.fromhex(_hex(cid, 32, 'cid'))
        path_bytes = _absolute_path(path).encode('utf-16-le')
        if op == 'prepare_profile':
            params = _valid_params(valid_params)
            if type(free_component) is not bool:
                raise ValueError('free_component must be boolean')
            flags = int(free_component) | (int(vst3_context_not_null) << 1)
            payload = class_id + struct.pack('<III', flags, len(path_bytes), len(params)) + path_bytes
            for item in params:
                payload += struct.pack('<I', item['id']) + item['title'].encode('utf-16-le').ljust(256, b'\0')
        else:
            if valid_params is not None or not free_component:
                raise ValueError('Instance signatures require prepare_profile')
            payload = class_id + path_bytes
    elif op == 'apply':
        if cid is not None or path is not None or valid_params is not None or not free_component:
            raise ValueError('Only prepare accepts a CID or path')
        _uint(serial, 64, 'serial', minimum=1)
        if not isinstance(values, (list, tuple)) or not 1 <= len(values) <= 256:
            raise ValueError('apply requires 1..256 parameter values')
        encoded = []
        for item in values:
            if not isinstance(item, (list, tuple)) or len(item) != 2:
                raise ValueError('Each parameter is an (ID, normalized value) pair')
            encoded.append(struct.pack('<If', _uint(item[0], 32, 'parameter ID'), _normalized(item[1])))
        payload = b''.join(encoded)
    elif serial or values is not None or cid is not None or path is not None or valid_params is not None or not free_component:
        raise ValueError('status and clear accept no payload or serial')
    return HEADER.pack(0x32525441, 1, OPS[op], group, len(payload), serial) + payload


def decode_response(response):
    """Validate JSON returned by the local endpoint and surface explicit errors."""
    if isinstance(response, bytes):
        if len(response) > MAX_RESPONSE:
            raise ValueError('Agent response exceeds 65536 bytes')
        try:
            response = json.loads(response.decode('utf-8'))
        except (UnicodeError, json.JSONDecodeError) as exc:
            raise ValueError('Agent returned malformed JSON') from exc
    if not isinstance(response, dict) or type(response.get('ok')) is not bool:
        raise ValueError('Agent response must be a JSON object with a boolean ok field')
    if not response['ok']:
        raise RuntimeError(str(response.get('error', 'Agent rejected the request')))
    return response


def file_sha256(path):
    with open(path, 'rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def pe_architecture(path):
    with open(path, 'rb') as source:
        header = source.read(64)
        if len(header) != 64 or header[:2] != b'MZ':
            raise ValueError('Plugin/agent file is not a PE binary')
        offset = struct.unpack_from('<I', header, 60)[0]
        if offset > 16 * 1024 * 1024:
            raise ValueError('Invalid PE header offset')
        source.seek(offset)
        pe = source.read(6)
    if len(pe) != 6 or pe[:4] != b'PE\0\0':
        raise ValueError('Invalid PE signature')
    machine = struct.unpack_from('<H', pe, 4)[0]
    if machine not in (0x8664, 0x14c):
        raise ValueError(f'Unsupported PE architecture 0x{machine:04x}')
    return 'x64' if machine == 0x8664 else 'x86'


class Profile:
    """Validated schema-v1 mapping. Missing roles remain unsupported capabilities."""
    def __init__(self, data):
        if not isinstance(data, dict) or type(data.get('schema_version')) is not int or data['schema_version'] != 1:
            raise ValueError('Unsupported profile schema_version; expected 1')
        self.data = copy.deepcopy(data)
        self.profile_id = data.get('profile_id')
        if not isinstance(self.profile_id, str) or not self.profile_id.strip():
            raise ValueError('profile_id must be nonempty')
        self.architecture = data.get('architecture')
        if self.architecture not in ('x64', 'x86'):
            raise ValueError('Profile architecture must be x64 or x86')
        self.plugin_file = _absolute_path(data.get('plugin_file'))
        self.sha256 = _hex(data.get('sha256'), 64, 'sha256')
        self.cid = _hex(data.get('cid'), 32, 'cid')
        if 'valid_params' not in self.data:
            raise ValueError('Profile valid_params is required; use [] when this component exposes no metadata')
        signature = self.data['valid_params']
        self.valid_params = _valid_params(signature)
        self.free_component = self.data.get('free_component', True)
        if type(self.free_component) is not bool:
            raise ValueError('free_component must be boolean')
        self.vst3_context_not_null = self.data.get('vst3_context_not_null', False)
        if type(self.vst3_context_not_null) is not bool:
            raise ValueError('vst3_context_not_null must be boolean')
        self.roles = self.data.get('roles')
        if not isinstance(self.roles, dict) or not self.roles:
            raise ValueError('roles must define at least one verified mapping')
        ids = set()
        for role, mapping in self.roles.items():
            if role not in ROLES or not isinstance(mapping, dict):
                raise ValueError(f'Unknown parameter role or malformed mapping: {role}')
            param_id = _uint(mapping.get('id'), 32, f'{role}.id')
            if param_id in ids:
                raise ValueError('Role mappings must have distinct parameter IDs')
            ids.add(param_id)
            if mapping.get('domain', 'normalized') != 'normalized':
                raise ValueError(f'Unsupported domain for {role}; only verified normalized values are accepted')
            options = mapping.get('options')
            if options is not None:
                if not isinstance(options, dict) or not options:
                    raise ValueError(f'{role}.options must map nonempty labels to normalized values')
                for label, value in options.items():
                    if not isinstance(label, str) or not label:
                        raise ValueError(f'{role}.options labels must be nonempty strings')
                    _normalized(value)

    @classmethod
    def load(cls, source):
        if isinstance(source, cls):
            return source
        if isinstance(source, dict):
            return cls(source)
        with open(source, encoding='utf-8-sig') as handle:
            return cls(json.load(handle))

    def verify_file(self):
        actual = file_sha256(self.plugin_file)
        if actual != self.sha256:
            raise ValueError(f'Plugin SHA-256 mismatch for {self.profile_id}; select a verified profile for this exact binary')
        architecture = pe_architecture(self.plugin_file)
        if architecture != self.architecture:
            raise ValueError(f'Plugin architecture {architecture} differs from profile architecture {self.architecture}')
        return {'path': self.plugin_file, 'sha256': actual, 'architecture': architecture}

    def map_values(self, values):
        if not isinstance(values, dict) or not 1 <= len(values) <= 256:
            raise ValueError('values must be a nonempty role-to-value object')
        mapped = []
        for role, value in values.items():
            if role not in self.roles:
                raise ValueError(f'Profile {self.profile_id}: unsupported parameter role {role}')
            mapping = self.roles[role]
            options = mapping.get('options')
            if isinstance(value, str):
                if options is None or value not in options:
                    raise ValueError(f'Unknown option {value!r} for {role}')
                value = options[value]
            value = _normalized(value)
            if options is not None:
                # Match a canonical discrete value after its documented float32 wire conversion.
                wire = struct.pack('<f', value)
                canonical = next((v for v in options.values() if struct.pack('<f', v) == wire), None)
                if canonical is None:
                    raise ValueError(f'{role} requires one of the verified discrete options')
                value = float(canonical)
            mapped.append((mapping['id'], value))
        return mapped

    def options(self, role):
        """Return only options declared by this profile (never a live plugin query)."""
        if role not in self.roles:
            raise ValueError(f'Profile {self.profile_id}: unsupported parameter role {role}')
        mapping = self.roles[role]
        declared = mapping.get('options')
        if not isinstance(declared, dict) or not declared:
            raise ValueError(f'Profile {self.profile_id}: role {role} has no declared discrete options')
        return {
            'ok': True, 'source': 'profile', 'profile_id': self.profile_id,
            'role': role, 'id': mapping['id'],
            'options': [{'label': label, 'normalized': float(value)} for label, value in declared.items()],
        }


def select_profile(profiles, plugin_file, architecture=None):
    """Select only by exact binary hash and architecture, rejecting ambiguity."""
    digest = file_sha256(plugin_file)
    actual_arch = pe_architecture(plugin_file)
    if architecture is not None and architecture != actual_arch:
        raise ValueError('Requested architecture does not match plugin architecture')
    matches = [p for item in profiles if (p := Profile.load(item)).sha256 == digest and p.architecture == actual_arch]
    if not matches:
        raise ValueError(f'No matching profile for SHA-256 {digest}; describe this build and add verified mappings')
    if len(matches) != 1:
        raise ValueError('Ambiguous profile selection; choose an explicit component/profile')
    # A profile can be distributed for a default installation path and selected for another install.
    data = copy.deepcopy(matches[0].data)
    # Keep the host's spelling: attach and the in-process agent compare against the module
    # path the host reports, which may go through a junction, symlink or 8.3 short name.
    data['plugin_file'] = ntpath.abspath(str(plugin_file))
    return Profile(data)


def validate_target(pid, loaded=()):
    _uint(pid, 32, 'PID', minimum=1)
    if pid in PROTECTED_PIDS:
        raise ValueError('Refusing protected target PID')
    for module in loaded:
        name = module.get('name', '') if isinstance(module, dict) else str(module)
        if ntpath.basename(name).lower() in ('em64.dll', 'em32.dll', 'autotune_agent.dll'):
            raise ValueError('An existing helper hook is loaded; choose a clean explicitly selected target')


class FILETIME(C.Structure):
    _fields_ = [('dwLowDateTime', W.DWORD), ('dwHighDateTime', W.DWORD)]


class MODULEENTRY32W(C.Structure):
    _fields_ = [('dwSize', W.DWORD), ('th32ModuleID', W.DWORD), ('th32ProcessID', W.DWORD),
                ('GlblcntUsage', W.DWORD), ('ProccntUsage', W.DWORD), ('modBaseAddr', C.c_void_p),
                ('modBaseSize', W.DWORD), ('hModule', W.HMODULE), ('szModule', W.WCHAR * 256),
                ('szExePath', W.WCHAR * 260)]


class OVERLAPPED(C.Structure):
    _fields_ = [('Internal', C.c_size_t), ('InternalHigh', C.c_size_t),
                ('Offset', W.DWORD), ('OffsetHigh', W.DWORD), ('hEvent', W.HANDLE)]


def _check(ok):
    if not ok:
        raise C.WinError(C.get_last_error())
    return ok


def _machine_arch(process_machine, native_machine):
    if process_machine == 0 and native_machine == 0x8664:
        return 'x64'
    if process_machine == 0x14C or native_machine == 0x14C:
        return 'x86'
    raise ValueError(f'Unsupported Windows process architecture 0x{native_machine:04x}/0x{process_machine:04x}')


_KERNEL32 = None
_ADVAPI32 = None


def _kernel32():
    global _KERNEL32, _ADVAPI32
    if _KERNEL32 is not None:
        return _KERNEL32
    if os.name != 'nt':
        raise RuntimeError('Windows is required')
    k = C.WinDLL('kernel32', use_last_error=True)
    signatures = {
        'CreateToolhelp32Snapshot': ([W.DWORD, W.DWORD], W.HANDLE),
        'Module32FirstW': ([W.HANDLE, C.POINTER(MODULEENTRY32W)], W.BOOL),
        'Module32NextW': ([W.HANDLE, C.POINTER(MODULEENTRY32W)], W.BOOL),
        'CloseHandle': ([W.HANDLE], W.BOOL),
        'OpenProcess': ([W.DWORD, W.BOOL, W.DWORD], W.HANDLE),
        'GetCurrentProcess': ([], W.HANDLE),
        'GetProcessTimes': ([W.HANDLE, C.POINTER(FILETIME), C.POINTER(FILETIME), C.POINTER(FILETIME), C.POINTER(FILETIME)], W.BOOL),
        'QueryFullProcessImageNameW': ([W.HANDLE, W.DWORD, W.LPWSTR, C.POINTER(W.DWORD)], W.BOOL),
        'IsWow64Process2': ([W.HANDLE, C.POINTER(W.WORD), C.POINTER(W.WORD)], W.BOOL),
        'GetModuleHandleW': ([W.LPCWSTR], W.HMODULE),
        'GetProcAddress': ([W.HMODULE, C.c_char_p], C.c_void_p),
        'GetModuleHandleExW': ([W.DWORD, C.c_void_p, C.POINTER(W.HMODULE)], W.BOOL),
        'GetModuleFileNameW': ([W.HMODULE, W.LPWSTR, W.DWORD], W.DWORD),
        'VirtualAllocEx': ([W.HANDLE, C.c_void_p, C.c_size_t, W.DWORD, W.DWORD], C.c_void_p),
        'VirtualFreeEx': ([W.HANDLE, C.c_void_p, C.c_size_t, W.DWORD], W.BOOL),
        'WriteProcessMemory': ([W.HANDLE, C.c_void_p, C.c_void_p, C.c_size_t, C.POINTER(C.c_size_t)], W.BOOL),
        'CreateRemoteThread': ([W.HANDLE, C.c_void_p, C.c_size_t, C.c_void_p, C.c_void_p, W.DWORD, C.c_void_p], W.HANDLE),
        'WaitForSingleObject': ([W.HANDLE, W.DWORD], W.DWORD),
        'WaitNamedPipeW': ([W.LPCWSTR, W.DWORD], W.BOOL),
        'CreateFileW': ([W.LPCWSTR, W.DWORD, W.DWORD, C.c_void_p, W.DWORD, W.DWORD, W.HANDLE], W.HANDLE),
        'GetNamedPipeServerProcessId': ([W.HANDLE, C.POINTER(W.DWORD)], W.BOOL),
        'SetNamedPipeHandleState': ([W.HANDLE, C.POINTER(W.DWORD), C.c_void_p, C.c_void_p], W.BOOL),
        'CreateEventW': ([C.c_void_p, W.BOOL, W.BOOL, W.LPCWSTR], W.HANDLE),
        'TransactNamedPipe': ([W.HANDLE, C.c_void_p, W.DWORD, C.c_void_p, W.DWORD, C.POINTER(W.DWORD), C.POINTER(OVERLAPPED)], W.BOOL),
        'GetOverlappedResult': ([W.HANDLE, C.POINTER(OVERLAPPED), C.POINTER(W.DWORD), W.BOOL], W.BOOL),
        'CancelIoEx': ([W.HANDLE, C.POINTER(OVERLAPPED)], W.BOOL),
    }
    for name, (args, result) in signatures.items():
        fn = getattr(k, name)
        fn.argtypes, fn.restype = args, result
    a = C.WinDLL('advapi32', use_last_error=True)
    for name, args, result in (
        ('OpenProcessToken', [W.HANDLE, W.DWORD, C.POINTER(W.HANDLE)], W.BOOL),
        ('GetTokenInformation', [W.HANDLE, C.c_int, C.c_void_p, W.DWORD, C.POINTER(W.DWORD)], W.BOOL),
        ('GetLengthSid', [C.c_void_p], W.DWORD),
        ('EqualSid', [C.c_void_p, C.c_void_p], W.BOOL),
    ):
        fn = getattr(a, name)
        fn.argtypes, fn.restype = args, result
    _KERNEL32, _ADVAPI32 = k, a
    return k


def _same_user(k, process):
    """Compare process token SID with this process token SID."""
    target, owner = W.HANDLE(), W.HANDLE()
    try:
        _check(_ADVAPI32.OpenProcessToken(process, 8, C.byref(target)))  # TOKEN_QUERY
        _check(_ADVAPI32.OpenProcessToken(k.GetCurrentProcess(), 8, C.byref(owner)))

        def token_sid(token):
            size = W.DWORD()
            _ADVAPI32.GetTokenInformation(token, 1, None, 0, C.byref(size))
            if not size.value:
                raise C.WinError(C.get_last_error())
            buffer = C.create_string_buffer(size.value)
            _check(_ADVAPI32.GetTokenInformation(token, 1, buffer, size, C.byref(size)))
            pointer = C.c_void_p.from_buffer(buffer).value
            if not pointer:
                raise RuntimeError('Token has no user SID')
            length = _ADVAPI32.GetLengthSid(pointer)
            if not length:
                raise C.WinError(C.get_last_error())
            return buffer, pointer

        left, left_sid = token_sid(target)
        right, right_sid = token_sid(owner)
        return bool(_ADVAPI32.EqualSid(left_sid, right_sid))
    finally:
        if target:
            k.CloseHandle(target)
        if owner:
            k.CloseHandle(owner)


def _same_identity(expected, actual):
    if any(expected.get(key) != actual.get(key) for key in ('pid', 'created_filetime', 'executable', 'architecture')):
        raise RuntimeError('Target process identity changed or PID was reused; create a new Client for the intended process')


class Client:
    """One explicitly selected process and one exact component profile.

    transport(pid, packet) is an optional external transport boundary. Supplying
    it disables automatic DLL injection (useful for embedded agents/fixtures).
    identity_reader and module_reader permit deterministic boundary tests; the
    real defaults always inspect creation time and loaded modules.
    """
    def __init__(self, pid, profile, *, transport=None, identity_reader=None, module_reader=None, agent_path=None):
        validate_target(pid)
        self.pid = pid
        self.profile = Profile.load(profile)
        self.group = None
        self.identity = None
        self._transport = transport
        self._identity_reader = identity_reader or process_identity
        self._module_reader = module_reader or modules
        self.agent_path = Path(agent_path) if agent_path else Path(__file__).parent / 'build' / self.profile.architecture / 'reference_agent.dll'
        self._serial = min(time.time_ns(), MAX_SERIAL - 1)
        self._lock = threading.RLock()

    def _check_identity(self):
        current = self._identity_reader(self.pid)
        if self.identity is not None:
            _same_identity(self.identity, current)
        return current

    def _call(self, packet):
        self._check_identity()
        if self._transport is None:
            response = request(self.pid, packet, expected_identity=self.identity)
        else:
            response = self._transport(self.pid, packet)
        self._check_identity()
        return decode_response(response)

    def attach(self):
        with self._lock:
            current = self._check_identity()
            if current['architecture'] != self.profile.architecture:
                raise ValueError('Target architecture differs from profile architecture')
            if current.get('same_user') is not True:
                raise ValueError('Only same-user local targets are supported')
            self.profile.verify_file()
            if self._transport is None and (self.profile.architecture != 'x64' or C.sizeof(C.c_void_p) != 8):
                raise ValueError('x86 attachment is unsupported: no matching loader is provided; use x64 Python and an x64 target')
            loaded = self._module_reader(self.pid)
            validate_target(self.pid, loaded)
            target_path = ntpath.normcase(ntpath.abspath(self.profile.plugin_file))
            self.identity = dict(current)
            if not any(ntpath.normcase(ntpath.abspath(m['path'])) == target_path for m in loaded):
                self.group = None
                return {'ok': True, 'pending': True, 'reason': 'plugin_not_loaded',
                        'pid': self.pid, 'profile_id': self.profile.profile_id,
                        'identity': dict(self.identity)}
            if self._transport is None:
                agent = self.agent_path.resolve(strict=True)
                exact = any(ntpath.normcase(m['path']) == ntpath.normcase(str(agent)) for m in loaded)
                if not exact:
                    if any(m['name'].lower() == agent.name.lower() for m in loaded):
                        raise ValueError('A different reference_agent.dll is already loaded')
                    inject(self.pid, agent, expected_identity=self.identity)
            result = self._call(encode_request('prepare_profile', cid=self.profile.cid, path=self.profile.plugin_file,
                                              valid_params=self.profile.valid_params, free_component=self.profile.free_component,
                                              vst3_context_not_null=self.profile.vst3_context_not_null))
            if result.get('pending'):
                self.group = None
                return result
            self.group = _uint(result.get('group'), 32, 'prepare response group')
            return result

    def status(self):
        with self._lock:
            if self.identity is None:
                self.identity = dict(self._identity_reader(self.pid))
            return self._call(encode_request('status', group=self.group or 0))

    def apply(self, values, serial=None):
        with self._lock:
            if self.group is None:
                raise RuntimeError('Call attach() before apply()')
            mapped = self.profile.map_values(values)
            if serial is None:
                if self._serial == MAX_SERIAL:
                    raise ValueError('Automatic serial exhausted; supply an explicit nonzero serial')
                self._serial += 1
                serial = self._serial
            return self._call(encode_request('apply', group=self.group, serial=serial, values=mapped))

    def clear(self):
        """Explicitly clear the agent cache. This does not restore plugin DSP state."""
        with self._lock:
            if self.group is None:
                raise RuntimeError('Call attach() before clear()')
            return self._call(encode_request('clear', group=self.group))


def process_identity(pid):
    """Read process creation identity, image path, architecture and user ownership."""
    validate_target(pid)
    if os.name != 'nt':
        raise RuntimeError('Windows is required for process identity checks')
    k = _kernel32()
    handle = k.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not handle:
        raise C.WinError(C.get_last_error())
    try:
        created, exited, kernel, user = FILETIME(), FILETIME(), FILETIME(), FILETIME()
        _check(k.GetProcessTimes(handle, C.byref(created), C.byref(exited), C.byref(kernel), C.byref(user)))
        image = C.create_unicode_buffer(32768)
        size = W.DWORD(len(image))
        _check(k.QueryFullProcessImageNameW(handle, 0, image, C.byref(size)))
        process_machine, native_machine = W.WORD(), W.WORD()
        _check(k.IsWow64Process2(handle, C.byref(process_machine), C.byref(native_machine)))
        architecture = _machine_arch(process_machine.value, native_machine.value)
        same_user = _same_user(k, handle)
        filetime = (created.dwHighDateTime << 32) | created.dwLowDateTime
        return {'pid': pid, 'created_filetime': filetime, 'executable': image.value,
                'architecture': architecture, 'same_user': same_user}
    finally:
        k.CloseHandle(handle)


def modules(pid):
    # Loader activity can invalidate a Toolhelp snapshot (ERROR_BAD_LENGTH).
    for attempt in range(5):
        try:
            return _modules_once(pid)
        except OSError as exc:
            if getattr(exc, 'winerror', None) != 24 or attempt == 4:
                raise
            time.sleep(.02)


def _modules_once(pid):
    validate_target(pid)
    if os.name != 'nt':
        raise RuntimeError('Windows is required for module enumeration')
    k = _kernel32()
    snap = k.CreateToolhelp32Snapshot(0x18, pid)  # TH32CS_SNAPMODULE|TH32CS_SNAPMODULE32
    if snap == C.c_void_p(-1).value:
        raise C.WinError(C.get_last_error())
    try:
        entry = MODULEENTRY32W()
        entry.dwSize = C.sizeof(entry)
        _check(k.Module32FirstW(snap, C.byref(entry)))
        found = []
        while True:
            found.append({'name': entry.szModule, 'path': entry.szExePath,
                          'base': int(entry.modBaseAddr or 0), 'size': entry.modBaseSize})
            if k.Module32NextW(snap, C.byref(entry)):
                continue
            if C.get_last_error() != 18:  # ERROR_NO_MORE_FILES
                raise C.WinError(C.get_last_error())
            return found
    finally:
        k.CloseHandle(snap)


def inject(pid, dll, *, expected_identity=None):
    validate_target(pid)
    if os.name != 'nt' or C.sizeof(C.c_void_p) != 8:
        raise RuntimeError('Only native x64 Python can load the x64 reference agent')
    path = Path(dll).resolve(strict=True)
    if pe_architecture(path) != 'x64':
        raise ValueError('Reference agent architecture must be x64')
    before = process_identity(pid)
    if expected_identity is not None:
        _same_identity(expected_identity, before)
    if before['architecture'] != 'x64' or not before['same_user']:
        raise ValueError('Target must be a same-user native x64 process')
    loaded = modules(pid)
    validate_target(pid, loaded)
    if any(ntpath.normcase(m['path']) == ntpath.normcase(str(path)) for m in loaded):
        return {'ok': True, 'pid': pid, 'agent': str(path), 'already_loaded': True}
    k = _kernel32()
    process = k.OpenProcess(0x043A, False, pid)
    if not process:
        raise C.WinError(C.get_last_error())
    remote = thread = None
    finished = False
    try:
        _check_birthtime(k, process, before)
        owner = W.HMODULE()
        local_loader = k.GetProcAddress(k.GetModuleHandleW('kernel32.dll'), b'LoadLibraryW')
        if not local_loader or not k.GetModuleHandleExW(6, local_loader, C.byref(owner)):
            raise C.WinError(C.get_last_error())
        owner_path = C.create_unicode_buffer(32768)
        if not k.GetModuleFileNameW(owner, owner_path, len(owner_path)):
            raise C.WinError(C.get_last_error())
        remote_owner = next((m for m in loaded if m['name'].lower() == ntpath.basename(owner_path.value).lower()), None)
        if remote_owner is None:
            raise RuntimeError('Cannot resolve target LoadLibraryW module')
        offset = int(local_loader) - int(owner.value)
        if not 0 <= offset < remote_owner['size']:
            raise RuntimeError('LoadLibraryW offset is outside the target kernel image')
        payload = C.create_unicode_buffer(str(path))
        remote = k.VirtualAllocEx(process, None, C.sizeof(payload), 0x3000, 4)
        if not remote:
            raise C.WinError(C.get_last_error())
        written = C.c_size_t()
        _check(k.WriteProcessMemory(process, remote, payload, C.sizeof(payload), C.byref(written)))
        if written.value != C.sizeof(payload):
            raise RuntimeError('Incomplete agent path write')
        thread = k.CreateRemoteThread(process, None, 0, remote_owner['base'] + offset, remote, 0, None)
        if not thread:
            raise C.WinError(C.get_last_error())
        if k.WaitForSingleObject(thread, 10000) != 0:
            raise TimeoutError('LoadLibraryW did not finish; do not retry blindly')
        finished = True
        after = process_identity(pid)
        _same_identity(before, after)
        if not any(ntpath.normcase(m['path']) == ntpath.normcase(str(path)) for m in modules(pid)):
            raise RuntimeError('Reference agent did not appear in target module list')
        return {'ok': True, 'pid': pid, 'agent': str(path), 'already_loaded': False}
    finally:
        if remote and (not thread or finished):
            k.VirtualFreeEx(process, remote, 0, 0x8000)
        if thread:
            k.CloseHandle(thread)
        k.CloseHandle(process)


def request(pid, packet, *, expected_identity=None):
    validate_target(pid)
    if os.name != 'nt':
        raise RuntimeError('Windows is required for named-pipe transport')
    current = process_identity(pid)
    if expected_identity is not None:
        _same_identity(expected_identity, current)
    if not current['same_user']:
        raise ValueError('Target is not owned by the current user')
    k = _kernel32()
    guard = k.OpenProcess(0x101000, False, pid)  # QUERY_LIMITED_INFORMATION|SYNCHRONIZE
    if not guard:
        raise C.WinError(C.get_last_error())
    pipe = None
    try:
        _check_birthtime(k, guard, current)
        pipe_name = rf'\\.\pipe\autotune-reference-{pid}'
        deadline = time.monotonic() + 5
        while True:
            error = 0
            if k.WaitNamedPipeW(pipe_name, 250):
                pipe = k.CreateFileW(pipe_name, 0xC0000000, 0, None, 3, 0x40000000, None)
                if pipe != C.c_void_p(-1).value:
                    break
                pipe = None
                error = C.get_last_error()
            else:
                error = C.get_last_error()
            # DLL worker creation and a just-disconnected previous client can
            # temporarily leave no listening pipe instance.
            if error not in (2, 121, 231) or time.monotonic() >= deadline:
                raise C.WinError(error)
            _check_birthtime(k, guard, current)
            time.sleep(.025)
        server_pid = W.DWORD()
        _check(k.GetNamedPipeServerProcessId(pipe, C.byref(server_pid)))
        if server_pid.value != pid:
            raise RuntimeError('Named-pipe server PID does not match explicit target PID')
        mode = W.DWORD(2)  # PIPE_READMODE_MESSAGE
        _check(k.SetNamedPipeHandleState(pipe, C.byref(mode), None, None))
        event = k.CreateEventW(None, True, False, None)
        if not event:
            raise C.WinError(C.get_last_error())
        try:
            incoming = C.create_string_buffer(MAX_RESPONSE)
            outgoing = C.create_string_buffer(packet)
            received = W.DWORD()
            overlap = OVERLAPPED(hEvent=event)
            ok = k.TransactNamedPipe(pipe, outgoing, len(packet), incoming,
                                      len(incoming), C.byref(received), C.byref(overlap))
            if not ok:
                error = C.get_last_error()
                if error == 234:
                    raise ValueError('Agent response exceeds 65536 bytes')
                if error != 997:  # ERROR_IO_PENDING
                    raise C.WinError(error)
                if k.WaitForSingleObject(event, 15000) != 0:
                    k.CancelIoEx(pipe, C.byref(overlap))
                    # Keep buffers alive until the cancelled kernel I/O has completed.
                    k.GetOverlappedResult(pipe, C.byref(overlap), C.byref(received), True)
                    raise TimeoutError('Reference agent timed out; query status before retrying a change')
                _check(k.GetOverlappedResult(pipe, C.byref(overlap), C.byref(received), False))
            if received.value > MAX_RESPONSE:
                raise ValueError('Agent response exceeds 65536 bytes')
            return decode_response(incoming.raw[:received.value])
        finally:
            k.CloseHandle(event)
    finally:
        if pipe:
            k.CloseHandle(pipe)
        k.CloseHandle(guard)


def _check_birthtime(k, handle, expected):
    created, exited, kernel, user = FILETIME(), FILETIME(), FILETIME(), FILETIME()
    _check(k.GetProcessTimes(handle, C.byref(created), C.byref(exited), C.byref(kernel), C.byref(user)))
    birth = (created.dwHighDateTime << 32) | created.dwLowDateTime
    if birth != expected['created_filetime'] or exited.dwLowDateTime or exited.dwHighDateTime:
        raise RuntimeError('Target process identity changed or exited')


class PROCESSENTRY32W(C.Structure):
    _fields_ = [('dwSize', W.DWORD), ('cntUsage', W.DWORD), ('th32ProcessID', W.DWORD),
                ('th32DefaultHeapID', C.c_size_t), ('th32ModuleID', W.DWORD),
                ('cntThreads', W.DWORD), ('th32ParentProcessID', W.DWORD),
                ('pcPriClassBase', W.LONG), ('dwFlags', W.DWORD), ('szExeFile', W.WCHAR * 260)]


def scan(pid=None):
    """Read Toolhelp metadata for loaded VST3 modules; never inject or read memory.

    Protected PIDs are reported as skipped. Permission/architecture failures
    remain visible in errors instead of being presented as complete discovery.
    """
    k = _kernel32()
    candidates = []
    if pid is not None:
        validate_target(pid)
        candidates = [pid]
    else:
        for name in ('Process32FirstW', 'Process32NextW'):
            fn = getattr(k, name)
            fn.argtypes, fn.restype = [W.HANDLE, C.POINTER(PROCESSENTRY32W)], W.BOOL
        snap = k.CreateToolhelp32Snapshot(2, 0)
        if snap == C.c_void_p(-1).value:
            raise C.WinError(C.get_last_error())
        try:
            entry = PROCESSENTRY32W()
            entry.dwSize = C.sizeof(entry)
            _check(k.Process32FirstW(snap, C.byref(entry)))
            while True:
                candidates.append(entry.th32ProcessID)
                if not k.Process32NextW(snap, C.byref(entry)):
                    if C.get_last_error() != 18:
                        raise C.WinError(C.get_last_error())
                    break
        finally:
            k.CloseHandle(snap)
    targets, errors = [], []
    for target in candidates:
        if target == 0 or target in PROTECTED_PIDS:
            errors.append({'pid': target, 'reason': 'protected_or_system_pid'})
            continue
        try:
            identity = process_identity(target)
            if not identity['same_user']:
                errors.append({'pid': target, 'reason': 'different_user'})
                continue
            vst3 = [m for m in modules(target) if m['path'].lower().endswith('.vst3')]
            if vst3:
                _same_identity(identity, process_identity(target))
                targets.append({'identity': identity, 'plugins': vst3})
        except (OSError, ValueError, RuntimeError) as exc:
            errors.append({'pid': target, 'reason': str(exc)})
    return {'ok': True, 'targets': targets, 'errors': errors, 'read_only': True}


def _cli_value(value):
    try:
        return float(value)
    except ValueError:
        return value


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pid', type=int, help='Explicit target PID (never auto-injected by scan)')
    parser.add_argument('--profile', help='Exact-build schema-v1 JSON profile')
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('scan', help='Read-only loaded VST3 module discovery')
    attach_parser = sub.add_parser('attach', help='Attach, or report pending until the selected plugin loads')
    attach_parser.add_argument('--wait', type=float, default=0, help='Poll only this PID for up to N seconds')
    sub.add_parser('status', help='Read existing agent status without injecting')
    setter = sub.add_parser('set')
    setter.add_argument('role', choices=ROLES)
    setter.add_argument('value', help='Normalized number or exact profile option label')
    setter.add_argument('--serial', type=int)
    batch = sub.add_parser('apply')
    batch.add_argument('values', help='JSON role-to-normalized-value/option-label object')
    batch.add_argument('--serial', type=int)
    sub.add_parser('clear', help='Explicitly clear helper cache; does not restore DSP state')
    options_parser = sub.add_parser('options', help='Read profile-declared discrete options without connecting')
    options_parser.add_argument('role', choices=('key', 'scale'))
    args = parser.parse_args(argv)
    try:
        if args.command == 'scan':
            result = scan(args.pid)
        else:
            if args.command == 'options':
                if args.profile is None:
                    raise ValueError('--profile is required for options')
                result = Profile.load(args.profile).options(args.role)
                print(json.dumps(result, ensure_ascii=False, indent=2))
                return 0
            if args.pid is None or args.profile is None:
                raise ValueError('--pid and --profile are required for this command')
            controller = Client(args.pid, args.profile)
            if args.command == 'status':
                result = controller.status()
            else:
                values = None
                if args.command == 'set':
                    values = {args.role: _cli_value(args.value)}
                    controller.profile.map_values(values)
                elif args.command == 'apply':
                    values = json.loads(args.values)
                    controller.profile.map_values(values)
                result = controller.attach()
                if args.command == 'attach' and args.wait:
                    if not math.isfinite(args.wait) or args.wait < 0:
                        raise ValueError('--wait must be finite and nonnegative')
                    deadline = time.monotonic() + args.wait
                    while result.get('pending') and time.monotonic() < deadline:
                        time.sleep(min(.25, max(0, deadline - time.monotonic())))
                        result = controller.attach()
                if args.command != 'attach':
                    if result.get('pending'):
                        raise RuntimeError('Plugin is not loaded; attach is pending')
                    if args.command == 'clear':
                        result = controller.clear()
                    else:
                        result = controller.apply(values, serial=args.serial)
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0
    except (OSError, ValueError, RuntimeError) as exc:
        print(json.dumps({'ok': False, 'error': str(exc)}, ensure_ascii=False))
        return 1


if __name__ == '__main__':
    sys.exit(main())
