#!/usr/bin/env python3
"""Measure the same complete bench command at direct hypercall boundaries."""
import json, pathlib, shlex, subprocess, time
root = pathlib.Path(__file__).resolve().parent
cli = (root/'cli-path.txt').read_text().strip()+'/bin/bedrock-cli'
remote_root = '/home/dev/bedrock/workloads/tempo/fair-timing'
ssh = ['sshpass','-p','root','ssh','-p','2222','root@127.0.0.1']
results = []
for index in (1,2):
    directory = root/f'run{index}'
    directory.mkdir(exist_ok=True)
    remote = 'set -e; cp '+remote_root+"/compose.yaml /tmp/tempo-workload/compose.yaml; exec "+shlex.join([
        cli,'-m','16384','-s','12345','-c','console=hvc0 nopti nokaslr mitigations=off break audit=0 bedrock_ncpus=5',
        '-i',remote_root+'/initrd.gz','--file','compose.yaml=/tmp/tempo-workload/compose.yaml',
        '--file','images.tar=/tmp/tempo-workload/images.tar','--wall-clock-timeout','600',
        '/nix/store/h7cqcyr373vc5gdpr1cbp62qjl9v3709-linux-6.18.0/vmlinux'])
    result = {'run':index,'txgen_count':10000,'target_tps':1000,'markers':[],'full_exit_tracing':False}
    start=time.monotonic()
    with (directory/'console.log').open('w') as output:
        proc=subprocess.Popen(ssh+[remote],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,bufsize=1)
        for line in proc.stdout:
            received=time.monotonic()
            output.write(line); output.flush()
            if line.startswith('FAIR_TIMING '):
                marker=json.loads(line.split(' ',1)[1])
                marker['ubuntu_received_monotonic_seconds']=received
                result['markers'].append(marker)
                print(f'Run {index} boundary {marker["marker"]}: guest {marker["guest_monotonic_ns"]/1e9:.6f}s, host {marker["host_seconds"]:.6f}s',flush=True)
            if line.startswith('FAIR_LIFETIME '):
                life=json.loads(line.split(' ',1)[1])
                result['bedrock_lifetime']={'guest_virtual_seconds':life['guest_tsc']/life['tsc_frequency'],'host_seconds':life['host_seconds']}
            if 'TEMPO_TXGEN_PASS' in line: result['workload_passed']=True
        result['launcher_exit_code']=proc.wait()
    result['launcher_host_seconds']=time.monotonic()-start
    if result['launcher_exit_code'] != 0 or not result.get('workload_passed') or len(result['markers']) != 2:
        (directory/'measurement.json').write_text(json.dumps(result,indent=2)+'\n')
        raise SystemExit(f'Run {index} failed; inspect {directory}/console.log')
    a,b=result['markers']
    if (a['marker'],b['marker']) != (1,2): raise SystemExit('Unexpected marker order')
    guest=(b['guest_monotonic_ns']-a['guest_monotonic_ns'])/1e9
    host=b['host_seconds']-a['host_seconds']
    ubuntu=b['ubuntu_received_monotonic_seconds']-a['ubuntu_received_monotonic_seconds']
    result['bench_command']={'guest_monotonic_seconds':guest,'guest_virtual_tsc_seconds':(b['guest_tsc']-a['guest_tsc'])/a['tsc_frequency'],
        'host_seconds':host,'ubuntu_marker_receipt_seconds':ubuntu,'host_clock_vs_ubuntu_receipt_difference_seconds':host-ubuntu,
        'host_per_guest_second':host/guest,'host_transactions_per_second':10000/host}
    subprocess.run(['sshpass','-p','root','scp','-P','2222','root@127.0.0.1:/tmp/tempo-workload/txgen-report.json',str(directory/'report.json')],check=True)
    report=json.loads((directory/'report.json').read_text())
    result['benchmark']={k:report[k] for k in ['sent','success','failed','elapsed_secs','tps','run_stats']}
    result['zero_reverts']=all(v['reverted_tx_count']==0 for v in report['block_composition']['summary']['kinds'])
    (directory/'measurement.json').write_text(json.dumps(result,indent=2)+'\n')
    results.append(result)
    print(json.dumps(result['bench_command'],indent=2),flush=True)
summary={'method':'A static guest wrapper issues READY hypercalls immediately before fork/exec of bench send and immediately after waitpid. Guest CLOCK_MONOTONIC nanoseconds travel in registers. Instrumented CLI records Instant elapsed immediately on vm.run return before processing serial events. Both clocks bracket the same complete bench command. The CLI host clock runs in outer NixOS/KVM; Ubuntu marker-receipt clock is an independent transport-limited check. No journal timestamps are used.','runs':results}
(root/'measurement.json').write_text(json.dumps(summary,indent=2)+'\n')
print('Saved fair-timing/measurement.json',flush=True)
