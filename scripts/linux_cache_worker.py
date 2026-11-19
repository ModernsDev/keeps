#!/usr/bin/env python3
"""Bounded remote cache worker; NAS remains the sole state/file publisher."""
import concurrent.futures
import argparse
import contextlib
import http.server
import hashlib
import json
import logging
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

LOG = logging.getLogger("keeps-worker")
STOP = threading.Event()


def tree_bytes(root):
    total = 0
    for path in root.rglob("*"):
        try:
            if path.is_file():
                total += path.stat().st_size
        except FileNotFoundError:
            pass
    return total


def verify_renderer(root):
    # Exercise the installed binary and codecs: a version label cannot detect stale images.
    with tempfile.TemporaryDirectory(prefix="renderer-check-", dir=root) as temp:
        directory = Path(temp)
        env = dict(os.environ, TMPDIR=temp, OMP_NUM_THREADS="1", MAGICK_THREAD_LIMIT="1")

        def run(*command):
            return subprocess.run(command, env=env, check=True, timeout=180, stdout=subprocess.PIPE).stdout

        source = directory / "source.heic"
        run("convert", "-size", "1024x512", "gradient:red-blue", "-depth", "16", "PNG48:" + str(directory / "source.png"))
        run("heif-enc", "-b", "10", "-q", "95", "-p", "x265:pools=1", "-p", "x265:frame-threads=1", "-o", str(source), str(directory / "source.png"))
        metadata = json.loads(run("exiftool", "-j", "-BitDepthLuma", "-BitDepthChroma", str(source)))[0]
        if metadata.get("BitDepthLuma") != 10 or metadata.get("BitDepthChroma") != 10:
            raise RuntimeError("HEIF renderer self-check failed: fixture is not 10-bit HEIF")
        run("heif-convert", "--quiet", str(source), str(directory / "reference.png"))
        run("keeps-render", str(source), temp, "false", "512", "50")
        run("heif-convert", "--quiet", str(directory / "thumbnail.heic"), str(directory / "actual.png"))
        reference = run("convert", str(directory / "reference.png"), "-resize", "512x512>", "-depth", "8", "rgb:-")
        actual = run("convert", str(directory / "actual.png"), "-depth", "8", "rgb:-")
        if len(reference) != 512 * 256 * 3 or len(actual) != len(reference):
            raise RuntimeError("HEIF renderer self-check failed: unexpected thumbnail dimensions")
        error = sum(abs(a - b) for a, b in zip(actual, reference)) / len(reference)
        if error > 12:
            raise RuntimeError(f"HEIF renderer self-check failed: 10-bit RGB mean error {error:.2f} exceeds 12; rebuild the worker image")
        LOG.info("HEIF renderer self-check passed: 10-bit RGB mean error %.2f", error)


