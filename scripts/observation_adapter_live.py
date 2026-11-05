#!/usr/bin/env python3
"""Native sandbox observation protocol acceptance in a disposable project.
Synthetic inspection/diagnosis worker and fixture actuator; native macOS read-only
observations and real Codex sandbox delivery checks.
No production endpoint, credential contents or business-code integration.
"""
import argparse,hashlib,json,os,shutil,subprocess,sys,tempfile,time
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
PROBE=r'''import errno,hashlib,json,os,pathlib,socket,sys,time
b=json.loads(os.environ['TELOS_OBSERVATION_CONTEXT'])
p=pathlib.Path(sys.argv[1]);raw=p.read_bytes();d=json.loads(raw)
try:
    pathlib.Path('forbidden-write').write_text('must be denied')
    raise RuntimeError('read-only observation unexpectedly allowed a project write')
except PermissionError: pass
try:
    pathlib.Path(sys.argv[2]).read_text()
    raise RuntimeError('observation read outside declared input roots')
except PermissionError: pass
try:
    with socket.socket() as client: client.connect(('127.0.0.1',9))
    raise RuntimeError('observation unexpectedly allowed network access')
except OSError as error:
    if error.errno not in (errno.EPERM,errno.EACCES): raise
v='passed' if d.get('value')==1 else 'failed'
if b.get('subject_id') is not None and d.get('subject_id')!=b['subject_id']:v='failed'
mode=d.get('mode','normal');observed=int(time.time())
coverage={'start':b['window_start'],'end':b['window_end'],'complete':True,'gap':False,'backlog_bytes':0}
evidence=['sha256:'+hashlib.sha256(raw).hexdigest()]
if mode=='gap':coverage['gap']=True
if mode=='backlog':coverage['backlog_bytes']=1048576
if mode=='stale':observed-=1000
if mode=='empty':evidence=[]
if mode=='wrong-binding':b['source_id']='unrelated-source'
print(json.dumps({'binding':b,'observed_at':observed,'coverage':coverage,'verdict':v,'summary':'Independent fixture predicate and subject check', 'evidence':evidence,'signals':[{'resource_key':'physical-resource-A','episode_key':'recovery-1','kind':'unavailable'}]*111}))
'''

