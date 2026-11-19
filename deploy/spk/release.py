#!/usr/bin/env python3

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

REPO = "ModernsDev/keeps"
PACKAGE = "KeepsNativeProbe.spk"


def api(path):
    return json.loads(subprocess.check_output(["gh", "api", "--hostname", "github.com", f"repos/{REPO}/{path}"]))


def main_revision():
    return api("git/ref/heads/main")["object"]["sha"]


def require_current_main(revision):
    if main_revision() != revision:
        raise ValueError(f"{revision} is not the current origin/main commit")


def require_approved_main(revision):
    require_current_main(revision)
    pulls = api(f"commits/{revision}/pulls?per_page=100")
    for pull in pulls:
        if pull["base"]["ref"] == "main" and pull["merged_at"] and pull["merge_commit_sha"] == revision:
            reviews = api(f"pulls/{pull['number']}/reviews?per_page=100")
            decisive = [review for review in reviews
                        if review["user"]["login"] == "hechuan9" and review["state"] != "COMMENTED"]
            if (decisive and decisive[-1]["state"] == "APPROVED"
                    and decisive[-1]["commit_id"] == pull["head"]["sha"]):
                break
    else:
        raise ValueError("origin/main must come from a merged PR approved by hechuan9 on its final revision")
    successful_run("ci.yml", revision, "push")


def successful_run(workflow, revision, event):
    query = f"branch=main&head_sha={revision}&event={event}&status=success"
    runs = api(f"actions/workflows/{workflow}/runs?{query}")["workflow_runs"]
    for run in runs:
        if (run["head_sha"] == revision and run["head_branch"] == "main"
                and run["event"] == event and run["conclusion"] == "success"
                and run["head_repository"]["full_name"] == REPO):
            return run["id"]
    raise ValueError(f"No successful {workflow} {event} run for origin/main at {revision}")


def fetch(output):
    revision = main_revision()
    run_id = successful_run("release-nas.yml", revision, "workflow_dispatch")
    if output.exists():
        raise ValueError(f"Output already exists: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output.parent) as temporary:
        artifact = Path(temporary) / "artifact"
        subprocess.run(["gh", "run", "download", str(run_id), "--repo", "github.com/" + REPO,
                        "--name", f"keeps-nas-spk-{revision}", "--dir", str(artifact)], check=True)
        if (artifact / "source-revision").read_text().strip() != revision:
            raise ValueError("SPK source revision does not match origin/main")
        with (artifact / PACKAGE).open("rb") as file:
            digest = hashlib.file_digest(file, "sha256").hexdigest()
        if (artifact / "SHA256SUMS").read_text().strip() != f"{digest}  {PACKAGE}":
            raise ValueError("SPK checksum does not match the release artifact")
        require_current_main(revision)
        artifact.rename(output)
    print(f"{revision} {output / PACKAGE}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check").add_argument("revision")
    commands.add_parser("fetch").add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "check":
            require_approved_main(args.revision)
        else:
            fetch(args.output)
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        parser.exit(1, f"{error}\n")
