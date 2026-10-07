#!/usr/bin/env python3
"""Measure matching txgen phases using guest log timestamps and host arrival times."""
import datetime, json, pathlib, re, shlex, subprocess, time
root = pathlib.Path(__file__).resolve().parent
repo = root.parents[2]
markers = ['Starting send', 'Bench send completed; starting post-processing', 'Txpool drain completed', 'Post-processing completed']
remote = "set -e; cp /home/dev/bedrock/workloads/tempo/compose-txgen-run.yaml /tmp/tempo-workload/compose.yaml; exec stdbuf -oL -eL bedrock-cli -m 16384 -s 12345 -c 'console=hvc0 nopti nokaslr mitigations=off break audit=0 bedrock_ncpus=5' -i /home/dev/bedrock/workloads/tempo/initrd.gz --file compose.yaml=/tmp/tempo-workload/compose.yaml --file images.tar=/tmp/tempo-workload/images.tar --wall-clock-timeout 600 /nix/store/h7cqcyr373vc5gdpr1cbp62qjl9v3709-linux-6.18.0/vmlinux"
cmd = ['sshpass','-p','root','ssh','-p','2222','root@127.0.0.1',remote]
result = {'txgen_count':10000,'target_tps':1000,'bedrock_seed':12345,'method':'Guest UTC timestamps in bench milestone logs versus Ubuntu monotonic timestamps on receipt of the same lines; host phase measurements include journal/SSH delivery latency. Full exit tracing disabled.','markers':{}}
started = time.monotonic()
with (root/'console.log').open('w') as log:
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
    for line in proc.stdout:
        now = time.monotonic()
        log.write(line)
        log.flush()
        for marker in markers:
            if marker in line and marker not in result['markers']:
                match = re.search(r'(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+Z)', line)
                if match:
                    timestamp = datetime.datetime.fromisoformat(match.group(1).replace('Z','+00:00')).timestamp()
                    result['markers'][marker] = {'guest_timestamp':match.group(1),'guest_seconds':timestamp,'host_monotonic_seconds':now}
                    print(marker, flush=True)
        if 'TEMPO_TXGEN_PASS' in line:
            result['workload_passed'] = True
        if 'Wall clock time:' in line:
            match = re.search(r'Wall clock time:\s+([\d.]+) seconds', line)
            if match: result['bedrock_run_loop_host_seconds'] = float(match.group(1))
    result['launcher_exit_code'] = proc.wait()
result['host_launcher_seconds'] = time.monotonic()-started
result['phases'] = {}
for name, end in [('send',markers[1]),('send_and_drain',markers[2]),('send_drain_and_postprocess',markers[3])]:
    a = result['markers'].get(markers[0]); b = result['markers'].get(end)
    if a and b:
        guest = b['guest_seconds']-a['guest_seconds']
        host = b['host_monotonic_seconds']-a['host_monotonic_seconds']
        result['phases'][name] = {'guest_seconds':guest,'host_seconds':host,'host_per_guest_second':host/guest}
if result['launcher_exit_code'] == 0:
    subprocess.run(['sshpass','-p','root','scp','-P','2222','root@127.0.0.1:/tmp/tempo-workload/txgen-report.json',str(root/'report.json')],check=True)
    report = json.loads((root/'report.json').read_text())
    result['benchmark'] = {k:report[k] for k in ['sent','success','failed','elapsed_secs','tps','run_stats']}
(root/'measurement.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2),flush=True)
if not result.get('workload_passed') or len(result['phases']) != 3:
    raise SystemExit('Measurement incomplete; see console.log')
