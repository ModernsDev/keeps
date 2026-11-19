import hashlib
import importlib.util
import io
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("worker", Path(__file__).with_name("linux_cache_worker.py"))
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class WorkerTests(unittest.TestCase):
    def setUp(self):
        worker.STOP.clear()

    def test_renderer_check_accepts_lossy_roundtrip_and_rejects_stripes(self):
        reference = bytes([180, 30, 70]) * (512 * 256)
        for actual, succeeds in [(bytes([178, 32, 72]) * (512 * 256), True),
                                 (bytes([0, 255, 0]) * (512 * 256), False), (b"", False)]:
            with self.subTest(succeeds=succeeds, size=len(actual)), tempfile.TemporaryDirectory() as directory:
                pixels = iter([reference, actual])
                def run(command, **kwargs):
                    if command[0] == "exiftool":
                        return SimpleNamespace(stdout=b'[{"BitDepthLuma":10,"BitDepthChroma":10}]')
                    return SimpleNamespace(stdout=next(pixels) if command[-1] == "rgb:-" else b"")
                with patch.object(worker.subprocess, "run", side_effect=run):
                    if succeeds:
                        worker.verify_renderer(Path(directory))
                    else:
                        with self.assertRaisesRegex(RuntimeError, "self-check failed"):
                            worker.verify_renderer(Path(directory))
                self.assertEqual(list(Path(directory).iterdir()), [])

    def test_renderer_check_rejects_eight_bit_fixture(self):
        with tempfile.TemporaryDirectory() as directory:
            def run(command, **kwargs):
                return SimpleNamespace(stdout=b'[{"BitDepthLuma":8,"BitDepthChroma":8}]' if command[0] == "exiftool" else b"")
            with patch.object(worker.subprocess, "run", side_effect=run):
                with self.assertRaisesRegex(RuntimeError, "fixture is not 10-bit"):
                    worker.verify_renderer(Path(directory))

    def test_renderer_failure_prevents_claiming_tasks(self):
        with patch.object(worker, "Worker") as factory, \
             patch.object(worker, "verify_renderer", side_effect=RuntimeError("bad renderer")), \
             patch.object(worker.concurrent.futures, "ThreadPoolExecutor") as pool:
            with self.assertRaisesRegex(RuntimeError, "bad renderer"):
                worker.main([])
            pool.assert_not_called()
            factory.return_value.run_one.assert_not_called()

    def test_renderer_check_only_needs_no_nas_credentials(self):
        with tempfile.TemporaryDirectory() as directory, \
             patch.dict(worker.os.environ, {"KEEPS_WORKER_TMP": directory}, clear=True), \
             patch.object(worker, "verify_renderer") as verify, \
             patch.object(worker, "Worker") as factory:
            worker.main(["--check-renderer-only"])
            verify.assert_called_once_with(Path(directory))
            factory.assert_not_called()

    def test_renderer_codec_failure_is_not_ignored(self):
        error = worker.subprocess.CalledProcessError(1, ["heif-enc"])
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(worker.subprocess, "run", side_effect=error):
                with self.assertRaises(worker.subprocess.CalledProcessError):
                    worker.verify_renderer(Path(directory))
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_rejects_corrupt_download_and_cleans_scratch(self):
        with tempfile.TemporaryDirectory() as directory:
            instance = worker.Worker.__new__(worker.Worker)
            instance.root = Path(directory)
            instance.reserve = 1024
            instance.request = lambda *args, **kwargs: io.BytesIO(b"wrong bytes")
            calls = []
            instance.json_request = lambda *args: calls.append(args)
            with self.assertRaisesRegex(RuntimeError, "SHA256 mismatch"):
                instance.process({"taskID": "test", "leaseSeconds": 1800, "filename": "photo.raw", "inputHash": hashlib.sha256(b"correct").hexdigest()})
            self.assertEqual(list(instance.root.iterdir()), [])
            self.assertEqual(calls[0][1], "tasks/test/fail")

    def test_refuses_oversize_input_before_render(self):
        with tempfile.TemporaryDirectory() as directory:
            instance = worker.Worker.__new__(worker.Worker)
            instance.root = Path(directory)
            instance.reserve = 8
            instance.request = lambda *args, **kwargs: io.BytesIO(b"12345")
            instance.json_request = lambda *args: {}
            with self.assertRaisesRegex(RuntimeError, "disk allowance"):
                instance.process({"taskID": "test", "leaseSeconds": 1800, "filename": "photo.raw"})
            self.assertEqual(list(instance.root.iterdir()), [])

    def test_video_proxy_forwards_ranges_without_exposing_auth(self):
        instance = worker.Worker.__new__(worker.Worker)
        class Response(io.BytesIO):
            status = 206
            headers = {"Content-Length": "3", "Content-Range": "bytes 7-9/10", "Accept-Ranges": "bytes"}
        calls = []
        def request(*args, **kwargs):
            calls.append((args, kwargs))
            return Response(b"789")
        instance.request = request
        with instance.video_source("tasks/video") as (url, transfer):
            with worker.urllib.request.urlopen(worker.urllib.request.Request(url, headers={"Range": "bytes=7-9"})) as response:
                self.assertEqual(response.status, 206)
                self.assertEqual(response.headers["Content-Range"], "bytes 7-9/10")
                self.assertEqual(response.read(), b"789")
        self.assertEqual(transfer["bytes"], 3)
        self.assertEqual(calls, [(("GET", "tasks/video/source"), {"extra_headers": {"Range": "bytes=7-9"}})])

    def test_no_redirect_with_credentials(self):
        with self.assertRaisesRegex(RuntimeError, "redirects"):
            worker.NoRedirect().redirect_request(None, None, 302, None, {}, "https://other.example")

    def test_lease_conflict_is_not_retried(self):
        instance = worker.Worker.__new__(worker.Worker)
        error = worker.urllib.error.HTTPError("https://nas/task", 409, "expired", {}, None)
        with patch.object(instance, "request", side_effect=error) as request:
            with self.assertRaises(worker.urllib.error.HTTPError):
                instance.json_request("POST", "tasks/test/complete", {})
            self.assertEqual(request.call_count, 1)


if __name__ == "__main__":
    unittest.main()
