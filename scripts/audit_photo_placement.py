#!/usr/bin/env python3
import argparse
from collections import Counter, defaultdict
from contextlib import closing
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import sys
import uuid
import xml.etree.ElementTree as ET

MEDIA = frozenset(
    "3fr ari arw bay cr2 cr3 crw dcr dng erf fff iiq k25 kdc mef mos mrw nef nrw "
    "orf pef raf raw rw2 rwl sr2 srf srw jpg jpeg heic heif hif png tif tiff "
    "mov mp4 m4v avi mkv mts m2ts".split()
)
VIDEO = frozenset("mov mp4 m4v avi mkv mts m2ts".split())
AUDITED_EXTENSIONS = MEDIA | {"xmp"}
STANDARD = "{http://ns.adobe.com/xap/1.0/}Identifier"
LEGACY = "{https://keeps.photos/ns/1.0/}PhotoID"
PREFIX = "https://keeps.photos/photo/"
SOFTWARE_IDS = tuple(
    "{http://ns.adobe.com/xap/1.0/mm/}" + name for name in ("OriginalDocumentID", "DocumentID")
)


@dataclass(frozen=True)
class Catalog:
    schema: int
    assets: set
    photo_ids: dict
    software_owners: dict
    deprecated: set
    locations: dict
    directories: dict
    indexed: dict
    registered_xmp: dict
    historical: int


def excluded(name):
    return name.startswith(".") or name in ("@eaDir", "#recycle")


def media_extension(path):
    return path.suffix[1:].lower()


def root_path(value):
    path = Path(value)
    if not path.is_absolute() or path != path.resolve(strict=True) or not path.is_dir():
        raise ValueError(f"图库根必须为真实、无符号链接的绝对目录 {value}")
    return path


def walk(roots, report):
    def raise_error(error):
        raise error

    for root in roots:
        for directory, dirs, names in os.walk(root, onerror=raise_error):
            parent = Path(directory)
            dirs[:] = [name for name in dirs if not excluded(name) and not (parent / name).is_symlink()]
            for name in names:
                path = parent / name
                if excluded(name) or media_extension(path) not in AUDITED_EXTENSIONS:
                    continue
                if not stat.S_ISREG(path.lstat().st_mode):
                    report("not_regular", paths=[str(path)])
                    continue
                yield path


def normalize_id(text):
    text = text.strip()
    for prefix in ("urn:uuid:", "xmp.did:", "uuid:"):
        if text.lower().startswith(prefix):
            text = text[len(prefix):]
            break
    try:
        value = uuid.UUID(text)
    except ValueError:
        return None
    return str(value) if value.int else None


def xmp_properties(root, name):
    values = {e.attrib[name].strip() for e in root.iter() if name in e.attrib}
    for element in root.iter(name):
        values.update(sub.text.strip() for sub in element.iter() if sub.text and sub.text.strip())
    return values


def stamped_file(path):
    entry = path.lstat()
    if not stat.S_ISREG(entry.st_mode):
        raise OSError(f"文件在校验期间已不是普通文件 {path}")
    return file_stamp(entry)


def read_sidecar(path):
    stamp = stamped_file(path)
    with path.open("rb") as source:
        if file_stamp(os.fstat(source.fileno())) != stamp:
            raise OSError(f"XMP 在校验期间变化 {path}")
        data = source.read()
    if stamped_file(path) != stamp:
        raise OSError(f"XMP 在校验期间变化 {path}")
    root = ET.fromstring(data)
    standard = {
        normalize_id(value[len(PREFIX):])
        for value in xmp_properties(root, STANDARD) if value.startswith(PREFIX)
    }
    legacy = {normalize_id(value) for value in xmp_properties(root, LEGACY)}
    software = {normalize_id(value) or value for key in SOFTWARE_IDS for value in xmp_properties(root, key) if value}
    return stamp, hashlib.sha256(data).hexdigest(), standard or legacy, software


def sidecar_names(path):
    for suffix in (".xmp", ".XMP"):
        yield path.with_suffix(suffix)
        yield path.with_name(path.name + suffix)


def file_stamp(info):
    return info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns


def file_digest(path):
    before = stamped_file(path)
    digest = hashlib.sha256()
    with path.open("rb") as source:
        if file_stamp(os.fstat(source.fileno())) != before:
            raise OSError(f"文件在校验期间变化 {path}")
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    if stamped_file(path) != before:
        raise OSError(f"文件在校验期间变化 {path}")
    return digest.hexdigest()