def main():
 parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--output',type=Path,required=True);parser.add_argument('--codex',default=shutil.which('codex'));args=parser.parse_args();assert args.codex
 out=args.output.resolve();out.mkdir(parents=True,exist_ok=False)
 project=out/'project';project.mkdir();(project/'value.txt').write_text('pending')
 for cmd in [('init','--quiet'),('add','.'),('commit','-qm','Fixture baseline')]:
  subprocess.run(['git','-C',str(project),'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','-c','commit.gpgSign=false','-c','core.hooksPath=/dev/null',*cmd],check=True,capture_output=True)
 acceptance=out/'acceptance';acceptance.mkdir();probe=acceptance/'probe.py';probe.write_text(PROBE);data=acceptance/'measurement.json'
 def set_measurement(value=0,mode='normal',subject='old-subject'):data.write_text(json.dumps({'value':value,'mode':mode,'subject_id':subject}))
 set_measurement()
 outside=out/'outside-input.txt';outside.write_text('must remain unreadable to the probe')
 wrapper=out/'fixture-codex';wrapper.write_text('#!/usr/bin/env python3\nimport json,os,pathlib,sys\nREAL='+repr(str(Path(args.codex).resolve()))+'\n'+r'''
if sys.argv[1]=='sandbox':os.execv(REAL,[REAL,*sys.argv[1:]])
assert sys.argv[sys.argv.index('--sandbox')+1]=='read-only', 'continuous worker was not read-only'
sys.stdin.read()
pathlib.Path(sys.argv[sys.argv.index('-o')+1]).write_text(json.dumps({'status':'done','summary':'Synthetic read-only inspection/diagnosis fixture','evidence_paths':[],'remaining_work':[]}))
print(json.dumps({'type':'turn.completed','usage':{'output_tokens':0}}))
''');wrapper.chmod(0o700)
 binary=ROOT/'target/debug/telos';state=out/'state';report={'status':'running','scope':'Disposable project; synthetic read-only worker/stage actuator; native macOS read-only observations and real Codex sandbox delivery acceptance','binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()}
 def call(request):
  p=out/'request.json';p.write_text(json.dumps(request));r=subprocess.run([str(binary),'--state-dir',str(state),'request',str(p)],capture_output=True,text=True,timeout=60)
  assert r.returncode==0,r.stderr
  return json.loads(r.stdout)
 def wait(test,seconds=45):
  deadline=time.monotonic()+seconds
  while time.monotonic()<deadline:
   value=test()
   if value:return value
   time.sleep(.2)
  raise RuntimeError('fixture acceptance deadline exceeded')
 def verify(goal):return call({'op':'verify_loop','id':goal,'revision':1})
 def readiness(goal, structured=True):
  args=[str(binary),'--state-dir',str(state),'readiness','--id',goal]
  result=subprocess.run(args+(['--json'] if structured else []),check=True,capture_output=True,text=True,timeout=15)
  return json.loads(result.stdout) if structured else result.stdout
 def artifact(evidence_id):
  result=bytearray();offset=0
  while True:
   page=call({'op':'get_evidence','id':evidence_id,'offset':offset,'limit':32768});result.extend(page['bytes'])
   if page['next_offset'] is None:return json.loads(result)
   offset=page['next_offset']
 with (out/'daemon.log').open('w') as log:daemon=subprocess.Popen([str(binary),'--state-dir',str(state),'daemon','--codex',str(wrapper)],stdout=log,stderr=log)
 started=time.monotonic()
 try:
  def ready():
   try:return call({'op':'ping'})
   except AssertionError:return None
  wait(ready)
  call({'op':'register_project','id':'fixture','root':str(project)})
  parent=call({'op':'create_loop','project_id':'fixture','kind':'continuous','name':'Generic project observation','contract':{'goal':'Observe actual fixture state','delivery_scope':'Read-only fixture; no external actions','criteria':[{'id':'health','description':'Independent project predicate with fresh complete coverage'}],'interval_seconds':21600,'max_attempts':2}})
  parent_id=parent['id'];report['parent_id']=parent_id
  initial_events=call({'op':'events','id':parent_id})
  initial=readiness(parent_id)
  assert parent['next_due_at'] is not None
  assert initial['configuration']['missing']==['execution_policy_missing','verification_plan_missing']
  assert initial['collection']['status']=='no_evidence' and initial['first_verification'] is None
  assert initial['inspection']['status']=='not_scheduled' and initial['inspection']['next_due_at'] is None
  assert 'Inspection: not scheduled' in readiness(parent_id,structured=False)
  assert call({'op':'events','id':parent_id})==initial_events
  call({'op':'configure_project','id':'fixture','expected_revision':0,'policy':{'enabled':True,'sandbox':'workspace_write','timeout_seconds':30,'max_output_bytes':131072,'max_steps_per_cycle':2}})
  started_goal=call({'op':'start_loop','id':parent_id,'revision':1,'request_key':'inspect-fixture'})
  wait(lambda:call({'op':'loop_context','id':parent_id,'revision':1})['recent_execution'])
  policy={'source_id':'fixture-source','window_seconds':5,'max_age_seconds':10,'poll_interval_seconds':3600,'after_stage':'activate-fixture'}
  plan={'test_checks':[{'criterion_id':'health','adapter':'local.observation_json','observation':policy,'command':{'program':str(Path(sys.executable).resolve()),'args':[str(probe),str(data),str(outside)],'readable_roots':[str(Path(sys.prefix).resolve()),str(acceptance)],'timeout_seconds':15,'max_output_bytes':131072}}]}
  call({'op':'configure_verification','id':parent_id,'revision':1,'expected_version':0,'plan':plan})
  def diagnosed():
   ctx=call({'op':'loop_context','id':parent_id,'revision':1})
   return ctx if len(ctx['recent_execution'])==2 and ctx['latest_verification'] and not ctx['progress']['execution_in_flight'] else None
  ctx=wait(diagnosed)
  assert ctx['latest_verification']['results'][0]['verdict']=='failed'
  # One diagnosis only. Wait until its automatic follow-up verifier is idle.
  wait(lambda:call({'op':'verification_status','id':parent_id})['active_verification'] is None)
  ctx=call({'op':'loop_context','id':parent_id,'revision':1});diagnosis=ctx['recent_execution'][0]['step_id']
  report['initial_failed']=ctx['latest_verification'];report['cases']={}
  first_observation=artifact(ctx['latest_verification']['results'][0]['evidence_ids'][0])
  assert first_observation['observations'][0]['detail']['report']['exit_code']==0, 'Semantic failure must come from the project assertion, not a failed sandbox process'
  assert first_observation['observations'][0]['detail']['assertion']['unique_signals']==1
  for mode in ['gap','backlog','stale','empty','wrong-binding']:
   set_measurement(value=1,mode=mode)
   result=verify(parent_id);assert result['verdict']==('error' if mode=='wrong-binding' else 'inconclusive'),(mode,result)
   report['cases'][mode]=result['verdict']
  signal={'resource_key':'physical-resource-A','episode_key':'recovery-1','kind':'unavailable'}
  batch={'source_id':'fixture-source','request_key':'range-1','expected_version':0,'generation':'generation-1','start':0,'end':262144,'source_size':1048576,'gap_reason':None,'signals':[signal]*111,'evidence':['fixture-range-digest']}
  first=call({'op':'record_observation_batch','id':parent_id,'revision':1,'batch':batch});assert first['new_signals']==1
  assert call({'op':'record_observation_batch','id':parent_id,'revision':1,'batch':batch})==first
  collected=readiness(parent_id)
  assert collected['configuration']['missing']==[]
  assert collected['collection']['status']=='evidence_recorded'
  assert collected['collection']['receipts'][0]['backlog_bytes']==786432
  assert collected['inspection']['armed'] and collected['inspection']['next_due_at'] is None
  assert collected['first_verification']['verdict']=='failed'
  set_measurement(value=1)
  assert verify(parent_id)['verdict']=='inconclusive','Known collection backlog must override a project pass'
  for index in range(2,5):
   batch.update(request_key='range-'+str(index),expected_version=index-1,start=(index-1)*262144,end=index*262144)
   receipt=call({'op':'record_observation_batch','id':parent_id,'revision':1,'batch':batch});assert receipt['new_signals']==0
  report['collection']={'first_new_signals':first['new_signals'],'final_backlog_bytes':receipt['backlog_bytes'],'final_version':receipt['version']}
  child=call({'op':'create_loop','project_id':'fixture','kind':'delivery','name':'Synthetic fixture delivery','contract':{'goal':'Fixture value is ready','delivery_scope':'Isolated fixture worktree only','criteria':[{'id':'value','description':'Independent native command checks fixture bytes'}],'interval_seconds':None,'max_attempts':2}})
  report['child_id']=child['id']
  workspace=call({'op':'prepare_workspace','id':child['id'],'revision':1,'policy_revision':1,'base':'HEAD'})
  call({'op':'configure_verification','id':child['id'],'revision':1,'expected_version':0,'plan':{'test_checks':[{'criterion_id':'value','command':{'program':'/bin/sh','args':['-c','test "$(cat value.txt)" = ready'],'readable_roots':['/bin','/usr/bin','/usr/lib','/System/Library'],'timeout_seconds':10,'max_output_bytes':4096}}]}})
  (Path(workspace['path'])/'value.txt').write_text('ready')
  assert verify(child['id'])['verdict']=='passed'
  assert readiness(child['id'])['first_verification']['verdict']=='passed'
  call({'op':'associate_remediation','id':parent_id,'revision':1,'diagnosis_step_id':diagnosis,'delivery_loop_id':child['id'],'delivery_revision':1,'request_key':'fixture-association'})
  assert verify(parent_id)['verdict']=='inconclusive','Delivery pass alone must not establish recovery'
  delivery=call({'op':'delivery_result','id':child['id'],'revision':1})
  stage=call({'op':'record_project_stage','id':child['id'],'revision':1,'receipt':{'request_key':'fixture-stage','stage':'activate-fixture','artifact':delivery['artifact'],'subject_id':'fixture-ready-v1','outcome':'succeeded','evidence_refs':['fixture-actuator-output-only']}})
  assert verify(parent_id)['verdict']=='inconclusive','Pre-stage observations must not satisfy the fresh window'
  wait(lambda:int(time.time())>=stage['recorded_at']+6,seconds=10)
  set_measurement(value=1,subject='old-subject')
  assert verify(parent_id)['verdict']=='failed','Wrong measured subject must fail even after a delivery and stage pass'
  set_measurement(value=1,subject='fixture-ready-v1')
  result=verify(parent_id);assert result['verdict']=='passed'
  observation=artifact(result['evidence_id']);assert observation['observations'][0]['detail']['assertion']['unique_signals']==1
  assert not (project/'forbidden-write').exists()
  current=call({'op':'get_loop','id':parent_id});assert current['state']=='active'
  final=call({'op':'loop_context','id':parent_id,'revision':1});assert final['active_cycle_id'] is None
  assert final['remediation_links']['items'][0]['recovery_check']['verdict']=='passed'
  recovered=readiness(parent_id)
  assert recovered['inspection']['armed'] and recovered['inspection']['next_due_at']==current['next_due_at']
  assert recovered['first_verification']==collected['first_verification']
  report['readiness']={'initial':initial,'collected':collected,'recovered':recovered}
  report.update(status='passed',elapsed_seconds=round(time.monotonic()-started,2),checks={'native_readonly_probe_enforced':True,'zero_exit_failed_assertion_not_accepted':True,'missing_stale_gap_backlog_wrong_binding_not_accepted':True,'collection_backlog_overrides_pass':True,'bounded_catchup_and_durable_cursor':True,'physical_episode_dedup':True,'continuous_readonly_under_project_write_grant':True,'delivery_does_not_accept_parent':True,'stage_requires_fresh_window_and_measured_subject':True,'parent_recovery_is_independent':True},final=result)
 finally:
  daemon.terminate();daemon.wait(timeout=30);report['daemon_exit_code']=daemon.returncode
  (out/'report.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
if __name__=='__main__':main()
