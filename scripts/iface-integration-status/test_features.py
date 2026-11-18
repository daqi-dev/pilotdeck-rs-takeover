"""Offline feature metadata and additive schema migration controls; synthetic only."""
import copy
import json
import tempfile
import unittest
import os
import stat
from unittest.mock import patch
from pathlib import Path

import extension as e
import root_apply as a
import test_status as base


def legacy_binding(new):
    old = copy.deepcopy(new)
    for kind, (_, key) in e.TABLES.items():
        old['tables'][kind]['fields'] = [key] + e.LEGACY_FIELDS[kind] + list(e.LEGACY_LINKS[kind])
        old['tables'][kind]['linkFields'].pop('关联接入变更')
    return old


class SchemaClient(base.FakeClient):
    def __init__(self, binding):
        super().__init__(binding)
        self.fields = {table: [{'id': 'protected-field-'+table, 'name': '原记录', 'type': 'text'}] for table in binding['protectedTableIds']}
        self.schema_fail = None
        for kind, table in binding['tables'].items():
            values = []
            for i, name in enumerate(table['fields']):
                value = {'id': 'field-' + kind + '-' + str(i), 'name': name, 'type': 'link' if name in table['linkFields'] else 'text'}
                if value['type'] == 'link':
                    value.update({k: table['linkFields'][name][k] for k in ('link_table', 'bidirectional')})
                values.append(value)
            self.fields[table['tableId']] = values

    def call(self, shortcut, table, body=None):
        if shortcut == '+field-list':
            return {'fields': copy.deepcopy(self.fields[table])}
        if shortcut != '+field-create':
            return super().call(shortcut, table, body)
        self.calls.append((shortcut, table, copy.deepcopy(body)))
        if self.schema_fail == 'before':
            self.schema_fail = None
            raise TimeoutError('unknown field create before service')
        field = dict(copy.deepcopy(body), id='new-field-' + str(len(self.calls)))
        self.fields[table].append(field)
        for row in self.rows[table]:
            row['fields'][body['name']] = None
        if self.schema_fail == 'after':
            self.schema_fail = None
            raise TimeoutError('unknown field create after service')
        if self.schema_fail == 'old_record':
            self.rows[table][0]['fields']['标题'] = 'concurrent edit'
        if self.schema_fail == 'old_field':
            self.fields[table][0]['id'] = 'concurrently replaced field id'
        if self.schema_fail == 'prior_new_field' and len(self.calls) > 1:
            next(x for x in self.fields[table] if x['id'].startswith('new-field-'))['id'] = 'replaced prior new field'
        if self.schema_fail == 'wrong_link' and field['type'] == 'link':
            field['link_table'] = 'protected-0'
        return {'created': True, 'field': copy.deepcopy(field)}