def indexed_files(db, library, report):
    sources = (
        ("catalog_paths", "SELECT path,asset_id,content_hash,1 FROM catalog_paths WHERE library_id=?"),
        ("catalog_version_paths", "SELECT path,asset_id,content_hash,available FROM catalog_version_paths WHERE library_id=?"),
    )
    rows = defaultdict(dict)
    for source, query in sources:
        for path, asset, digest, available in db.execute(query, (library,)):
            if source in rows[path]:
                report("duplicate_index_row", assets=[asset], paths=[path], table=source)
            if available not in (0, 1):
                report("invalid_path_availability", assets=[asset], paths=[path], table=source)
            rows[path][source] = asset, digest, available
    indexed = {}
    historical = 0
    for path, found in rows.items():
        if len(set(found.values())) != 1:
            report("conflicting_index_rows", assets=sorted({r[0] for r in found.values()}), paths=[path],
                   rows={t: dict(assetId=a, sha256=h, available=v) for t, (a, h, v) in found.items()})
        online = next((r for r in found.values() if r[2] == 1), None)
        if online:
            indexed[path] = online[:2]
        else:
            historical += 1
    return indexed, historical


def catalog_snapshot(db, library, report):
    def get(query):
        return db.execute(query, (library,))

    db.execute("BEGIN")
    schema = db.execute("PRAGMA user_version").fetchone()[0]
    if schema not in (24, 25):
        raise ValueError(f"不支持图库 schema {schema}，需要 24 或 25")
    assets = {asset for asset, in get("SELECT id FROM catalog_assets WHERE library_id=?")}
    roots = dict(get("SELECT asset_id,root_id FROM catalog_identity_roots WHERE library_id=?"))
    software_owners = defaultdict(set)
    for value, a in get("SELECT value,asset_id FROM catalog_photo_sources WHERE library_id=? AND kind='software'"):
        software_owners[normalize_id(value) or value].add(a)
    for value, a in get("SELECT root_id,asset_id FROM catalog_identity_aliases WHERE library_id=?"):
        software_owners[normalize_id(value) or value].add(a)
    for a, value in roots.items():
        software_owners[normalize_id(value) or value].add(a)
    deprecated = {path for path, in get("SELECT path FROM catalog_deprecated_files WHERE library_id=?")}
    locations = dict(get("SELECT asset_id,path FROM catalog_asset_locations WHERE library_id=?"))
    indexed, historical = indexed_files(db, library, report)
    directories, registered = {}, {}
    if schema == 25:
        directories = dict(get("SELECT asset_id,directory FROM catalog_asset_directories WHERE library_id=?"))
        for path, asset, digest, versioned in get(
            "SELECT path,asset_id,content_hash,versioned FROM catalog_physical_files WHERE library_id=? AND role='sidecar'"
        ):
            if versioned:
                report("versioned_xmp", assets=[asset], paths=[path])
            registered[Path(path)] = (asset, digest)
    for table, rowid, parent, number in db.execute("PRAGMA foreign_key_check"):
        report("foreign_key_violation", table=table, rowid=rowid, parentTable=parent, foreignKey=number)
    db.rollback()
    return Catalog(schema, assets, roots, software_owners, deprecated, locations, directories,
                   indexed, registered, historical)


def in_scope(path, raw, roots):
    return str(path) == raw and ".." not in path.parts and any(
        root in path.parents and not any(excluded(part) for part in path.relative_to(root).parts)
        for root in roots
    )


