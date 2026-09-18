#!/usr/bin/env python3
"""tsgen -- build a probe request body from the recorded template, varying the "起手信息".

Why: every probe used to replay the same template verbatim, so all of them carried the same
reasoning effort and the same Chinese title-generation prompt. The backend sees a request's shape,
and we do not yet know which shapes get a 292. Varying them is how that gets measured -- and it
costs no Rust changes, because `codex-rec tsgrab --patch` already rewrites arbitrary text in the body.

The template's own `<environment_context>` and client ids stay alone: `tsgrab` refreshes the ids and
`[rewrite.environment]` fixes the timezone/date at send time. This script only touches the prompt
text and the reasoning effort.

Usage
  tsgen.py --out /path/body.zst            # random shape
  tsgen.py --out /path/body.zst --show     # ...and print what was chosen
  tsgen.py --list                          # show the prompt shapes and effort levels, then exit
"""

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
import tempfile

ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"

DEFAULT_TEMPLATE = "/root/tsgrab/templates/probe.req.body"

# The template's last user message is a title-generation instruction ending in these two lines.
# Everything before them is the instruction block; the text after is the user's request we replace.
PROMPT_MARKER = "User prompt:"

# Prompt shapes, one per "kind of thing a user asks codex". Deliberately short: this is a probe, and
# a heavier body only changes what the backend has to charge for, not the routing decision we are
# sampling. The instruction block in the template says "do not answer the request", which keeps the
# generation tiny.
SHAPES: list[tuple[str, str]] = [
    ("code",        "Write a Python function that returns the n-th Fibonacci number."),
    ("fix",         "This function crashes on an empty list. What is the likely cause?"),
    ("explain",     "In two sentences, explain what a mutex is."),
    ("shell",       "Write a one-line shell command that counts files in the current directory."),
    ("refactor",    "Rename the variable tmp to buffer in this snippet and say what changed."),
    ("test",        "Name three edge cases worth testing for a function that parses dates."),
    ("prose",       "Write a haiku about a pelican riding a bicycle."),
    ("question",    "What does the --strip-components flag do in tar?"),
    ("data",        "Given rows of CSV, what is the shortest way to sum the third column?"),
    ("review",      "List two things to check in a code review of a retry loop."),
]

# Reasoning effort, from the middle of the range upward: too low and the request stops looking like
# real agent traffic, too high and every probe is expensive. "minimal" and "none" are deliberately
# absent for that first reason.
EFFORTS = ["medium", "high", "xhigh"]


def zstd_decode(path: str) -> bytes:
    return subprocess.run(["zstd", "-d", "-q", "-c", path],
                          capture_output=True, check=True).stdout


def zstd_encode(data: bytes, path: str) -> None:
    # Write to a temp file first: a half-written body would be a corrupt probe.
    d = os.path.dirname(os.path.abspath(path)) or "."
    fd, tmp = tempfile.mkstemp(dir=d, prefix=".tsgen-", suffix=".zst")
    try:
        with os.fdopen(fd, "wb") as fh:
            p = subprocess.run(["zstd", "-q", "-c"], input=data,
                               stdout=fh, stderr=subprocess.PIPE)
            if p.returncode != 0:
                raise RuntimeError("zstd failed: %s" % p.stderr.decode(errors="replace"))
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def replace_user_prompt(body: dict, new_prompt: str) -> tuple[bool, str]:
    """Rewrite the text after `User prompt:` in whichever message carries it.

    Returns (found, old_prompt). The replacement keeps the instruction block and the marker itself,
    so the request still reads like the real client's title-generation turn.
    """
    for item in body.get("input") or []:
        if item.get("type") != "message":
            continue
        parts = item.get("content")
        if not isinstance(parts, list):
            continue
        for part in parts:
            if not isinstance(part, dict):
                continue
            text = part.get("text")
            if not isinstance(text, str) or PROMPT_MARKER not in text:
                continue
            head, _, old = text.partition(PROMPT_MARKER)
            part["text"] = "%s%s\n%s" % (head, PROMPT_MARKER, new_prompt)
            return True, old.strip()
    return False, ""


def set_effort(body: dict, effort: str) -> tuple[bool, str]:
    """Set reasoning.effort, reporting the previous value."""
    r = body.get("reasoning")
    if not isinstance(r, dict):
        return False, ""
    old = r.get("effort", "")
    r["effort"] = effort
    return True, str(old)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--template", default=DEFAULT_TEMPLATE)
    ap.add_argument("--out", help="where to write the new zstd body")
    ap.add_argument("--shape", choices=[s[0] for s in SHAPES],
                    help="prompt shape to use (default: random)")
    ap.add_argument("--effort", choices=EFFORTS, help="reasoning effort (default: random)")
    ap.add_argument("--seed", type=int, help="fix the randomness, for a reproducible probe")
    ap.add_argument("--show", action="store_true", help="print the chosen shape and effort")
    ap.add_argument("--list", action="store_true", help="list the shapes and efforts")
    a = ap.parse_args()

    if a.list:
        print("shapes:")
        for name, text in SHAPES:
            print("  %-9s %s" % (name, text))
        print("efforts: %s" % ", ".join(EFFORTS))
        return 0

    if not a.out:
        ap.error("--out is required (or use --list)")

    rnd = random.Random(a.seed)
    shape, prompt = next(((n, t) for n, t in SHAPES if n == a.shape), rnd.choice(SHAPES)) \
        if a.shape else rnd.choice(SHAPES)
    effort = a.effort or rnd.choice(EFFORTS)

    body = json.loads(zstd_decode(a.template))
    found, old_prompt = replace_user_prompt(body, prompt)
    if not found:
        print("warning: no %r marker found; the prompt was left as-is" % PROMPT_MARKER,
              file=sys.stderr)
    effort_set, old_effort = set_effort(body, effort)

    zstd_encode(json.dumps(body, separators=(",", ":")).encode(), a.out)

    if a.show:
        print("shape        : %s" % shape)
        print("prompt       : %s" % prompt)
        print("effort       : %s%s" % (effort, "  (was %s)" % old_effort if old_effort else ""))
        if old_prompt:
            print("replaced     : %s" % old_prompt[:70])
        if not effort_set:
            print("warning: the body has no reasoning object; effort left alone", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())

