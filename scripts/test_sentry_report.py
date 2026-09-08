import json
import unittest
from unittest import mock

import sentry_report as sentry


def report():
    return {"schema": 2, "app": "fileman", "version": "0.4.0", "revision": "abcdef",
            "os": "linux", "arch": "x86_64", "payload": {
                "type": "failure", "kind": "panic", "site": None, "trace": {
                    "debug_id": "12345678-abcd-0000-0000-0123456789ab", "code_id": "abcd",
                    "image_size": 65536, "image_vmaddr": 0, "offsets": [2048, 1024]}}}


class ReportTests(unittest.TestCase):
    def test_relative_frames_and_binary_identity_are_preserved(self):
        event = sentry.event_from_report(report(), "fileman")
        stack = event["exception"]["values"][0]["stacktrace"]
        self.assertEqual(stack["instruction_addr_adjustment"], "all")
        frames = stack["frames"]
        self.assertEqual(frames[0]["instruction_addr"], "0x800")
        self.assertEqual(frames[1]["addr_mode"], "rel:0")
        self.assertEqual(event["debug_meta"]["images"][0]["debug_id"], report()["payload"]["trace"]["debug_id"])
        self.assertNotIn("user", event)
        self.assertNotIn("timestamp", event)

    def test_unknown_fields_at_every_level_are_rejected(self):
        for location in [(), ("payload",), ("payload", "trace")]:
            value = report()
            node = value
            for key in location:
                node = node[key]
            node["CANARY"] = "/secret/host"
            with self.assertRaises(ValueError):
                sentry.event_from_report(value, "fileman")

    def test_bad_values_wrong_apps_and_usage_are_rejected(self):
        for key, value in [("schema", True), ("schema", 1), ("app", "starcom"),
                           ("version", "/home/CANARY"), ("revision", "x\n"), ("os", "private-os")]:
            sample = report()
            sample[key] = value
            with self.assertRaises(ValueError):
                sentry.event_from_report(sample, "fileman")
        sample = report()
        sample["payload"] = {"type": "usage", "counters": {}}
        with self.assertRaises(ValueError):
            sentry.event_from_report(sample, "fileman")

    def test_offsets_are_bounded_and_not_booleans(self):
        for offsets in [[], [True], [-1], [65536], [0] * 49, "CANARY"]:
            sample = report()
            sample["payload"]["trace"]["offsets"] = offsets
            with self.assertRaises(ValueError):
                sentry.event_from_report(sample, "fileman")

    def test_source_and_image_fields_cannot_contain_runtime_paths(self):
        for location in ["/home/CANARY/src/main.rs", "src/../CANARY.rs"]:
            sample = report()
            sample["payload"]["site"] = {"file": location, "line": 1, "column": 1}
            with self.assertRaises(ValueError):
                sentry.event_from_report(sample, "fileman")
        for field in ["debug_id", "code_id"]:
            sample = report()
            sample["payload"]["trace"][field] = "/home/CANARY"
            with self.assertRaises(ValueError):
                sentry.event_from_report(sample, "fileman")

    def test_only_hosted_https_ingestion_is_accepted(self):
        key = "a" * 32
        for host in ["o1.ingest.sentry.io", "o1.ingest.us.sentry.io", "o1.ingest.de.sentry.io"]:
            endpoint, public = sentry.destination(f"https://{key}@{host}/12")
            self.assertEqual(endpoint, f"https://{host}/api/12/envelope/")
            self.assertEqual(public, key)
        for dsn in [f"http://{key}@o1.ingest.sentry.io/1", f"https://{key}@localhost/1",
                    f"https://{key}@o1.ingest.sentry.io.evil.test/1",
                    f"https://{key}@o1.ingest.sentry.io:123/1",
                    f"https://{key}:CANARY@o1.ingest.sentry.io/1",
                    f"https://{key}@o1.ingest.sentry.io/1?recipient=evil"]:
            with self.assertRaises(ValueError):
                sentry.destination(dsn)

    def test_conversion_does_no_network_io(self):
        with mock.patch.object(sentry.urllib.request, "build_opener") as opener:
            sentry.event_from_report(report(), "fileman")
            opener.assert_not_called()

    def test_invalid_dsn_does_no_network_io(self):
        with mock.patch.object(sentry.urllib.request, "build_opener") as opener:
            with self.assertRaises(ValueError):
                sentry._send_event(sentry.event_from_report(report(), "fileman"), "")
            opener.assert_not_called()

    def test_private_email_does_not_authorize_third_party_upload(self):
        for consent in [None, False, 1, "true"]:
            sample = report()
            if consent is not None:
                sample["sentry_consent"] = consent
            with mock.patch.object(sentry.urllib.request, "build_opener") as opener:
                with self.assertRaises(ValueError):
                    sentry.send_report(sample, "fileman", f"https://{'a'*32}@o1.ingest.sentry.io/1")
                opener.assert_not_called()

    def test_redirects_never_forward_the_event(self):
        with self.assertRaises(ValueError):
            sentry.NoRedirects().redirect_request(None, None, 307, "", {}, "https://evil.test")

    def test_sends_one_valid_envelope_and_no_auth_token(self):
        event = sentry.event_from_report(report(), "fileman")
        with mock.patch.object(sentry.urllib.request, "build_opener") as builder:
            response = builder.return_value.open.return_value.__enter__.return_value
            response.status = 200
            returned = sentry._send_event(event, f"https://{'a'*32}@o1.ingest.sentry.io/1")
            self.assertEqual(returned, event["event_id"])
            args, kwargs = builder.return_value.open.call_args
            header, item, body = args[0].data.splitlines()
            self.assertEqual(json.loads(item)["length"], len(body))
            self.assertEqual(json.loads(header)["event_id"], event["event_id"])
            self.assertEqual(kwargs["timeout"], 15)
            self.assertNotIn("Bearer", str(args[0].headers))
            builder.return_value.open.assert_called_once()


if __name__ == "__main__":
    unittest.main()