def audit_media(catalog, roots, media_files, verify_sha256, report):
    retained = defaultdict(set)
    visible = defaultdict(set)
    photo_assets = set()
    digests_checked = 0
    for raw, (asset, digest) in catalog.indexed.items():
        path = Path(raw)
        if asset not in catalog.assets:
            report("unknown_asset", assets=[asset], paths=[raw])
            continue
        if not in_scope(path, raw, roots):
            report("index_path_outside_scope", assets=[asset], paths=[raw])
            continue
        if catalog.schema == 25 and str(path.parent) != catalog.directories.get(asset):
            report("directory_index_mismatch", assets=[asset], paths=[raw], expected=catalog.directories.get(asset))
        retained[asset].add(path)
        if path not in media_files:
            report("indexed_media_missing", assets=[asset], paths=[raw])
            continue
        if verify_sha256:
            actual = file_digest(path)
            digests_checked += 1
            if actual != digest:
                report("media_sha256_mismatch", assets=[asset], paths=[raw], expected=digest, actual=actual)
        if raw not in catalog.deprecated:
            visible[asset].add(path)
        if media_extension(path) not in VIDEO:
            photo_assets.add(asset)

    for asset, paths in retained.items():
        parents = {p.parent for p in paths}
        if len(parents) != 1:
            report("multiple_directories", assets=[asset], paths=sorted(map(str, paths)))
        location = catalog.locations.get(asset)
        if location and Path(location) not in (visible.get(asset) or paths):
            report("location_not_active", assets=[asset], paths=[location, *sorted(map(str, paths))])
    for path in sorted(media_files):
        if str(path) not in catalog.indexed:
            report("media_not_indexed", paths=[str(path)])
    return retained, photo_assets, digests_checked


def audit_sidecars(catalog, media_files, xmp_files, register_xmp, report):
    paths_by_sidecar = defaultdict(set)
    for path in media_files:
        for candidate in set(sidecar_names(path)) & xmp_files:
            paths_by_sidecar[candidate].add(path)

    for path, (asset, _) in catalog.registered_xmp.items():
        if path not in xmp_files:
            report("registered_xmp_missing", assets=[asset], paths=[str(path)])

    covered = defaultdict(set)
    supplemental = []
    candidates = {}
    stamps = {}
    for sidecar in sorted(xmp_files):
        paths = paths_by_sidecar[sidecar]
        owners = {catalog.indexed[str(p)][0] for p in paths if str(p) in catalog.indexed}
        if not paths:
            report("unpaired_xmp", paths=[str(sidecar)])
            continue
        if not owners:
            report("xmp_without_indexed_media", paths=[str(sidecar), *sorted(map(str, paths))])
            continue
        if len(owners) != 1:
            report("xmp_shared_between_assets", assets=sorted(owners), paths=[str(sidecar), *sorted(map(str, paths))])
            continue
        asset = next(iter(owners))
        stamp, digest, identities, software = read_or_report(sidecar, asset, report)
        if stamp is None:
            continue
        stamps[sidecar] = stamp

        expected = normalize_id(catalog.photo_ids.get(asset, ""))
        if not expected or identities and identities != {expected}:
            report("xmp_identity_mismatch", assets=[asset], paths=[str(sidecar)],
                   expected=expected, actual=sorted(v for v in identities if v is not None))
            continue
        registered = catalog.registered_xmp.get(sidecar)
        if catalog.schema == 25:
            if str(sidecar.parent) != catalog.directories.get(asset):
                report("xmp_directory_mismatch", assets=[asset], paths=[str(sidecar)], expected=catalog.directories.get(asset))
                continue
            if registered:
                owner, recorded_digest = registered
                if owner != asset:
                    report("xmp_owner_mismatch", assets=sorted({asset, owner}), paths=[str(sidecar)])
                    continue
                if recorded_digest != digest:
                    report("xmp_sha256_mismatch", assets=[asset], paths=[str(sidecar)], expected=recorded_digest, actual=digest)
                    continue
            elif not register_xmp:
                report("xmp_not_registered", assets=[asset], paths=[str(sidecar)])

        if identities:
            if catalog.schema == 24 or registered or register_xmp:
                covered[asset].add(sidecar)
        else:
            owners_from_content = set().union(*(catalog.software_owners.get(v, ()) for v in software))
            if not registered and owners_from_content != {asset}:
                report("xmp_without_identity_evidence", assets=[asset], paths=[str(sidecar)],
                       softwareClaims=sorted(owners_from_content))
                continue
            supplemental.append((asset, sidecar))
        if register_xmp and not registered:
            candidates[sidecar] = (asset, digest)
            for path in paths:
                if path in stamps or str(path) not in catalog.indexed:
                    continue
                stamp = stamped_file(path)
                actual = file_digest(path)
                if stamped_file(path) != stamp:
                    report("file_changed_during_audit", assets=[asset], paths=[str(path)])
                    continue
                stamps[path] = stamp
                _, expected_digest = catalog.indexed[str(path)]
                if actual != expected_digest:
                    report("media_sha256_mismatch", assets=[asset], paths=[str(path)],
                           expected=expected_digest, actual=actual)

    for asset, sidecar in supplemental:
        if not covered[asset]:
            report("xmp_without_canonical_sibling", assets=[asset], paths=[str(sidecar)])
    return covered, candidates, stamps