class Worker:
    def __init__(self):
        self.base = os.environ["KEEPS_WORKER_URL"].rstrip("/")
        self.library = urllib.parse.quote(os.environ["KEEPS_LIBRARY_ID"], safe="")
        token_file = os.environ.get("KEEPS_TOKEN_FILE")
        self.token = Path(token_file).read_text().strip() if token_file else os.environ["KEEPS_API_TOKEN"]
        self.root = Path(os.environ.get("KEEPS_WORKER_TMP", "/work"))
        self.root.mkdir(parents=True, exist_ok=True)
        self.concurrency = int(os.environ.get("KEEPS_WORKER_CONCURRENCY", "4"))
        self.budget = int(os.environ.get("KEEPS_WORKER_MAX_BYTES", str(32 * 1024**3)))
        self.reserve = int(os.environ.get("KEEPS_WORKER_TASK_BYTES", str(8 * 1024**3)))
        if not 1 <= self.concurrency <= 32 or self.reserve <= 0 or self.budget < self.concurrency * self.reserve:
            raise ValueError("invalid concurrency or disk budget")
        self.opener = urllib.request.build_opener(NoRedirect())
        # Refuse expansion over crash leftovers; never delete arbitrary scratch contents.
        if tree_bytes(self.root) + self.concurrency * self.reserve > self.budget:
            raise RuntimeError("scratch contains crash leftovers exceeding disk budget; inspect while worker is stopped")

    def request(self, method, path, body=None, raw=False, extra_headers=None):
        headers = {"Authorization": "Bearer " + self.token}
        headers.update(extra_headers or {})
        if body is not None:
            headers["Content-Type"] = "image/heic" if raw else "application/json"
            if not raw:
                body = json.dumps(body).encode()
        request = urllib.request.Request(self.base + "/libraries/" + self.library + "/worker/" + path, data=body, headers=headers, method=method)
        return self.opener.open(request, timeout=1800 if path == "claim" or path.endswith("/complete") else 120)

    def json_request(self, method, path, body):
        for attempt in range(3):
            try:
                with self.request(method, path, body) as response:
                    return json.load(response)
            except urllib.error.HTTPError as error:
                if error.code < 500 or attempt == 2:
                    raise
            except (OSError, TimeoutError):
                if attempt == 2:
                    raise
            STOP.wait(2 ** attempt)

    def run_one(self, slot):
        worker_id = os.environ.get("KEEPS_WORKER_ID", "linux") + "-" + str(slot)
        while not STOP.is_set():
            if shutil.disk_usage(self.root).free < self.budget + 2 * 1024**3:
                LOG.warning("Waiting for local free space")
                STOP.wait(60)
                continue
            try:
                # Claim is intentionally not retried: a lost response may already own a lease.
                with self.request("POST", "claim", {"workerID": worker_id}) as response:
                    task = json.load(response).get("task")
                if task is None:
                    STOP.wait(15)
                    continue
                self.process(task)
            except Exception:
                LOG.exception("Worker slot %s failed", slot)
                STOP.wait(15)

    @contextlib.contextmanager
    def video_source(self, prefix):
        worker = self
        transfer = {"bytes": 0}
        class Source(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path != "/source":
                    self.send_error(404)
                    return
                try:
                    headers = {"Range": self.headers["Range"]} if "Range" in self.headers else {}
                    with worker.request("GET", prefix + "/source", extra_headers=headers) as response:
                        self.send_response(response.status)
                        for name in ("Content-Length", "Content-Range", "Accept-Ranges"):
                            if response.headers.get(name):
                                self.send_header(name, response.headers[name])
                        self.end_headers()
                        while chunk := response.read(256 * 1024):
                            self.wfile.write(chunk)
                            transfer["bytes"] += len(chunk)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception:
                    LOG.exception("Video source request failed")
                    self.send_error(502)
            def log_message(self, *_):
                pass
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Source)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield "http://127.0.0.1:" + str(server.server_port) + "/source", transfer
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def render(self, command, env, directory, lease_lost):
        process = subprocess.Popen(command, env=env, start_new_session=True)
        deadline = time.monotonic() + 1500
        try:
            while process.poll() is None:
                if lease_lost.is_set() or tree_bytes(directory) > self.reserve or time.monotonic() > deadline:
                    raise RuntimeError("render stopped: lease, disk budget or deadline")
                time.sleep(1)
            if process.returncode:
                raise RuntimeError("renderer exit " + str(process.returncode))
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()

    def process(self, task):
        started = time.monotonic()
        task_id = task["taskID"]
        prefix = "tasks/" + urllib.parse.quote(task_id, safe="")
        done = threading.Event()
        lease_lost = threading.Event()

        def renew():
            interval = min(60, max(5, int(task["leaseSeconds"]) // 3))
            while not done.wait(interval):
                try:
                    self.json_request("POST", prefix + "/heartbeat", {})
                except Exception:
                    LOG.exception("Lease renewal failed for %s", task_id)
                    lease_lost.set()
                    return

        heartbeat = threading.Thread(target=renew, daemon=True)
        heartbeat.start()
        try:
            with tempfile.TemporaryDirectory(prefix="task-", dir=self.root) as temp:
                directory = Path(temp)
                source = directory / ("source" + Path(task["filename"]).suffix.lower())
                count = 0
                env = dict(os.environ, TMPDIR=temp)
                if task.get("mediaType") == "video":
                    source = directory / "first-frame.png"
                    # NAS validates the full source hash at claim and publication.
                    # The loopback proxy keeps credentials out of ffmpeg and refuses redirects.
                    with self.video_source(prefix) as (url, transfer):
                        self.render(["ffmpeg", "-nostdin", "-v", "error", "-threads", "1", "-i", url,
                                     "-vf", f"scale='min(iw,{task['thumbnailEdge']})':'min(ih,{task['thumbnailEdge']})':force_original_aspect_ratio=decrease,thumbnail=60", "-frames:v", "1", "-threads", "1", "-filter_threads", "1", str(source)],
                                    env, directory, lease_lost)
                    count = transfer["bytes"]
                else:
                    if task.get("sizeBytes", 0) > self.reserve // 2:
                        raise RuntimeError("source exceeds per-task disk allowance")
                    digest = hashlib.sha256()
                    with self.request("GET", prefix + "/source") as response, source.open("xb") as output:
                        while chunk := response.read(1024 * 1024):
                            count += len(chunk)
                            if count > self.reserve // 2:
                                raise RuntimeError("source exceeds per-task disk allowance")
                            digest.update(chunk)
                            output.write(chunk)
                    if digest.hexdigest() != task["inputHash"]:
                        raise RuntimeError("download SHA256 mismatch")
                command = ["keeps-render", str(source), temp, str(bool(task["generateStandard"])).lower(), str(task["thumbnailEdge"]), str(task["thumbnailQuality"])]
                self.render(command, env, directory, lease_lost)
                for role in (["standard", "thumbnail"] if task["generateStandard"] else ["thumbnail"]):
                    if lease_lost.is_set():
                        raise RuntimeError("lease lost before upload")
                    path = directory / (role + ".heic")
                    if path.stat().st_size > 256 * 1024**2:
                        raise RuntimeError("result exceeds upload limit")
                    for attempt in range(3):
                        try:
                            with path.open("rb") as data:
                                # urllib streams file bodies; explicit size avoids chunked uploads.
                                request = urllib.request.Request(self.base + "/libraries/" + self.library + "/worker/" + prefix + "/" + role, data=data, method="PUT", headers={"Authorization": "Bearer " + self.token, "Content-Type": "image/heic", "Content-Length": str(path.stat().st_size)})
                                with self.opener.open(request, timeout=120) as response:
                                    response.read()
                            break
                        except urllib.error.HTTPError as error:
                            if error.code < 500 or attempt == 2:
                                raise
                        except OSError:
                            if attempt == 2:
                                raise
                        time.sleep(2 ** attempt)
                self.json_request("POST", prefix + "/complete", {})
                output_bytes = sum((directory / (role + ".heic")).stat().st_size for role in (["standard", "thumbnail"] if task["generateStandard"] else ["thumbnail"]))
                LOG.info("Completed task %s elapsedSeconds=%.2f sourceBytes=%s outputBytes=%s", task_id, time.monotonic() - started, count, output_bytes)
        except Exception:
            LOG.exception("Task %s failed", task_id)
            try:
                self.json_request("POST", prefix + "/fail", {"error": "Linux worker failed; see worker logs for full traceback"})
            except Exception:
                LOG.exception("Failure report failed for %s", task_id)
            raise
        finally:
            done.set()
            heartbeat.join(timeout=125)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise RuntimeError("worker API redirects are not permitted")


def main(argv=None):
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-renderer-only", action="store_true", help="validate the installed renderer without contacting NAS")
    args = parser.parse_args(argv)
    if args.check_renderer_only:
        root = Path(os.environ.get("KEEPS_WORKER_TMP", "/work"))
        root.mkdir(parents=True, exist_ok=True)
        verify_renderer(root)
        return
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda *_: STOP.set())
    worker = Worker()
    verify_renderer(worker.root)
    with concurrent.futures.ThreadPoolExecutor(max_workers=worker.concurrency) as pool:
        list(pool.map(worker.run_one, range(worker.concurrency)))


if __name__ == "__main__":
    main()
