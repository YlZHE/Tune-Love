"""Apply explicitly supplied values to one already selected local test process.

Example (replace PID and profile with your verified local test target):
  python reference/examples/control_parameters.py --pid 1234 --profile PROFILE.json \
    --values '{"retune":0.5,"flex":0.25,"vibrato":0.5,"humanize":0.2,"key":"F#","scale":"Dorian"}'

All numeric values are normalized, not GUI units. This example neither assumes
a baseline nor restores DSP state. The apply response confirms cache publication;
inspect status for instance consumption before deciding on a later cache clear.
"""
import argparse
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from client import Client


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pid', type=int, required=True)
    parser.add_argument('--profile', required=True)
    parser.add_argument('--values', required=True, help='JSON object containing verified role values')
    parser.add_argument('--serial', type=int, help='Optional duplicate-message token')
    args = parser.parse_args()
    try:
        values = json.loads(args.values)
        controller = Client(args.pid, args.profile)
        # Validate values before target injection/preparation has side effects.
        controller.profile.map_values(values)
        attached = controller.attach()
        if attached.get('pending'):
            print(json.dumps(attached, ensure_ascii=False, indent=2))
            return 2
        result = {'attach': attached, 'apply': controller.apply(values, serial=args.serial)}
        result['status'] = controller.status()
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0
    except (ValueError, RuntimeError, OSError) as exc:
        print(json.dumps({'ok': False, 'error': str(exc)}, ensure_ascii=False))
        return 1


if __name__ == '__main__':
    sys.exit(main())
