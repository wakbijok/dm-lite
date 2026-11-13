#!/usr/bin/env python3
"""Tests for contrib/sign-release.py: the parts that need neither GitHub nor the release key.

    python3 contrib/test_sign_release.py

What they show: the trusted comment is the archive name, a tab and the version, and is the
one scripts/package-release.sh prints; the signer passes it to rsign; and --packaged accepts
the output directory of scripts/package-release.sh only when it is for the tag's commit and
the archives are unchanged since the script listed them.
"""

import hashlib
import importlib.util
import os
import re
import stat
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("sign_release", os.path.join(HERE, "sign-release.py"))
sr = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sr)

NAME = "dmem-v0.3.9-x86_64-unknown-linux-gnu.tar.gz"
COMMIT = "0123456789abcdef0123456789abcdef01234567"


def sha(data):
    return hashlib.sha256(data).hexdigest()


class TrustedComment(unittest.TestCase):
    def test_is_the_file_a_tab_and_the_version(self):
        self.assertEqual(sr.trusted_comment(NAME, "v0.3.9"), "file:" + NAME + "\tversion:0.3.9")

    def test_a_release_candidate_keeps_its_suffix(self):
        self.assertEqual(sr.trusted_comment("dmem-v0.4.0-rc.1-x.tar.gz", "v0.4.0-rc.1").split("\t")[1], "version:0.4.0-rc.1")

    def test_is_not_the_space_form_the_hub_tooling_writes(self):
        self.assertNotIn(" ", sr.trusted_comment(NAME, "v0.3.9"))

    def test_is_the_comment_the_packaging_script_prints(self):
        with open(os.path.join(HERE, "..", "scripts", "package-release.sh")) as fh:
            m = re.search(r"^COMMENT_FORMAT='([^']*)'$", fh.read(), re.M)
        self.assertIsNotNone(m, "COMMENT_FORMAT is not in scripts/package-release.sh")
        # the script's format is a printf format: %s twice, and a backslash-t for the tab
        from_script = m.group(1).replace("\\t", "\t") % (NAME, "0.3.9")
        self.assertEqual(from_script, sr.trusted_comment(NAME, "v0.3.9"))


class SignPassesTheComment(unittest.TestCase):
    def test_rsign_gets_the_trusted_comment_and_the_archive_name(self):
        with tempfile.TemporaryDirectory() as work:
            fake = os.path.join(work, "fake-rsign")
            with open(fake, "w") as fh:
                # asks for the password once, as rsign does, then records its arguments
                fh.write("#!/bin/sh\nprintf 'Password:'\nread -r pw\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > argv.bin\nexit 0\n")
            os.chmod(fake, os.stat(fake).st_mode | stat.S_IXUSR)
            comment = sr.trusted_comment(NAME, "v0.3.9")
            sr.sign(fake, "the.key", work, NAME, "not-a-real-secret", comment)
            with open(os.path.join(work, "argv.bin"), "rb") as fh:
                argv = fh.read().split(b"\0")[:-1]
        self.assertEqual(
            [a.decode() for a in argv],
            ["sign", "-s", "the.key", "-x", NAME + ".minisig", "-t", comment, NAME],
        )


class VerifyOutput(unittest.TestCase):
    comment = sr.trusted_comment(NAME, "v0.3.9")

    def test_reads_the_comment_from_stderr_as_rsign2_0_6_prints_it(self):
        err = "Signature and comment signature verified\nTrusted comment: " + self.comment + "\n"
        self.assertTrue(sr.carries_comment("", err, self.comment))

    def test_reads_the_comment_from_stdout_as_older_rsign_prints_it(self):
        self.assertTrue(sr.carries_comment("Trusted comment: " + self.comment + "\n", "", self.comment))

    def test_refuses_another_file_or_a_longer_comment(self):
        other = sr.trusted_comment(MAC, "v0.3.9")
        self.assertFalse(sr.carries_comment("", "Trusted comment: " + other + "\n", self.comment))
        self.assertFalse(sr.carries_comment("", "Trusted comment: " + self.comment + "\textra\n", self.comment))

    def test_rsign_on_this_host_agrees(self):
        import shutil, subprocess
        rsign = shutil.which("rsign")
        if not rsign:
            self.skipTest("no rsign")
        with tempfile.TemporaryDirectory() as d:
            subprocess.run([rsign, "generate", "-W", "-f", "-p", "t.pub", "-s", "t.key"], cwd=d,
                           capture_output=True, check=True)
            with open(os.path.join(d, NAME), "wb") as fh:
                fh.write(b"archive bytes")
            subprocess.run([rsign, "sign", "-W", "-s", "t.key", "-x", NAME + ".minisig", "-t", self.comment, NAME],
                           cwd=d, capture_output=True, check=True)
            v = subprocess.run([rsign, "verify", "-p", "t.pub", "-x", NAME + ".minisig", NAME],
                               cwd=d, capture_output=True, text=True, check=True)
        self.assertTrue(sr.carries_comment(v.stdout, v.stderr, self.comment), v.stdout + v.stderr)


