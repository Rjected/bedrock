#!/usr/bin/env python3
"""Align deterministic exits with bounded lookahead to identify missing records."""
import collections
import hashlib
import importlib.util
import json
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location('compare', Path(__file__).with_name('compare-traces.py'))
compare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare)
FIELDS = compare.FIELDS + ['exit_reason', 'exit_qualification']


def records(path, summary):
    reasons = collections.Counter()
    other_hashes = collections.defaultdict(hashlib.sha256)
    previous = None
    with path.open('rb') as stream:
        for line in stream:
            record = compare.decode(line)
            if record['kind'] == 'exit' and record['deterministic']:
                reasons[record['data']['exit_reason']] += 1
                record['_previous_event'] = previous
                yield record, tuple(record['data'][k] for k in FIELDS)
            elif record['kind'] != 'exit':
                other_hashes[record['kind']].update(compare.encode({'tsc': record['tsc'], 'data': record['data']}) + b'\n')
            previous = {k: record[k] for k in ['seq', 'tsc', 'kind']}
    summary['deterministic_exit_counts_by_reason'] = dict(reasons)
    summary['non_exit_semantic_hashes'] = {k: v.hexdigest() for k, v in other_hashes.items()}


def main():
    root = Path(sys.argv[1])
    result = {'runs': [{}, {}], 'missing_from_run1': [], 'missing_from_run2': [], 'matched_records': 0, 'unresolved_divergence': None}
    streams = [iter(records(root/f'run{i}/events.jsonl', result['runs'][i-1])) for i in [1, 2]]
    queues = [collections.deque(), collections.deque()]
    def fill(i, count):
        while len(queues[i]) < count:
            item = next(streams[i], None)
            if item is None:
                break
            queues[i].append(item)
    while True:
        for i in [0, 1]:
            fill(i, 1)
        if not all(queues):
            if any(queues):
                result['unresolved_divergence'] = 'different ending'
            break
        if queues[0][0][1] == queues[1][0][1]:
            for queue in queues:
                queue.popleft()
            result['matched_records'] += 1
            continue
        for i in [0, 1]:
            fill(i, 16)
        a, b = queues[0][0], queues[1][0]
        ai = next((j for j, item in enumerate(queues[0]) if item[1] == b[1]), None)
        bi = next((j for j, item in enumerate(queues[1]) if item[1] == a[1]), None)
        if ai is None and bi is None:
            result['unresolved_divergence'] = {'run1': a[0], 'run2': b[0]}
            break
        index = 0 if ai is not None and (bi is None or ai <= bi) else 1
        count = ai if index == 0 else bi
        destination = 'missing_from_run2' if index == 0 else 'missing_from_run1'
        for _ in range(count):
            dropped = queues[index].popleft()[0]
            result[destination].append(dropped)
            print('Missing:', destination, dropped['seq'], dropped['data']['exit_reason'], dropped['tsc'], dropped['_previous_event'], flush=True)
    result['non_exit_streams_match'] = result['runs'][0].get('non_exit_semantic_hashes') == result['runs'][1].get('non_exit_semantic_hashes')
    (root/'trace-loss-diagnosis.json').write_text(json.dumps(result, indent=2) + '\n')
    print('Matched', result['matched_records'], 'Unresolved:', result['unresolved_divergence'], 'Non-exit streams match:', result['non_exit_streams_match'], flush=True)


if __name__ == '__main__':
    main()
