#!/usr/bin/env python3
"""Sign a dm-lite GitHub release: the release manager's step, run by hand.

    contrib/sign-release.py v0.3.6            # check, sign, verify, upload
    contrib/sign-release.py v0.3.6 --no-upload

Run it from a dm-lite clone on the machine that holds the release key. It:
  1. refuses to go on unless ALL of these hold:
     - the GitHub tag's code is identical to the upstream (GitLab) tag of the same
       name in this clone;
     - a successful run of the release workflow built that exact commit, and every
       archive on the release is byte-identical to that run's build artifact (so a
       file uploaded or swapped by hand, or built from another commit, is caught);
     - that tag's src/upgrade.rs trusts the key you are signing with;
  2. shows each archive with its sha256, asks you to confirm, then asks for the key
     passphrase ONCE;
  3. signs every archive with rsign, verifies each signature with the public key,
     and uploads the .minisig files to the release.

The passphrase is typed into rsign through a private pseudo-terminal with echo off.
It is never written to disk, argv, or a child's environment, and it is forgotten
when the script exits. This is deliberately a human step: the signature is what
stops a hijacked GitHub account from shipping code to `dmem upgrade` users, so no
automated service should be able to produce one on request.

Needs: gh (logged in), rsign (rsign2), git.
"""

import argparse
import base64
import getpass
import hashlib
import json
import os
import pty
import re
import select
import shutil
import subprocess
import sys
import tempfile
import termios
import time

TAG_RE = re.compile(r"v\d{1,4}\.\d{1,4}\.\d{1,4}(-rc\.\d{1,4})?")
KEY_DIR = os.path.expanduser("~/.config/dmem-release")
SIGN_TIMEOUT = 120  # scrypt key derivation takes a few seconds per archive
CHILD_ENV = {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C"}


def die(msg):
    sys.exit(f"sign-release: {msg}")


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw).stdout


def gh_api(path):
    return json.loads(run(["gh", "api", path]))


def pubkey_line(pub):
    with open(pub) as fh:
        lines = [l.strip() for l in fh if l.strip()]
    key_id = lines[0].rsplit(" ", 1)[-1] if lines[0].startswith("untrusted comment") else "?"
    return key_id, lines[-1]


def sign(rsign, key, work, name, secret):
    """Run rsign on a private pty and answer its single password prompt."""
    pid, fd = pty.fork()
    if pid == 0:
        try:
            # Relative name, so the trusted comment reads file:<archive>, not a temp path.
            os.chdir(work)
            os.execve(rsign, [rsign, "sign", "-s", key, "-x", name + ".minisig", name], CHILD_ENV)
        finally:
            os._exit(127)  # never fall through into the parent's code
    # Echo off before anything is typed: rsign disables echo only after printing its
    # prompt, so an early answer would otherwise be echoed back into our buffer.
    attrs = termios.tcgetattr(fd)
    attrs[3] &= ~termios.ECHO
    termios.tcsetattr(fd, termios.TCSANOW, attrs)
    out, prompts, deadline = b"", 0, time.monotonic() + SIGN_TIMEOUT
    try:
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError("rsign timed out")
            ready, _, _ = select.select([fd], [], [], left)
            if not ready:
                continue
            try:
                chunk = os.read(fd, 4096)
            except OSError:  # EIO: child closed the pty
                break
            if not chunk:
                break
            out += chunk
            if out.count(b"Password:") > prompts:
                prompts += 1
                if prompts > 1:  # a second prompt means the first answer was wrong
                    raise PermissionError("wrong passphrase for the release key")
                os.write(fd, secret.encode() + b"\n")
    except BaseException:
        os.kill(pid, 9)
        os.waitpid(pid, 0)
        raise
    finally:
        os.close(fd)
    _, status = os.waitpid(pid, 0)
    text = out.decode(errors="replace").replace(secret, "***")
    if "Wrong password" in text:
        raise PermissionError("wrong passphrase for the release key")
    if prompts != 1 or os.waitstatus_to_exitcode(status) != 0:
        raise RuntimeError(f"rsign sign failed: {text.strip()[-300:]}")


