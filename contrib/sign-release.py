#!/usr/bin/env python3
"""Sign a dm-lite GitHub release: the release manager's step, run by hand.

    contrib/sign-release.py v0.3.6            # check, sign, verify, upload
    contrib/sign-release.py v0.3.6 --no-upload
    contrib/sign-release.py v0.3.9 --packaged DIR   # archives made by scripts/package-release.sh

Run it from a dm-lite clone on the machine that holds the release key. It:
  1. refuses to go on unless ALL of these hold:
     - the GitHub tag's code is identical to the upstream (GitLab) tag of the same
       name in this clone;
     - every archive on the release is byte-identical to its trusted build, so a file
       uploaded or swapped by hand, or built from another commit, is caught. The trusted
       build is a successful run of the release workflow that built that exact commit
       (its build artifact). With --packaged DIR, an archive that the output directory of
       scripts/package-release.sh on this machine holds for that commit (its COMMIT and
       SHA256SUMS files, and the archives themselves, unchanged since it wrote them) is
       checked against that directory instead, and every other archive still against the
       workflow run: a release carrying the hand-built x86_64 linux archive beside the
       workflow's other targets is checked lane by lane;
     - that tag's src/upgrade.rs trusts the key you are signing with;
  2. shows each archive with its sha256 and lane, asks you to confirm, then asks for the
     key passphrase ONCE;
  3. signs every archive with rsign, with the trusted comment `file:<archive>`, a TAB,
     `version:X.Y.Z` (what scripts/package-release.sh prints for a hand signature; `dmem
     upgrade` needs the file field and splits the comment on tabs), and verifies each
     signature with the public key and the comment;
  4. writes SHA256SUMS over every archive on the release (from the checked files), signs
     it the same way (`file:SHA256SUMS`), and uploads the .minisig files, then SHA256SUMS
     with its .minisig, which replace the SHA256SUMS already on the release.

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
# The signed trusted comment. A TAB between the fields: `dmem upgrade` splits the comment on
# tabs and needs a field equal to file:<archive>. The hub's own tooling writes a space, which
# dmem's verifier refuses. scripts/package-release.sh prints the same comment (a test checks
# that the two agree).
COMMENT_FORMAT = "file:{name}\tversion:{version}"


def trusted_comment(name, tag):
    """The trusted comment for archive `name` of release `tag` (vX.Y.Z)."""
    return COMMENT_FORMAT.format(name=name, version=tag.removeprefix("v"))


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


def sign(rsign, key, work, name, secret, comment):
    """Run rsign on a private pty and answer its single password prompt. `comment` is the
    trusted comment the signature carries."""
    pid, fd = pty.fork()
    if pid == 0:
        try:
            # Relative name, so the trusted comment reads file:<archive>, not a temp path.
            os.chdir(work)
            os.execve(rsign, [rsign, "sign", "-s", key, "-x", name + ".minisig", "-t", comment, name], CHILD_ENV)
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


def carries_comment(stdout, stderr, comment):
    """Does `rsign verify` output show this exact trusted comment? rsign2 0.6 prints its
    result on stderr, earlier versions on stdout, so both are read."""
    return f"Trusted comment: {comment}" in (stdout + "\n" + stderr).splitlines()


def tree_check(tag, remote, commit):
    """(ok, message): does the GitHub tag's commit carry the upstream tag's exact tree?
    `remote` is a remote name or a URL. The GitHub tag goes to a private ref, never into
    refs/tags, which holds the upstream tags."""
    ref = f"refs/sign-release/github-tags/{tag}"
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


def packaged_build(directory, commit):
    """(label, {file: sha256}) of what scripts/package-release.sh wrote into `directory` for
    `commit`: its COMMIT file must name that commit, and every archive in the directory must
    be unchanged since the script listed it in SHA256SUMS. A directory that does not satisfy
    that is not a trusted build."""
    commit_file = os.path.join(directory, "COMMIT")
    sums_file = os.path.join(directory, "SHA256SUMS")
    for f in (commit_file, sums_file):
        if not os.path.isfile(f):
            die(f"{f} is missing: --packaged takes the output directory of scripts/package-release.sh")
    with open(commit_file) as fh:
        packaged = fh.read().strip()
    if packaged != commit:
        die(f"{directory} was packaged at {packaged[:10] or '?'}, not at {commit[:10]}")
    listed = {}
    with open(sums_file) as fh:
        for line in fh:
            m = re.fullmatch(r"([0-9a-f]{64}) [ *](\S+)\n?", line)
            if m:
                listed[m.group(2)] = m.group(1)
    digests = {}
    for name in sorted(os.listdir(directory)):
        if not re.fullmatch(r"dmem-v[^/]+\.(tar\.gz|zip)", name):
            continue
        with open(os.path.join(directory, name), "rb") as fh:
            digest = hashlib.sha256(fh.read()).hexdigest()
        if listed.get(name) != digest:
            die(f"{name} in {directory} is not what SHA256SUMS lists: changed since it was packaged")
        digests[name] = digest
    if not digests:
        die(f"no dmem-v* archive in {directory}")
    return f"packaged by scripts/package-release.sh ({directory})", digests


def check_archives(rows, packaged, workflow):
    """[(name, lane or None)] for `rows` of (name, size, sha256, uploader): an archive the
    packaged build holds must match it; any other must match the workflow build. None when
    the archive matches neither (or its lane has no build to check against)."""
    out = []
    for name, _, digest, _ in rows:
        if packaged is not None and name in packaged:
            out.append((name, "packaged build" if packaged[name] == digest else None))
        elif workflow is not None and workflow.get(name) == digest:
            out.append((name, "workflow build"))
        else:
            out.append((name, None))
    return out


SUMS = "SHA256SUMS"


def sums_text(rows):
    """SHA256SUMS over every archive in `rows`, in the format `sha256sum -c` reads, by name."""
    return "".join(f"{digest}  {name}\n" for name, _, digest, _ in sorted(rows))


def main():
    ap = argparse.ArgumentParser(description="Sign a dm-lite GitHub release (human step).")
    ap.add_argument("tag")
    ap.add_argument("--repo", default="wakbijok/dm-lite")
    ap.add_argument("--remote", default="github", help="git remote for the GitHub repo")
    ap.add_argument("--key", default=os.path.join(KEY_DIR, "dm-lite-release-2.key"))
    ap.add_argument("--pub", default=os.path.join(KEY_DIR, "dm-lite-release-2.pub"))
    ap.add_argument("--no-upload", action="store_true", help="sign and verify only; keep the files")
    ap.add_argument("--clobber", action="store_true", help="replace .minisig files already on the release")
    ap.add_argument("--packaged", metavar="DIR",
                    help="trust the archives scripts/package-release.sh wrote into DIR for this tag's commit; "
                         "every other archive is still checked against the release workflow run")
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
        packaged, workflow, labels = None, None, []
        if a.packaged:
            # The hand lane packages the upstream tag's commit. GitHub carries a snapshot commit
            # of the same tree (tree_check proves that), so the packaged COMMIT is checked
            # against the upstream tag, not against the GitHub commit.
            try:
                upstream = run(["git", "rev-parse", f"refs/tags/{a.tag}^{{commit}}"]).strip()
            except subprocess.CalledProcessError:
                die(f"no upstream tag {a.tag} in this clone (fetch it from GitLab first)")
            label, packaged = packaged_build(os.path.abspath(a.packaged), upstream)
            labels.append(label)
        if any(packaged is None or name not in packaged for name, *_ in rows):
            run_id, workflow = workflow_build(a.repo, a.tag, commit, work)
            labels.append(f"release workflow run {run_id}")

        print(f"\nRelease {a.repo} {a.tag} (commit {commit[:10]}): {len(rows)} archive(s)")
        checked = check_archives(rows, packaged, workflow)
        bad = [name for name, lane in checked if lane is None]
        for (name, size, digest, who), (_, lane) in zip(rows, checked):
            print(f"  {name}  {size} bytes  sha256 {digest}  by {who}  "
                  + (f"= {lane}" if lane else "<- DOES NOT MATCH its trusted build"))
        print(f"Build:  {'; '.join(labels)}")
        print(f"Code:   {code_msg}")
        print(f"Key:    {key_id} ({a.pub})")
        print(f"Trust:  this tag's src/upgrade.rs {'trusts' if trusted else 'does NOT trust'} that key")
        if not same_code:
            die("the GitHub tag's code is not the upstream tag's code; not signing")
        if bad:
            die(f"{len(bad)} archive(s) differ from their trusted build; not signing")
        if not trusted:
            die("this tag's src/upgrade.rs does not trust the key; upgraders would reject the signatures")
        if have_sigs and not a.clobber and not a.no_upload:
            die(f"the release already has {len(have_sigs)} .minisig file(s); rerun with --clobber to replace them")
        with open(os.path.join(work, SUMS), "w") as fh:
            fh.write(sums_text(rows))
        print(f"Sums:   {SUMS} over all {len(rows)} archive(s), signed too; it replaces the release's {SUMS}")
        if input("\nSign these archives? [y/N] ").strip().lower() not in ("y", "yes"):
            die("aborted; nothing signed")

        secret = getpass.getpass(f"Passphrase for key {key_id}: ")
        try:
            for name in [n for n, *_ in rows] + [SUMS]:
                comment = trusted_comment(name, a.tag)
                sign(rsign, a.key, work, name, secret, comment)
                v = subprocess.run([rsign, "verify", "-p", a.pub, "-x", name + ".minisig", name],
                                   cwd=work, env=CHILD_ENV, capture_output=True, text=True, timeout=60)
                if v.returncode != 0:
                    die(f"{name}: signature did not verify: {(v.stdout + v.stderr).strip()[-300:]}")
                if not carries_comment(v.stdout, v.stderr, comment):
                    die(f"{name}: the signature does not carry the trusted comment {comment!r}")
                print(f"  signed + verified  {name}")
        finally:
            del secret

        sigs = [os.path.join(work, n + ".minisig") for n, *_ in rows]
        sums = [os.path.join(work, SUMS), os.path.join(work, SUMS + ".minisig")]
        if a.no_upload:
            out = os.path.abspath(f"minisig-{a.tag}")
            os.makedirs(out, exist_ok=True)
            for s in sigs + sums:
                shutil.copy2(s, out)
            print(f"\nSignatures and {SUMS} kept in {out} (not uploaded).")
            return
        cmd = ["gh", "release", "upload", a.tag, "-R", a.repo, *sigs] + (["--clobber"] if a.clobber else [])
        subprocess.run(cmd, check=True)
        # SHA256SUMS is made here from the checked archives, so it always replaces the old one.
        subprocess.run(["gh", "release", "upload", a.tag, "-R", a.repo, *sums, "--clobber"], check=True)
        print(f"\nUploaded {len(sigs)} signature(s), {SUMS} and its signature to {a.repo} {a.tag}.")
    except PermissionError as e:
        die(str(e))
    except subprocess.CalledProcessError as e:
        die(f"{' '.join(e.cmd[:3])} failed: {(e.stderr or '').strip()[-300:]}")
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
