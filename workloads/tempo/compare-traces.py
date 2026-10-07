#!/usr/bin/env python3
"""Compare two full Bedrock traces using the repository's deterministic fields."""
import argparse
import collections
import hashlib
import itertools
import json
import re
import time
from pathlib import Path

try:
    import orjson
    decode = orjson.loads
    def encode(data):
        return orjson.dumps(data, option=orjson.OPT_SORT_KEYS)
except ImportError:
    decode = json.loads
    def encode(data):
        return json.dumps(data, sort_keys=True, separators=(',', ':')).encode()

FIELDS = "tsc rip rflags rax rbx rcx rdx rsi rdi rsp rbp r8 r9 r10 r11 r12 r13 r14 r15 memory_hash apic_hash serial_hash ioapic_hash rtc_hash mtrr_hash rdrand_hash fs_base gs_base kernel_gs_base cr3 cs_base ds_base es_base ss_base pending_dbg_exceptions interruptibility_state cow_page_count".split()
ANSI = re.compile(r'\x1b\[[0-9;]*m')


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def event_lines(path, follow):
    with path.open('rb') as stream:
        while True:
            position = stream.tell()
            line = stream.readline()
            if line.endswith(b'\n'):
                yield line
            elif not follow or (path.parent/'timing.json').exists():
                if line:
                    yield line
                break
            else:
                stream.seek(position)
                time.sleep(0.1)


def exits(path, summary, exclude_ept=False, follow=False):
    counts = collections.Counter()
    non_exit_hashes = collections.defaultdict(hashlib.sha256)
    exit_metadata_hash = hashlib.sha256()
    summary['deterministic_exits'] = 0
    summary['sequence_gaps'] = 0
    previous = -1
    deterministic_hash = hashlib.sha256()
    for line in event_lines(path, follow):
        record = decode(line)
        if record['seq'] != previous + 1:
            summary['sequence_gaps'] += 1
        previous = record['seq']
        counts[record['kind']] += 1
        if record['kind'] != 'exit':
            non_exit_hashes[record['kind']].update(encode({'tsc': record['tsc'], 'data': record['data']}) + b'\n')
        if record['kind'] == 'exit' and record['deterministic']:
            if exclude_ept and record['data']['exit_reason'] == 48:
                continue
            summary['deterministic_exits'] += 1
            data = {k: record['data'][k] for k in FIELDS}
            deterministic_hash.update(encode(data) + b'\n')
            exit_metadata_hash.update(encode({k: record['data'][k] for k in ['tsc', 'exit_reason', 'exit_qualification']}) + b'\n')
            yield record, data
    summary['event_counts'] = dict(counts)
    summary['deterministic_exit_sha256'] = deterministic_hash.hexdigest()
    summary['deterministic_exit_metadata_sha256'] = exit_metadata_hash.hexdigest()
    summary['non_exit_semantic_sha256'] = {k: h.hexdigest() for k, h in non_exit_hashes.items()}


def blocks(path):
    found = {}
    for line in ANSI.sub('', path.read_text()).splitlines():
        if 'Built payload' in line:
            match = re.search(r' number=(\d+) hash=(0x[0-9a-f]+)', line)
            if match:
                found[int(match[1])] = match[2]
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('trace_directory', type=Path)
    parser.add_argument('--exclude-ept', action='store_true', help='Also compare execution with EPT fault exits omitted')
    parser.add_argument('--follow', action='store_true', help='Follow growing traces until each run writes timing.json')
    args = parser.parse_args()
    root = args.trace_directory
    result = {'comparison_fields': FIELDS, 'host_timestamps_and_sequence_numbers_compared': False,
              'memory_hash_enabled': False, 'runs': [{}, {}], 'first_divergence': None}
    result['excluded_exit_reasons'] = [48] if args.exclude_ept else []
    streams = [exits(root / f'run{i+1}/events.jsonl', result['runs'][i], args.exclude_ept, args.follow) for i in range(2)]
    for index, pair in enumerate(itertools.zip_longest(*streams)):
        if result['first_divergence'] is not None:
            continue
        if None in pair:
            result['first_divergence'] = {'deterministic_exit_index_zero_based': index, 'reason': 'different deterministic exit counts'}
            continue
        (a, ad), (b, bd) = pair
        differences = {k: {'run1': ad[k], 'run2': bd[k]} for k in FIELDS if ad[k] != bd[k]}
        if differences:
            result['first_divergence'] = {'deterministic_exit_index_zero_based': index,
                'run1_sequence': a['seq'], 'run2_sequence': b['seq'],
                'differences': differences, 'run1_record': a, 'run2_record': b}
            print('First divergence:', json.dumps({k:v for k,v in result['first_divergence'].items() if not k.endswith('_record')}), flush=True)
    for i, summary in enumerate(result['runs'], 1):
        directory = root / f'run{i}'
        if args.follow:
            deadline = time.monotonic() + 10
            while not (directory/'txgen-report.json').exists() and time.monotonic() < deadline:
                time.sleep(0.1)
        report = json.loads((directory/'txgen-report.json').read_text())
        summary.update(json.loads((directory/'timing.json').read_text()))
        transactions = directory/'transactions.ndjson'
        summary['transactions_sha256'] = digest(transactions) if transactions.exists() else None
        summary['transaction_export_failed'] = not transactions.exists()
        summary['benchmark'] = {k: report[k] for k in ['sent', 'success', 'failed', 'elapsed_secs', 'tps', 'run_stats']}
        summary['transaction_composition'] = report['block_composition']['summary']
        text = ANSI.sub('', (directory/'console.log').read_text())
        summary['workload_passed'] = 'TEMPO_TXGEN_PASS' in text and 'TXGEN_EXIT_STATUS=0' in text
        summary['sparse_trie_confirmed'] = 'Sparse' in text or 'sparse_trie_state_root_wait_elapsed=Some' in text
        summary['pebs_registration_failed'] = 'PEBS registration failed' in text
    a, b = [blocks(root/f'run{i}/console.log') for i in (1,2)]
    common = sorted(a.keys() & b.keys())
    result['block_comparison'] = {'payload_counts': [len(a),len(b)], 'matching_hashes_at_same_height': sum(a[n] == b[n] for n in common),
        'first_different_height': next((n for n in common if a[n] != b[n]), None)}
    first = result['block_comparison']['first_different_height']
    if first is not None:
        result['block_comparison']['first_hashes'] = [a[first], b[first]]
    hashes = [run['transactions_sha256'] for run in result['runs']]
    result['identical_signed_transaction_streams'] = hashes[0] == hashes[1] if all(hashes) else None
    result['deterministic_exits_match'] = result['first_divergence'] is None
    result['deterministic_exit_metadata_match'] = result['runs'][0]['deterministic_exit_metadata_sha256'] == result['runs'][1]['deterministic_exit_metadata_sha256']
    result['non_exit_semantic_streams_match'] = result['runs'][0]['non_exit_semantic_sha256'] == result['runs'][1]['non_exit_semantic_sha256']
    output = 'comparison-without-ept.json' if args.exclude_ept else 'comparison.json'
    (root/output).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