def tree_check(tag, remote, commit):
    """(ok, message): does the GitHub tag's commit carry the upstream tag's exact tree?"""
    ref = f"refs/remotes/{remote}/tags/{tag}"
    try:
        run(["git", "fetch", "-q", "--no-tags", remote, f"+refs/tags/{tag}:{ref}"])
        fetched = run(["git", "rev-parse", f"{ref}^{{commit}}"]).strip()
        gh_tree = run(["git", "rev-parse", f"{ref}^{{tree}}"]).strip()
    except subprocess.CalledProcessError:
        return False, "could not fetch the GitHub tag into this clone"
    if fetched != commit:
        return False, f"the GitHub tag moved while checking ({fetched[:10]} vs {commit[:10]})"
    try:
        up_tree = run(["git", "rev-parse", f"refs/tags/{tag}^{{tree}}"]).strip()
    except subprocess.CalledProcessError:
        return False, f"no upstream tag {tag} in this clone (fetch it from GitLab first)"
    if gh_tree != up_tree:
        return False, f"DIFFERENT code from the upstream tag ({gh_tree[:10]} vs {up_tree[:10]})"
    return True, "same code as the upstream tag"


def workflow_build(repo, tag, commit, work):
    """(run id, {file: sha256}) of the release workflow's build of this exact commit."""
    runs = gh_api(f"repos/{repo}/actions/workflows/release.yml/runs?event=push&head_sha={commit}&per_page=20")
    runs = [r for r in runs.get("workflow_runs", [])
            if r.get("head_branch") == tag and r.get("conclusion") == "success"
            and r.get("path", "").split("@")[0] == ".github/workflows/release.yml"]
    if not runs:
        die(f"no successful release workflow run built {tag} at {commit[:10]}")
    run_id = runs[0]["id"]
    dest = os.path.join(work, "artifacts")
    run(["gh", "run", "download", str(run_id), "-R", repo, "-D", dest])
    digests = {}
    for root, _, files in os.walk(dest):
        for f in files:
            with open(os.path.join(root, f), "rb") as fh:
                digests[f] = hashlib.sha256(fh.read()).hexdigest()
    return run_id, digests