class FeatureControls(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.ledger = base.ledger_fixture()
        self.run = base.run_fixture()
        self.ledger['runs'] = [self.run]
        self.change = {'id': 'iface.change.synthetic', 'title': 'synthetic', 'sourceRepository': 'agent-kernel', 'sourceVersion': 'a' * 40, 'sourcePath': 'src/synthetic.rs', 'reviewDepth': 'synthetic', 'decision': 'review only', 'reason': 'synthetic', 'implementationStatus': '未核实', 'testStatus': '未实测', 'mergeStatus': '未核实', 'taskIds': ['iface.test.task'], 'contractIds': [], 'runIds': []}
        self.ledger['kernelChanges'] = [self.change]
        self.path = self.directory / 'ledger.json'
        base.save_fixture(self.path, self.ledger)
        self.metadata_path = self.directory / 'ledger.features.json'
        self.meta = e.load(self.metadata_path)
        self.meta['features'] = [{'id': 'iface.feature.model-input-inspection', 'title': '完整模型输入检查', 'requirement': 'synthetic requirement only', 'boundaries': ['no product acceptance'], 'evidence': copy.deepcopy(self.run['evidence'])}]
        self.association = {'id': 'iface.association.synthetic', 'featureId': self.meta['features'][0]['id'], 'collection': 'runs', 'recordId': self.run['id'], 'relatedChangeIds': [self.change['id']], 'proofKind': 'executed', 'proofScope': 'original synthetic only', 'appliesToVersions': copy.deepcopy(self.run['actualVersions']), 'runIds': [self.run['id']], 'recordSha256': e.sha(e.canonical(self.run).encode()), 'runFactSha256': {self.run['id']: e.sha(e.canonical(self.run).encode())}, 'evidence': copy.deepcopy(self.run['evidence']), 'reviewer': 'synthetic', 'reviewedAt': '2026-11-15T10:02:00+08:00'}
        self.meta['associations'] = [self.association]

    def project(self):
        e.save(self.metadata_path, self.meta)
        self.ledger['featureMetadata'] = {'path': str(self.metadata_path), 'sha256': e.sha(self.metadata_path.read_bytes())}
        e.save(self.path, self.ledger)
        return e.project(self.path)

    def test_explicit_features_links_and_original_run_fact_unchanged(self):
        original = e.canonical(self.run)
        projection = self.project()
        row = projection['tables']['runs']['rows'][0]
        self.assertEqual(row['事实JSON'], original)
        self.assertEqual(row['功能ID'], 'iface.feature.model-input-inspection')
        self.assertEqual(e.relation_values(row, '__featureChangeIds'), [self.change['id']])
        # Same contract does not auto-add feature association to contract/task/other run.
        self.assertEqual(projection['tables']['contracts']['rows'][0]['功能ID'], '')
        self.assertEqual(projection['tables']['tasks']['rows'][0]['功能ID'], '')
        self.assertEqual(projection['tables']['tasks']['rows'][-1]['记录类型'], '接入变更')
        self.assertEqual(projection['tables']['tasks']['rows'][-1]['目标分支'], 'master')
        self.assertEqual(projection['tables']['tasks']['rows'][-1]['合入状态'], '未核实（ssh://example.invalid/kernel.git/master）')
        binding = base.binding_fixture(); client = base.FakeClient(binding)
        plan = e.plan(projection, a.snapshot(client, binding))
        for table in plan['tables']:
            for op in table['operations']: client.call(op['shortcut'], op['tableId'], op['body'])
        links = e.link_operations(projection, a.snapshot(client, binding))
        run_record = client.rows['managed-runs'][0]
        task_ids = {r['fields']['任务ID']: r['id'] for r in client.rows['managed-tasks']}
        relation = next(op['body']['update_records'][run_record['id']]['关联接入变更'] for op in links if run_record['id'] in op['body']['update_records'])
        self.assertEqual(relation, [{'id': task_ids[self.change['id']]}])

    def test_metadata_missing_hash_disagreement_and_file_drift_rejected(self):
        self.ledger.pop('featureMetadata');e.save(self.path, self.ledger)
        with self.assertRaisesRegex(ValueError, 'explicit SHA-bound'): e.project(self.path)
        self.project(); self.metadata_path.write_text('{}')
        with self.assertRaisesRegex(ValueError, 'metadata drift'): e.project(self.path)
        self.project()
        with self.assertRaisesRegex(ValueError, 'disagreement'): e.project(self.path, feature_metadata={'path': str(self.metadata_path), 'sha256': '0' * 64})

    def test_classification_missing_duplicate_wrong_enum_and_unknown_record(self):
        valid = copy.deepcopy(self.meta)
        for mutation in ('missing', 'duplicate', 'enum', 'unknown', 'basis'):
            self.meta = copy.deepcopy(valid)
            if mutation == 'missing': self.meta['records'].pop()
            elif mutation == 'duplicate': self.meta['records'].append(copy.deepcopy(self.meta['records'][0]))
            elif mutation == 'enum': self.meta['records'][0]['changeNatures'] = ['ifACE变更']
            elif mutation == 'unknown': self.meta['records'][0]['recordId'] = 'iface.missing'
            else: self.meta['records'][0].pop('classificationBasis')
            with self.subTest(mutation=mutation), self.assertRaises(ValueError): self.project()

    def test_branch_identity_visibility_and_target_merge_are_separate(self):
        branch = self.meta['records'][-1]['branchIntegration']
        branch['localCommit'] = 'a' * 40
        branch['remoteVisibility'] = {'status': 'VISIBLE', 'observedAt': '2026-11-15T10:00:00+08:00', 'observedCommit': 'a' * 40, 'evidence': self.run['evidence'], 'referenceScope': 'EXPLICIT_REFS', 'refNames': ['refs/heads/master']}
        row = self.project()['tables']['tasks']['rows'][-1]
        self.assertEqual(row['远端可见'], '远端可见');self.assertEqual(row['目标主线合入'], '未核实')
        branch['targetBranch'] = 'main'
        with self.assertRaisesRegex(ValueError, 'target branch'): self.project()
        branch['targetBranch'] = 'master'
        branch['targetIntegration'] = {'status': 'MERGED', 'observedAt': '2026-11-15T10:00:00+08:00', 'observedCommit': 'c' * 40, 'evidence': self.run['evidence']}
        with self.assertRaisesRegex(ValueError, 'ancestry'): self.project()
        branch['targetIntegration']['ancestryEvidence'] = {'candidateCommit': 'a' * 40, 'targetCommit': 'c' * 40, 'isAncestor': True}
        self.assertEqual(self.project()['tables']['tasks']['rows'][-1]['合入状态'], '已合入（ssh://example.invalid/kernel.git/master）')
        branch['targetIntegration']['ancestryEvidence']['isAncestor'] = False
        with self.assertRaisesRegex(ValueError, 'ancestry'): self.project()

    def test_remote_non_ancestry_does_not_imply_not_visible(self):
        branch = self.meta['records'][-1]['branchIntegration']
        branch['localCommit'] = 'a' * 40
        branch['remoteVisibility'] = {'status': 'NOT_VISIBLE', 'observedAt': '2026-11-15T10:00:00+08:00', 'observedCommit': 'a' * 40, 'evidence': self.run['evidence'], 'referenceScope': 'EXPLICIT_REFS', 'refNames': ['refs/heads/master']}
        with self.assertRaisesRegex(ValueError, 'complete remote refs'): self.project()
        branch['remoteVisibility'].update(referenceScope='ALL_REMOTE_REFS', allRefsComplete=True)
        self.project()
        branch['remote'] = 'origin'
        with self.assertRaisesRegex(ValueError, 'actual remote URL'): self.project()

    def test_example_only_metadata_cannot_project(self):
        self.meta['exampleOnly'] = True
        with self.assertRaisesRegex(ValueError, 'example feature'): self.project()

    def test_feature_unknown_refs_and_execution_proof_cannot_be_borrowed(self):
        original = copy.deepcopy(self.association)
        for field, value in [('featureId', 'iface.feature.missing'), ('recordId', 'iface.missing'), ('relatedChangeIds', ['iface.test.task']), ('runIds', []), ('proofKind', 'build'), ('appliesToVersions', [{'component': 'synthetic-composition', 'commit': 'f' * 40, 'scope': 'current head'}])]:
            self.meta['associations'] = [dict(copy.deepcopy(original), **{field: value})]
            with self.subTest(field=field), self.assertRaises(ValueError): self.project()

    def test_original_unknown_identity_can_be_attached_without_retag(self):
        self.run['actualVersions'] = [{'component': 'old-dirty-source', 'identityStatus': 'UNCONFIRMED', 'reason': 'original source not pinned', 'scope': 'original negative control'}]
        self.association['appliesToVersions'] = copy.deepcopy(self.run['actualVersions'])
        self.association['recordSha256'] = e.sha(e.canonical(self.run).encode())
        self.association['runFactSha256'] = {self.run['id']: e.sha(e.canonical(self.run).encode())}
        with self.assertRaisesRegex(ValueError, 'original-only'): self.project()
        self.association['observedOriginalIdentityOnly'] = True
        row = self.project()['tables']['runs']['rows'][0]
        self.assertEqual(json.loads(row['事实JSON'])['actualVersions'], self.run['actualVersions'])
        self.association['appliesToVersions'][0]['reason'] = 'changed reason'
        with self.assertRaisesRegex(ValueError, 'retag'): self.project()

    def test_same_id_changed_classification_association_and_referenced_run_fact_rejected(self):
        self.ledger['tasks'][0]['nextAction'] = 'changed under same identity'
        with self.assertRaisesRegex(ValueError, 'classification source record hash'): self.project()
        self.ledger['tasks'][0].pop('nextAction', None)
        # Restore the exact fixture, then change run command without changing actualVersions.
        self.ledger['tasks'][0] = base.ledger_fixture()['tasks'][0]
        self.run['command'] = 'same versions changed original command'
        with self.assertRaisesRegex(ValueError, 'association source record hash'): self.project()
        self.association['recordSha256'] = e.sha(e.canonical(self.run).encode())
        with self.assertRaisesRegex(ValueError, 'referenced run fact hash'): self.project()

    def test_nonrun_association_binds_referenced_run_whole_fact(self):
        task=self.ledger['tasks'][0]
        self.association.update(collection='tasks', recordId=task['id'], recordSha256=e.sha(e.canonical(task).encode()))
        self.project()
        self.run['result']='FAIL'
        with self.assertRaisesRegex(ValueError,'referenced run fact hash'):self.project()

    def test_source_review_not_rebranded_run_or_delivery(self):
        self.association.update(collection='kernelChanges', recordId=self.change['id'], proofKind='source_review', runIds=[])
        self.association['recordSha256'] = e.sha(e.canonical(self.change).encode())
        self.association['runFactSha256'] = {}
        projection = self.project()
        self.assertEqual(projection['summary']['taskStatuses']['已交付'], 0)
        self.assertEqual(self.ledger['contracts'][0]['audit']['jointlyVerified']['status'], 'UNKNOWN')
        self.association['runIds'] = [self.run['id']]
        with self.assertRaisesRegex(ValueError, 'hash mismatch|impersonate'): self.project()

    def test_forged_metadata_projection_and_historical_fact_change_still_block(self):
        projection = self.project(); binding = base.binding_fixture(); client = base.FakeClient(binding)
        plan = e.plan(projection, a.snapshot(client, binding))
        projection['tables']['runs']['rows'][0]['功能ID'] = 'iface.feature.forged'
        with self.assertRaisesRegex(ValueError, 'forged'): a.apply(client, binding, projection, plan, self.directory/'receipt.json', self.directory/'binding.write-journal.json', True)
        projection = self.project()
        self.metadata_path.write_text('{}')
        with self.assertRaisesRegex(ValueError, 'metadata drift'): a.apply(client, binding, projection, plan, self.directory/'receipt.json', self.directory/'binding.write-journal.json', True)
        self.assertFalse(client.calls)


class MigrationControls(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.new = base.binding_fixture(); self.old = legacy_binding(self.new)
        self.client = SchemaClient(self.old)
        ledger = base.ledger_fixture();path = self.directory/'ledger.json';base.save_fixture(path, ledger)
        projection = e.project(path)
        for kind, table in projection['tables'].items():
            self.client.rows[self.old['tables'][kind]['tableId']] = [{'id': 'original-record-'+kind+'-'+str(i), 'fields': {k:v for k,v in row.items() if k in self.old['tables'][kind]['fields']}} for i,row in enumerate(table['rows'])]
        self.binding_path = self.directory/'binding.json';e.save(self.binding_path,self.old)
        self.journal_path = self.binding_path.with_suffix('.write-journal.json')
        self.main = a.journal_state(self.journal_path, self.old)
        self.main['attempts'] = [{'status': 'historical failure kept', 'receipt': 'old-failure.json'}]
        self.main['resolutions'] = [{'reason': 'explicit historical resolution retained'}]
        e.save(self.journal_path,self.main)
        self.before = a.schema_snapshot(self.client,self.old)
        self.plan = a.feature_schema_plan(self.old,self.new,self.before)

    def apply(self, execute=False, name='schema-receipt.json'):
        return a.apply_feature_schema(self.client,self.old,self.new,self.before,self.plan,self.binding_path,self.directory/name,execute)

    def test_dry_run_writes_nothing_and_success_preserves_records_and_history(self):
        self.assertEqual(self.apply()['status'],'DRY_RUN')
        self.assertFalse(self.client.calls)
        self.assertFalse(self.binding_path.with_suffix('.schema-journal.json').exists())
        result = self.apply(True)
        self.assertEqual(result['status'],'PASS_READBACK')
        self.assertEqual(len(self.client.calls),20)
        self.assertTrue(all(x[0]=='+field-create' and x[1].startswith('managed-') for x in self.client.calls))
        self.assertEqual(e.load(self.binding_path),self.new)
        migrated = e.load(self.journal_path)
        self.assertEqual(migrated['attempts'],self.main['attempts'])
        self.assertEqual(migrated['resolutions'],self.main['resolutions'])
        self.assertEqual(len(migrated['bindingMigrations']),1)
        schema_journal = e.load(self.binding_path.with_suffix('.schema-journal.json'))
        self.assertEqual(schema_journal['pendingOperations'],[])
        self.assertEqual(len(schema_journal['verifiedOperations']),20)
        self.assertEqual(len(schema_journal['verifiedFields']),20)
        self.assertTrue(all(x['field']['id'].startswith('new-field-') for x in schema_journal['verifiedFields']))
        a.verify_schema_progress(self.before,result['afterSnapshot'],self.old,self.new,self.plan['operations'])

    def test_atomic_save_fsyncs_file_then_parent_directory(self):
        observed=[];original=os.fsync
        def checked(fd):
            observed.append('directory' if stat.S_ISDIR(os.fstat(fd).st_mode) else 'file')
            return original(fd)
        with patch.object(a.os, 'fsync', side_effect=checked): a.atomic_save(self.directory/'durable.json', {'pending': True})
        self.assertEqual(observed, ['file', 'directory'])

    def test_directory_sync_failure_prevents_schema_dispatch(self):
        original=os.fsync
        def fail_directory(fd):
            if stat.S_ISDIR(os.fstat(fd).st_mode):raise OSError('synthetic directory sync failure')
            return original(fd)
        with patch.object(a.os,'fsync',side_effect=fail_directory), self.assertRaisesRegex(OSError,'directory sync failure'):self.apply(True)
        self.assertFalse(self.client.calls)
        self.assertEqual(e.load(self.binding_path),self.old)
        self.assertEqual(e.load(self.journal_path),self.main)

    def test_completed_migration_pins_field_ids_for_later_record_writes(self):
        self.apply(True)
        schema_state=e.load(self.binding_path.with_suffix('.schema-journal.json'))
        a.verify_persisted_schema(self.client,self.new,schema_state)
        self.client.fields['managed-tasks'][0]['id']='same-name-same-type-new-id'
        with self.assertRaisesRegex(ValueError,'field identity/properties drift'):a.verify_persisted_schema(self.client,self.new,schema_state)
        projection=e.project(self.directory/'ledger.json')
        plan=e.plan(projection,a.snapshot(self.client,self.new));calls=len(self.client.calls)
        with self.assertRaisesRegex(ValueError,'field identity/properties drift'):a.apply(self.client,self.new,projection,plan,self.directory/'records.json',self.journal_path,True)
        self.assertEqual(len(self.client.calls),calls)

    def test_target_binding_is_explicitly_unconfirmed_and_protected_schema_drift_blocks(self):
        proposed = a.proposed_feature_binding(self.old)
        self.assertFalse(proposed['schemaConfirmedByRoot'])
        with self.assertRaisesRegex(ValueError, 'confirm actual schemas'): e.validate_binding(proposed, proposed['baseToken'])
        self.client.fields['protected-0'][0]['id'] = 'changed protected field'
        with self.assertRaisesRegex(ValueError, 'drift'): self.apply(True)
        self.assertFalse(self.client.calls)

    def test_unknown_write_and_unknown_repair_block_before_schema_mutation(self):
        self.main['pendingOperations']=[{'unknown':'retained'}];e.save(self.journal_path,self.main)
        with self.assertRaisesRegex(ValueError,'unresolved'):self.apply(True)
        self.assertFalse(self.client.calls)
        self.main['pendingOperations']=[];e.save(self.journal_path,self.main)
        repair=self.completed_fact_repair_fixture();repair.update(status='STOP_REPAIR_UNKNOWN_OR_PARTIAL_REQUIRES_READBACK',pendingOperations=copy.deepcopy(repair['verifiedOperations']))
        e.save(self.binding_path.with_suffix('.fact-repair-journal.json'),repair)
        with self.assertRaisesRegex(ValueError,'unresolved fact repair'):self.apply(True)
        self.assertFalse(self.client.calls)

    def completed_fact_repair_fixture(self):
        operation={'shortcut':'+record-batch-update','tableId':self.old['tables']['runs']['tableId'],'body':{'update_records':{'original-repaired-run':{'事实JSON':'{"schema":"iface-fact-reference/v1","id":"iface.synthetic"}','行SHA256':'a'*64}}}}
        return {'schema':'iface-fact-repair-journal/v1','repairPlanSha256':'b'*64,'status':'PASS_TARGETED_REPAIR_READBACK','pendingOperations':[],'verifiedOperations':[operation]}

    def test_actual_completed_fact_repair_journal_allows_preview_without_rewrite(self):
        path=self.binding_path.with_suffix('.fact-repair-journal.json')
        e.save(path,self.completed_fact_repair_fixture());before=path.read_bytes()
        self.assertEqual(self.apply()['status'],'DRY_RUN')
        self.assertFalse(self.client.calls)
        self.assertEqual(path.read_bytes(),before)
        self.assertEqual(e.load(self.journal_path),self.main)
        self.assertFalse(self.binding_path.with_suffix('.schema-journal.json').exists())

    def test_repair_journal_schema_status_pending_and_history_fail_closed(self):
        original=self.completed_fact_repair_fixture();path=self.binding_path.with_suffix('.fact-repair-journal.json')
        variants=[]
        for status in ('STARTED','STOP_REPAIR_UNKNOWN_OR_PARTIAL_REQUIRES_READBACK','UNKNOWN','PASS_READBACK',None):
            variants.append(dict(copy.deepcopy(original),status=status))
        variants += [dict(copy.deepcopy(original),schema='iface-write-journal/v1'),dict(copy.deepcopy(original),pendingOperations=[original['verifiedOperations'][0]]),dict(copy.deepcopy(original),repairPlanSha256='not-a-hash'),dict(copy.deepcopy(original),verifiedOperations=[])]
        bad=copy.deepcopy(original);bad['verifiedOperations'][0]['tableId']='protected-0';variants.append(bad)
        bad=copy.deepcopy(original);bad['verifiedOperations'][0]['body']['update_records']['original-repaired-run']['标题']='unexpected';variants.append(bad)
        for index,variant in enumerate(variants):
            e.save(path,variant);before=path.read_bytes()
            with self.subTest(index=index),self.assertRaisesRegex(ValueError,'unresolved fact repair'):self.apply(True)
            self.assertFalse(self.client.calls)
            self.assertEqual(path.read_bytes(),before)
            self.assertEqual(e.load(self.journal_path),self.main)
            self.assertEqual(e.load(self.binding_path),self.old)

    def install_legacy_schema_predecessor(self):
        # Exact original journal shape, with only service identities mapped into this synthetic service.
        prior=e.load(Path(__file__).parent/'fixtures/legacy-schema-migration-v1.json')
        for index,operation in enumerate(prior['operations']):
            kind=operation['requested']['kind'];after=operation['after'] if index else [operation['after']]
            values=[]
            for field in after:
                current=next(value for value in self.client.fields[self.old['tables'][kind]['tableId']] if value['name']==field['name'])
                value=copy.deepcopy(field);value['id']=current['id']
                if value['type']=='link':value['link_table']=self.old['tables']['tasks']['tableId']
                current.clear();current.update(value);values.append(value)
            operation['after']=values if index else values[0]
            if index==0:
                operation['before']=dict(copy.deepcopy(values[0]),name='Kernel实现')
                operation['requested']['rename']['knownFieldId']=values[0]['id']
                operation['ack']['field']=copy.deepcopy(values[0])
            else:
                for specification in operation['requested']['createFields']:
                    if specification['type']=='link':specification['link_table']=self.old['tables']['tasks']['tableId']
                operation['ack']['fields' if len(values)>1 else 'field']=copy.deepcopy(values if len(values)>1 else values[0])
        prior['newBindingSha256']=self.main['bindingSha256']
        self.main['bindingMigrations']=[{'from':prior['oldBindingSha256'],'to':prior['newBindingSha256']}]
        e.save(self.journal_path,self.main)
        self.before=a.schema_snapshot(self.client,self.old)
        protected={key:{field:value[field] for field in ('count','sha256')} for key,value in self.before['protectedSnapshots'].items()}
        prior['protectedBefore']=copy.deepcopy(protected);prior['protectedAfter']=copy.deepcopy(protected)
        path=self.binding_path.with_suffix('.schema-journal.json');e.save(path,prior)
        self.plan=a.feature_schema_plan(self.old,self.new,self.before,e.sha(path.read_bytes()))
        return path,prior

    def test_original_legacy_schema_fixture_is_exact_retained_evidence(self):
        path=Path(__file__).parent/'fixtures/legacy-schema-migration-v1.json'
        self.assertEqual(e.sha(path.read_bytes()),'e4e0df74f5e15938ac0fd0277fddd15764cbb204ee7a64faac9e1dc24f79925b')
        prior=e.load(path)
        self.assertNotIn('pendingOperations',prior)
        self.assertEqual(len(prior['operations']),4)
        self.assertTrue(all(op['status']=='VERIFIED_READBACK' for op in prior['operations']))

    def test_completed_legacy_schema_continues_and_keeps_exact_predecessor_bytes(self):
        path,prior=self.install_legacy_schema_predecessor();raw=path.read_bytes()
        repair_path=self.binding_path.with_suffix('.fact-repair-journal.json');e.save(repair_path,self.completed_fact_repair_fixture());repair_raw=repair_path.read_bytes()
        preview=self.apply()
        self.assertEqual(preview['status'],'DRY_RUN');self.assertEqual(preview['predecessorSchemaJournalSha256'],e.sha(raw))
        self.assertEqual(path.read_bytes(),raw);self.assertEqual(repair_path.read_bytes(),repair_raw);self.assertFalse(self.client.calls)
        result=self.apply(True)
        self.assertEqual(result['status'],'PASS_READBACK');self.assertEqual(len(self.client.calls),20)
        journal=e.load(path)
        self.assertEqual(journal['predecessor']['rawUtf8'].encode('utf-8'),raw)
        self.assertEqual(journal['predecessor']['sha256'],e.sha(raw))
        self.assertEqual(journal['predecessor']['document'],prior)
        self.assertEqual(result['predecessor'],journal['predecessor'])
        self.assertEqual(repair_path.read_bytes(),repair_raw)
        self.assertEqual(e.load(self.journal_path)['bindingMigrations'][0],self.main['bindingMigrations'][0])
        self.assertEqual(journal['pendingOperations'],[])

    def test_legacy_unknown_partial_binding_fields_and_protected_drift_block(self):
        path,original=self.install_legacy_schema_predecessor();variants=[]
        for status in ('STARTED','UNKNOWN','STOP_UNKNOWN_OR_PARTIAL_REQUIRES_READBACK'):
            variants.append(dict(copy.deepcopy(original),status=status))
        variants += [dict(copy.deepcopy(original),newBindingSha256='0'*64),dict(copy.deepcopy(original),pendingOperations=[{'unknown':True}])]
        bad=copy.deepcopy(original);bad['operations'][1]['status']='ACK_PENDING_READBACK';variants.append(bad)
        bad=copy.deepcopy(original);bad['operations'][0]['before']['id']='different-old-field';variants.append(bad)
        bad=copy.deepcopy(original);bad['operations'][2]['after'][0]['id']='same-name-new-id';variants.append(bad)
        bad=copy.deepcopy(original);bad['operations'][2]['after'][-1]['link_table']='protected-0';variants.append(bad)
        bad=copy.deepcopy(original);bad['protectedAfter']['protected-0']['sha256']='0'*64;variants.append(bad)
        bad=copy.deepcopy(original);bad['protectedBefore']['protected-0']['sha256']='0'*64;bad['protectedAfter']=copy.deepcopy(bad['protectedBefore']);variants.append(bad)
        bad=copy.deepcopy(original);bad['operations'].pop();variants.append(bad)
        bad=copy.deepcopy(original);bad['operations'][1]['requested']['createFields'][0]['name']='unrecognized addition';variants.append(bad)
        for index,variant in enumerate(variants):
            e.save(path,variant);raw=path.read_bytes();self.plan=a.feature_schema_plan(self.old,self.new,self.before,e.sha(raw))
            with self.subTest(index=index),self.assertRaises(ValueError):self.apply(True)
            self.assertFalse(self.client.calls);self.assertEqual(path.read_bytes(),raw)
            self.assertEqual(e.load(self.binding_path),self.old);self.assertEqual(e.load(self.journal_path),self.main)

    def test_legacy_predecessor_plan_hash_and_main_history_must_match(self):
        path,prior=self.install_legacy_schema_predecessor()
        prior['operations'][1]['ack']['verification_hint']='changed after approval';e.save(path,prior)
        with self.assertRaisesRegex(ValueError,'predecessor journal drift'):self.apply(True)
        self.assertFalse(self.client.calls)
        self.plan=a.feature_schema_plan(self.old,self.new,self.before,e.sha(path.read_bytes()))
        changed=copy.deepcopy(self.main);changed['bindingMigrations']=[];e.save(self.journal_path,changed)
        with self.assertRaisesRegex(ValueError,'missing from write journal'):self.apply(True)
        self.assertFalse(self.client.calls)

    def test_legacy_predecessor_durability_failure_retains_full_evidence_and_blocks_dispatch(self):
        path,prior=self.install_legacy_schema_predecessor();raw=path.read_bytes();original=os.fsync
        def fail_directory(fd):
            if stat.S_ISDIR(os.fstat(fd).st_mode):raise OSError('synthetic directory sync failure')
            return original(fd)
        with patch.object(a.os,'fsync',side_effect=fail_directory),self.assertRaisesRegex(OSError,'directory sync failure'):self.apply(True)
        self.assertFalse(self.client.calls)
        retained=e.load(path)
        self.assertEqual(retained['predecessor']['rawUtf8'].encode('utf-8'),raw)
        self.assertEqual(retained['predecessor']['document'],prior)
        with self.assertRaises(ValueError):self.apply(True,'new-receipt.json')
        # Even regenerating the current-format plan cannot borrow the successful predecessor.
        self.plan=a.feature_schema_plan(self.old,self.new,self.before)
        with self.assertRaisesRegex(ValueError,'prior schema outcome unknown'):self.apply(True,'regenerated-receipt.json')
        self.assertFalse(self.client.calls)

    def test_unknown_field_create_never_retries_under_new_receipt(self):
        for mode in ('before','after'):
            with self.subTest(mode=mode):
                self.client.schema_fail=mode
                with self.assertRaises(TimeoutError):self.apply(True)
                calls=len(self.client.calls)
                with self.assertRaisesRegex(ValueError,'schema|snapshot'):self.apply(True,'different-receipt.json')
                self.assertEqual(len(self.client.calls),calls)
                self.assertEqual(e.load(self.journal_path),self.main)
                self.assertEqual(e.load(self.binding_path),self.old)
                journal=e.load(self.binding_path.with_suffix('.schema-journal.json'))
                self.assertTrue(journal['pendingOperations'])
                if mode=='before':
                    # Separate synthetic attempt state for the after-create negative control.
                    self.binding_path.with_suffix('.schema-journal.json').unlink()
                    (self.directory/'schema-receipt.json').unlink()
                    self.client.calls=[]

    def test_old_field_record_and_protected_drift_are_not_tolerated(self):
        self.client.rows['protected-0'][0]['fields']['原记录']='concurrent'
        with self.assertRaisesRegex(ValueError,'drift'):self.apply(True)
        self.assertFalse(self.client.calls)
        self.client.rows['protected-0'][0]['fields']['原记录']='不改'
        self.client.schema_fail='old_record'
        with self.assertRaisesRegex(ValueError,'old record'):self.apply(True)
        self.assertEqual(e.load(self.journal_path),self.main)
        self.assertEqual(e.load(self.binding_path),self.old)

    def test_old_actual_field_id_replacement_blocks_rebind(self):
        self.client.schema_fail='old_field'
        with self.assertRaisesRegex(ValueError,'old actual field'):self.apply(True)
        self.assertEqual(e.load(self.binding_path),self.old)

    def test_previously_created_field_identity_is_pinned(self):
        self.client.schema_fail='prior_new_field'
        with self.assertRaisesRegex(ValueError,'previously created field'):self.apply(True)
        self.assertEqual(e.load(self.binding_path),self.old)
        self.assertEqual(e.load(self.journal_path),self.main)

    def test_wrong_live_link_target_blocks_rebind(self):
        self.client.schema_fail='wrong_link'
        with self.assertRaisesRegex(ValueError,'actual link destination'):self.apply(True)
        self.assertEqual(e.load(self.binding_path),self.old)

    def test_target_rebind_rename_deletion_and_protected_ids_are_rejected(self):
        for mode in ('table','fields','link','protected'):
            bad=copy.deepcopy(self.new)
            if mode=='table':bad['tables']['tasks']['tableId']='other'
            elif mode=='fields':bad['tables']['tasks']['fields'].remove('标题')
            elif mode=='link':bad['tables']['tasks']['linkFields']['关联接入变更']['link_table']='protected-0'
            else:bad['protectedTableIds'][0]='other'
            with self.subTest(mode=mode), self.assertRaises(ValueError):a.feature_schema_plan(self.old,bad,self.before)

    def test_forged_plan_and_binding_journal_concurrency_block(self):
        self.plan['operations'][0]['body']['name']='arbitrary field'
        with self.assertRaisesRegex(ValueError,'forged'):self.apply(True)
        self.assertFalse(self.client.calls)
        self.plan=a.feature_schema_plan(self.old,self.new,self.before)
        changed=copy.deepcopy(self.old);changed['baseToken']='different';e.save(self.binding_path,changed)
        with self.assertRaisesRegex(ValueError,'binding file drift'):self.apply(True)
        self.assertFalse(self.client.calls)

    def test_unresolved_schema_journal_blocks_normal_record_apply(self):
        e.save(self.binding_path,self.new)
        e.save(self.journal_path,a.journal_state(self.directory/'absent',self.new))
        e.save(self.binding_path.with_suffix('.schema-journal.json'),{'schema':'iface-schema-journal/v1','status':'UNKNOWN','pendingOperations':[],'newBindingSha256':e.sha(e.canonical(self.new).encode())})
        ledger=base.ledger_fixture();path=self.directory/'record-ledger.json';base.save_fixture(path,ledger)
        projection=e.project(path);client=base.FakeClient(self.new);plan=e.plan(projection,a.snapshot(client,self.new))
        with self.assertRaisesRegex(ValueError,'unresolved schema journal'):a.apply(client,self.new,projection,plan,self.directory/'record-receipt.json',self.journal_path,True)
        self.assertFalse(client.calls)


if __name__=='__main__':unittest.main(verbosity=2)