class PackagedBuild(unittest.TestCase):
    def make(self, d, *, commit=COMMIT, archive=b"archive bytes", listed=None, with_commit=True, with_sums=True):
        path = os.path.join(d, NAME)
        with open(path, "wb") as fh:
            fh.write(archive)
        if with_commit:
            with open(os.path.join(d, "COMMIT"), "w") as fh:
                fh.write(commit + "\n")
        if with_sums:
            with open(os.path.join(d, "SHA256SUMS"), "w") as fh:
                fh.write((listed or sha(archive)) + "  " + NAME + "\n")

    def test_accepts_what_the_script_wrote(self):
        with tempfile.TemporaryDirectory() as d:
            self.make(d)
            label, built = sr.packaged_build(d, COMMIT)
        self.assertEqual(built, {NAME: sha(b"archive bytes")})
        self.assertIn("package-release.sh", label)

    def test_refuses_another_commit(self):
        with tempfile.TemporaryDirectory() as d:
            self.make(d, commit="f" * 40)
            with self.assertRaises(SystemExit) as cm:
                sr.packaged_build(d, COMMIT)
        self.assertIn("not at", str(cm.exception))

    def test_refuses_an_archive_changed_since_it_was_listed(self):
        with tempfile.TemporaryDirectory() as d:
            self.make(d, listed=sha(b"what was packaged"))
            with self.assertRaises(SystemExit) as cm:
                sr.packaged_build(d, COMMIT)
        self.assertIn("changed since it was packaged", str(cm.exception))

    def test_refuses_an_archive_that_sha256sums_does_not_list(self):
        with tempfile.TemporaryDirectory() as d:
            self.make(d)
            with open(os.path.join(d, "SHA256SUMS"), "w") as fh:
                fh.write(sha(b"x") + "  some-other-file.tar.gz\n")
            with self.assertRaises(SystemExit):
                sr.packaged_build(d, COMMIT)

    def test_refuses_a_directory_without_commit_or_sums(self):
        for kw in ({"with_commit": False}, {"with_sums": False}):
            with tempfile.TemporaryDirectory() as d:
                self.make(d, **kw)
                with self.assertRaises(SystemExit) as cm:
                    sr.packaged_build(d, COMMIT)
                self.assertIn("is missing", str(cm.exception))

    def test_refuses_a_directory_with_no_archive(self):
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "COMMIT"), "w") as fh:
                fh.write(COMMIT + "\n")
            with open(os.path.join(d, "SHA256SUMS"), "w") as fh:
                fh.write("")
            with self.assertRaises(SystemExit) as cm:
                sr.packaged_build(d, COMMIT)
        self.assertIn("no dmem-v* archive", str(cm.exception))


MAC = "dmem-v0.3.9-aarch64-apple-darwin.tar.gz"
WIN = "dmem-v0.3.9-x86_64-pc-windows-msvc.zip"


def row(name, data):
    return (name, len(data), sha(data), "someone")


class CheckArchives(unittest.TestCase):
    # A release since 0.4.x: the hand-built x86_64 linux archive (packaged lane) beside the
    # workflow's other targets. The workflow also built its own linux archive, which differs.
    rows = [row(NAME, b"hand linux"), row(MAC, b"mac"), row(WIN, b"win")]
    packaged = {NAME: sha(b"hand linux")}
    workflow = {NAME: sha(b"workflow linux"), MAC: sha(b"mac"), WIN: sha(b"win")}

    def test_a_mixed_release_is_checked_lane_by_lane(self):
        got = sr.check_archives(self.rows, self.packaged, self.workflow)
        self.assertEqual(got, [(NAME, "packaged build"), (MAC, "workflow build"), (WIN, "workflow build")])

    def test_the_packaged_lane_wins_for_its_archive_and_must_match_it(self):
        rows = [row(NAME, b"workflow linux")]  # the workflow's file, where the hand build should be
        self.assertEqual(sr.check_archives(rows, self.packaged, self.workflow), [(NAME, None)])

    def test_an_archive_swapped_on_the_release_is_refused(self):
        rows = [row(NAME, b"hand linux"), row(MAC, b"tampered mac")]
        self.assertEqual(sr.check_archives(rows, self.packaged, self.workflow)[1], (MAC, None))

    def test_an_archive_with_no_trusted_build_is_refused(self):
        self.assertEqual(sr.check_archives([row(MAC, b"mac")], self.packaged, None), [(MAC, None)])
        self.assertEqual(sr.check_archives([row(MAC, b"mac")], None, {}), [(MAC, None)])

    def test_workflow_only_as_before(self):
        rows = [row(MAC, b"mac"), row(WIN, b"win")]
        self.assertEqual([lane for _, lane in sr.check_archives(rows, None, self.workflow)], ["workflow build"] * 2)


class SumsText(unittest.TestCase):
    def test_lists_every_archive_sorted_in_sha256sum_format(self):
        rows = [row(WIN, b"win"), row(NAME, b"hand linux"), row(MAC, b"mac")]
        text = sr.sums_text(rows)
        names = [line.split("  ", 1)[1] for line in text.splitlines()]
        self.assertEqual(names, sorted([NAME, MAC, WIN]))
        self.assertIn(f"{sha(b'mac')}  {MAC}\n", text)
        for line in text.splitlines():
            self.assertRegex(line, r"^[0-9a-f]{64}  dmem-v\S+$")

    def test_sha256sum_checks_it(self):
        import subprocess
        with tempfile.TemporaryDirectory() as d:
            rows = []
            for name, data in [(NAME, b"hand linux"), (MAC, b"mac")]:
                with open(os.path.join(d, name), "wb") as fh:
                    fh.write(data)
                rows.append(row(name, data))
            with open(os.path.join(d, sr.SUMS), "w") as fh:
                fh.write(sr.sums_text(rows))
            r = subprocess.run(["sha256sum", "-c", sr.SUMS], cwd=d, capture_output=True, text=True)
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_its_signature_is_bound_to_the_file_name(self):
        self.assertEqual(sr.trusted_comment(sr.SUMS, "v0.3.9"), "file:SHA256SUMS\tversion:0.3.9")


if __name__ == "__main__":
    unittest.main()
