#!/usr/bin/env python3
import json,pathlib,subprocess,time,os
root=pathlib.Path(__file__).resolve().parent
runs=[]
cpu_set=os.environ.get('NATIVE_CPU_SET','2-6')
cpu_count=sum(int(t.split('-')[1])-int(t.split('-')[0])+1 if '-' in t else 1 for t in cpu_set.split(','))
result_root=root/os.environ.get('NATIVE_OUTPUT','.')
result_root.mkdir(exist_ok=True)

def docker(*args,**kwargs):
 return subprocess.run(['sudo','-n','docker',*map(str,args)],check=True,**kwargs)
for index in (1,2):
 out=result_root/f'native{index}';out.mkdir(exist_ok=True)
 tempo=f'bedrock-native-tempo-{index}';txgen=f'bedrock-native-txgen-{index}'
 try:
  docker('run','-d','--name',tempo,'--network','none','--cpuset-cpus',cpu_set,'--memory','16g',
   '--tmpfs','/data:rw,size=12g','-e','RUST_LOG=info,reth::engine::tree=debug',
   'bedrock/tempo-localnet:pinned','--bare','--block-time','200ms',stdout=subprocess.DEVNULL)
  args=['run','--name',txgen,'--network',f'container:{tempo}','--cpuset-cpus',cpu_set,'--memory','16g',
   '--tmpfs','/results:rw,size=1g','-v',f'{root}/fair-bench:/usr/local/bin/fair-bench:ro',
   '-v',f'{root}/fair-run.sh:/workload/run.sh:ro','-v',f'{out}:/export',
   '-e','BEDROCK=0','-e','TXGEN_COUNT=10000','-e','TXGEN_TPS=0','bedrock/tempo-txgen:latest']
  markers=[];passed=False
  begin=time.monotonic()
  with (out/'console.log').open('w') as log:
   proc=subprocess.Popen(['sudo','-n','docker',*args],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,bufsize=1)
   for line in proc.stdout:
    log.write(line);log.flush()
    if line.startswith('NATIVE_TIMING '):
     m=json.loads(line.split(' ',1)[1]);markers.append(m)
     print(f'Native {index}: boundary {m["marker"]}',flush=True)
    if 'TEMPO_TXGEN_PASS' in line:passed=True
   status=proc.wait()
  if status or not passed or len(markers)!=2:raise RuntimeError(f'Native run {index} failed; inspect {out}/console.log')
  report=json.loads((out/'report.json').read_text())
  wall=(markers[1]['host_monotonic_ns']-markers[0]['host_monotonic_ns'])/1e9
  result={'run':index,'execution':'native Ubuntu Docker','cpu_set':cpu_set,'actual_execution_cpus':cpu_count,'txgen_count':10000,'target_tps':0,
   'bench_host_seconds':wall,'bench_host_tps':10000/wall,'txgen_container_host_seconds':time.monotonic()-begin,'markers':markers,
   'sent':report['sent'],'success':report['success'],'failed':report['failed'],
   'zero_reverts':all(k['reverted_tx_count']==0 for k in report['block_composition']['summary']['kinds']),
   'benchmark_elapsed_seconds':report['elapsed_secs'],'block_run_stats':report['run_stats']}
  (out/'measurement.json').write_text(json.dumps(result,indent=2)+'\n');runs.append(result)
  print(json.dumps(result,indent=2),flush=True)
 finally:
  with (out/'tempo.log').open('w') as log:
   subprocess.run(['sudo','-n','docker','logs',tempo],stdout=log,stderr=subprocess.STDOUT)
  subprocess.run(['sudo','-n','docker','rm','-f','-v',txgen,tempo],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
(result_root/'native-results.json').write_text(json.dumps(runs,indent=2)+'\n')