def read_or_report(path, asset, report):
    try:
        return read_sidecar(path)
    except ET.ParseError as error:
        report("invalid_xmp", assets=[asset], paths=[str(path)], detail=str(error))
        return None, None, None, None


def register_sidecars(db, library, before, candidates, stamps, report):
    if not candidates:
        return 0
    db.execute("BEGIN IMMEDIATE")
    try:
        if db.execute("PRAGMA data_version").fetchone()[0] != before:
            report("catalog_changed_before_registration")
            return 0
        changed = [path for path, expected in stamps.items() if stamped_file(path) != expected]
        if changed:
            report("file_changed_before_registration", paths=sorted(map(str, changed)))
            return 0
        rows = [(library, str(path), asset, digest) for path, (asset, digest) in candidates.items()]
        db.executemany(
            "INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role,versioned)"
            " VALUES(?,?,?,?,'sidecar',0)", rows
        )
        db.commit()
        return len(rows)
    finally:
        if db.in_transaction:
            db.rollback()


def audit(db, library, roots, verify_sha256=False, register_xmp=False):
    issues = []

    def report(reason, **details):
        issues.append({"reason": reason, **details})

    before = db.execute("PRAGMA data_version").fetchone()[0]
    catalog = catalog_snapshot(db, library, report)
    if register_xmp and catalog.schema != 25:
        raise ValueError("--register-xmp 只适用于 schema 25")

    media_files, xmp_files = set(), set()
    for path in walk(roots, report):
        (xmp_files if media_extension(path) == "xmp" else media_files).add(path)

    retained, photo_assets, digests_checked = audit_media(catalog, roots, media_files, verify_sha256, report)
    covered, candidates, stamps = audit_sidecars(catalog, media_files, xmp_files, register_xmp, report)
    for asset in sorted(photo_assets - covered.keys()):
        report("photo_without_valid_xmp", assets=[asset], paths=sorted(map(str, retained[asset])))
    if db.execute("PRAGMA data_version").fetchone()[0] != before:
        report("catalog_changed_during_audit")
    registered = register_sidecars(db, library, before, candidates, stamps, report) if register_xmp and not issues else 0

    return {
        "libraryId": library,
        "schemaVersion": catalog.schema,
        "roots": list(map(str, roots)),
        "summary": {
            "indexedPaths": len(catalog.indexed), "historicalVersionPaths": catalog.historical,
            "mediaOnDisk": len(media_files), "xmpOnDisk": len(xmp_files),
            "activePhotoIdentities": len(photo_assets), "sha256Checked": digests_checked,
            "registeredXmp": len(catalog.registered_xmp) + registered,
            "newlyRegisteredXmp": registered,
            "issueCounts": dict(Counter(issue["reason"] for issue in issues)),
        },
        "issues": issues,
    }


def main():
    parser = argparse.ArgumentParser(description="一次性核对图库数据库、NAS 文件夹及 XMP，默认只读")
    parser.add_argument("--catalog", type=Path, required=True)
    parser.add_argument("--library", required=True)
    parser.add_argument("--root", type=root_path, action="append", required=True)
    parser.add_argument("--verify-sha256", action="store_true", help="对磁盘媒体逐个读取并校验 SHA-256")
    parser.add_argument("--register-xmp", action="store_true", help="Keeps 停写及备份后，在 schema 25 补登记符合条件的遗留 XMP")
    args = parser.parse_args()
    roots = sorted(set(args.root))
    roots = [p for p in roots if not any(q in p.parents for q in roots)]
    mode = "rw" if args.register_xmp else "ro"
    uri = args.catalog.resolve(strict=True).as_uri() + "?mode=" + mode
    with closing(sqlite3.connect(uri, uri=True, timeout=30)) as db:
        db.execute("PRAGMA foreign_keys=ON")
        if not args.register_xmp:
            db.execute("PRAGMA query_only=ON")
        report = audit(db, args.library, roots, args.verify_sha256, args.register_xmp)
    print(json.dumps(report, indent=2))
    return bool(report["issues"])


if __name__ == "__main__":
    sys.exit(main())
