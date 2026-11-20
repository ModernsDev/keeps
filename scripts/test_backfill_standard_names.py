#!/usr/bin/env python3
import hashlib
import errno
from unittest.mock import patch
import json
from pathlib import Path
import sqlite3
import os
import subprocess
import tempfile
import unittest
from backfill_standard_names import plan, rewrite, update_database, publish


class BackfillTests(unittest.TestCase):
    def test_publish_links_without_overwrite_and_cross_device_copy(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            stage, linked, copied = root / 'stage', root / 'linked', root / 'copied'
            stage.write_bytes(b'generated')
            publish(stage, linked)
            self.assertEqual(stage.stat().st_ino, linked.stat().st_ino)
            with self.assertRaises(FileExistsError):
                publish(stage, linked)
            with patch('backfill_standard_names.os.link', side_effect=OSError(errno.EXDEV, 'cross device')):
                publish(stage, copied)
                with self.assertRaises(FileExistsError):
                    publish(stage, copied)
            self.assertEqual(copied.read_bytes(), b'generated')
            self.assertNotEqual(stage.stat().st_ino, copied.stat().st_ino)

    def test_only_owned_verified_files_and_collision(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            rawhash = 'a' * 64
            old = root / f'DSC.ARW.keeps-{rawhash}.heic'
            old.write_bytes(b'generated')
            original = root / 'DSC.ARW'
            original.write_bytes(b'raw')
            existing = root / 'DSC.heic'
            existing.write_bytes(b'user photo')
            db = sqlite3.connect(':memory:')
            db.executescript('CREATE TABLE catalog_paths(library_id,asset_id,path,content_hash); CREATE TABLE catalog_version_paths(path); CREATE TABLE catalog_versions(library_id,asset_id,content_hash,evidence);')
            digest = hashlib.sha256(old.read_bytes()).hexdigest()
            db.execute('INSERT INTO catalog_paths VALUES(?,?,?,?)', ['lib', 'asset', str(old), digest])
            db.execute('INSERT INTO catalog_versions VALUES(?,?,?,?)', ['lib', 'asset', digest, json.dumps({'generatedFrom': rawhash})])
            result = plan(db, root)
            self.assertEqual(result[0]['new'], str(root / 'DSC.1.heic'))
            self.assertEqual(original.read_bytes(), b'raw')
            self.assertEqual(existing.read_bytes(), b'user photo')
            old.write_bytes(b'changed')
            with self.assertRaisesRegex(RuntimeError, 'changed since indexing'):
                plan(db, root)

    def test_managed_catalog_rejects_legacy_apply_before_publication(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            subprocess.run(['server/target/debug/keeps-server', 'migrate'], env={**os.environ, 'KEEPS_ROOT': temp}, check=True, capture_output=True)
            catalog = root / 'db/control_plane.sqlite'
            original = root / 'photo.heic'
            original.write_bytes(b'original')
            result = subprocess.run([
                'python3', 'scripts/backfill_standard_names.py', '--control-db', str(catalog),
                '--jobs-db', str(root / 'db/jobs.sqlite'), '--photo-root', temp,
                '--plan', str(root / 'plan.json'), '--backup', str(root / 'backup'),
                '--apply', '--service-stopped',
            ], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('Use the Keeps API', result.stderr)
            self.assertEqual(original.read_bytes(), b'original')
            self.assertFalse((root / 'backup').exists())
            self.assertFalse((root / 'plan.json').exists())

    def test_identical_content_keeps_distinct_paths_and_asset_metadata(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            db = sqlite3.connect(':memory:')
            db.executescript('CREATE TABLE catalog_paths(library_id,asset_id,path,content_hash); CREATE TABLE catalog_version_paths(path); CREATE TABLE catalog_versions(library_id,asset_id,content_hash,evidence);')
            content_hash = hashlib.sha256(b'identical generated image').hexdigest()
            for index in range(2):
                source_hash = str(index) * 64
                old = root / f'DSC{index}.ARW.keeps-{source_hash}.heic'
                old.write_bytes(b'identical generated image')
                db.execute('INSERT INTO catalog_paths VALUES(?,?,?,?)', ['lib', str(index), str(old), content_hash])
                db.execute('INSERT INTO catalog_versions VALUES(?,?,?,?)', ['lib', str(index), content_hash, json.dumps({'generatedFrom': source_hash})])
            items = plan(db, root)
            self.assertEqual([Path(i['new']).name for i in items], ['DSC0.heic', 'DSC1.heic'])
            db.execute("ATTACH DATABASE ':memory:' AS jobsdb")
            db.executescript('CREATE TABLE jobsdb.files(path,asset_id,version,size,mtime_ns); CREATE TABLE jobsdb.jobs(current_path);')
            for index, item in enumerate(items):
                item.update(newHash='same-new-hash', size=123, mtimeNs=100+index)
                db.execute('INSERT INTO jobsdb.files VALUES(?,?,?,?,?)', [item['old'], item['asset'], content_hash, 1, 0])
            update_database(db, 'jobsdb', items)
            self.assertEqual(db.execute('SELECT path,asset_id,version,mtime_ns FROM jobsdb.files ORDER BY asset_id').fetchall(), [(items[i]['new'], str(i), 'same-new-hash', 100+i) for i in range(2)])

    def test_json_metadata_and_jobs_identity(self):
        item = dict(old='/x/old.heic', new='/x/new.heic', oldHash='oldhash', newHash='newhash', size=123, mtimeNs=456)
        changed = rewrite({'path': item['old'], 'contentHash': 'oldhash', 'sizeBytes': 1, 'mtimeNs': '2'}, {item['old']: item['new'], 'oldhash': 'newhash'}, {item['old']: item, 'oldhash': item})
        self.assertEqual(changed, {'path': item['new'], 'contentHash': 'newhash', 'sizeBytes': 123, 'mtimeNs': '456'})
        db = sqlite3.connect(':memory:')
        db.execute("ATTACH DATABASE ':memory:' AS jobsdb")
        db.executescript('CREATE TABLE jobsdb.files(path,asset_id,version,size,mtime_ns,metadata_stamp); CREATE TABLE jobsdb.jobs(current_path);')
        db.execute('INSERT INTO jobsdb.files VALUES(?,?,?,?,?,?)', [item['old'], 'stable-id', 'oldhash', 1, 2, 'oldstamp'])
        db.execute('INSERT INTO jobsdb.jobs VALUES(?)', [item['old']])
        update_database(db, 'jobsdb', [item])
        self.assertEqual(db.execute('SELECT * FROM jobsdb.files').fetchone(), (item['new'], 'stable-id', 'newhash', 123, 456, ''))
        self.assertEqual(db.execute('SELECT current_path FROM jobsdb.jobs').fetchone()[0], item['new'])


if __name__ == '__main__':
    unittest.main()