def main():
    ap = argparse.ArgumentParser(description="Sign a dm-lite GitHub release (human step).")
    ap.add_argument("tag")
    ap.add_argument("--repo", default="wakbijok/dm-lite")
    ap.add_argument("--remote", default="github", help="git remote for the GitHub repo")
    ap.add_argument("--key", default=os.path.join(KEY_DIR, "dm-lite-release-2.key"))
    ap.add_argument("--pub", default=os.path.join(KEY_DIR, "dm-lite-release-2.pub"))
    ap.add_argument("--no-upload", action="store_true", help="sign and verify only; keep the files")
    ap.add_argument("--clobber", action="store_true", help="replace .minisig files already on the release")
    a = ap.parse_args()

    if not TAG_RE.fullmatch(a.tag):
        die("tag must look like v0.3.6")
    rsign = shutil.which("rsign") or os.path.expanduser("~/.cargo/bin/rsign")
    for f in (a.key, a.pub, rsign):
        if not os.path.isfile(f):
            die(f"missing {f}")
    key_id, pub_b64 = pubkey_line(a.pub)

    rel = gh_api(f"repos/{a.repo}/releases/tags/{a.tag}")
    if rel.get("draft"):
        die(f"{a.tag} is a draft release")
    name_re = re.compile(rf"dmem-{re.escape(a.tag)}-[A-Za-z0-9_.-]+\.(tar\.gz|zip)")
    assets = [x for x in rel.get("assets", []) if name_re.fullmatch(x.get("name", ""))]
    if not assets:
        die(f"no dmem-{a.tag}-* archives on the release")
    have_sigs = {x["name"] for x in rel.get("assets", []) if x["name"].endswith(".minisig")}

    upgrade_rs = base64.b64decode(gh_api(f"repos/{a.repo}/contents/src/upgrade.rs?ref={a.tag}")["content"]).decode()
    trusted = pub_b64 in upgrade_rs

    work = tempfile.mkdtemp(prefix="dm-lite-sign-")
    try:
        rows = []
        for x in assets:
            name, size = x["name"], int(x["size"])
            run(["gh", "release", "download", a.tag, "-R", a.repo, "-p", name, "-D", work])
            path = os.path.join(work, name)
            got = os.path.getsize(path)
            if got != size:
                die(f"{name}: downloaded {got} bytes, release says {size}")
            with open(path, "rb") as fh:
                digest = hashlib.sha256(fh.read()).hexdigest()
            rows.append((name, size, digest, (x.get("uploader") or {}).get("login", "?")))

        commit = gh_api(f"repos/{a.repo}/commits/{a.tag}")["sha"]
        same_code, code_msg = tree_check(a.tag, a.remote, commit)
        run_id, built = workflow_build(a.repo, a.tag, commit, work)

        print(f"\nRelease {a.repo} {a.tag} (commit {commit[:10]}): {len(rows)} archive(s)")
        bad = []
        for name, size, digest, who in rows:
            match = built.get(name) == digest
            if not match:
                bad.append(name)
            print(f"  {name}  {size} bytes  sha256 {digest}  by {who}  "
                  + ("= workflow build" if match else "<- DOES NOT MATCH the workflow build"))
        print(f"Build:  release workflow run {run_id}")
        print(f"Code:   {code_msg}")
        print(f"Key:    {key_id} ({a.pub})")
        print(f"Trust:  this tag's src/upgrade.rs {'trusts' if trusted else 'does NOT trust'} that key")
        if not same_code:
            die("the GitHub tag's code is not the upstream tag's code; not signing")
        if bad:
            die(f"{len(bad)} archive(s) differ from what the release workflow built; not signing")
        if not trusted:
            die("this tag's src/upgrade.rs does not trust the key; upgraders would reject the signatures")
        if have_sigs and not a.clobber and not a.no_upload:
            die(f"the release already has {len(have_sigs)} .minisig file(s); rerun with --clobber to replace them")
        if input("\nSign these archives? [y/N] ").strip().lower() not in ("y", "yes"):
            die("aborted; nothing signed")

        secret = getpass.getpass(f"Passphrase for key {key_id}: ")
        try:
            for name, *_ in rows:
                sign(rsign, a.key, work, name, secret)
                v = subprocess.run([rsign, "verify", "-p", a.pub, "-x", name + ".minisig", name],
                                   cwd=work, env=CHILD_ENV, capture_output=True, text=True, timeout=60)
                if v.returncode != 0:
                    die(f"{name}: signature did not verify: {(v.stdout + v.stderr).strip()[-300:]}")
                print(f"  signed + verified  {name}")
        finally:
            del secret

        sigs = [os.path.join(work, n + ".minisig") for n, *_ in rows]
        if a.no_upload:
            out = os.path.abspath(f"minisig-{a.tag}")
            os.makedirs(out, exist_ok=True)
            for s in sigs:
                shutil.copy2(s, out)
            print(f"\nSignatures kept in {out} (not uploaded).")
            return
        cmd = ["gh", "release", "upload", a.tag, "-R", a.repo, *sigs] + (["--clobber"] if a.clobber else [])
        subprocess.run(cmd, check=True)
        print(f"\nUploaded {len(sigs)} signature(s) to {a.repo} {a.tag}.")
    except PermissionError as e:
        die(str(e))
    except subprocess.CalledProcessError as e:
        die(f"{' '.join(e.cmd[:3])} failed: {(e.stderr or '').strip()[-300:]}")
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
