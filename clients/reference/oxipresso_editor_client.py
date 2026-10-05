#!/usr/bin/env python3
"""Reference editor client for Oxipresso.

This is the integration contract a real editor plugin (emacs / vscode) would
implement, kept runnable so the wire stays honest. It matches TeXpresso's
editor protocol (`src/frontend/editor.c`):

  editor -> engine (one s-expression per line):
    (open "path" base64)            register editor content for a file
    (open-base64 "path" "b64")      same, explicit form
    (change "path" OFFSET REMOVE INSERT)
    (change-lines "path" START REMOVE "new" ...)
    (change-range "path" L1 C1 L2 C2 "new")
    (pause) (resume) (rescan)
    (synctex-forward "path" LINE)
    plus page/window/theme commands

  engine -> editor (one s-expression per line):
    (truncate out|log N)      (truncate-lines out|log N)
    (append out|log POS "text")   (append-lines out|log "line" ...)
    (flush)
    (input-file INDEX "path")
    (lookup-file READ|WRITE SUCCESSFUL|FAILED|PROMISED "path")
    (synctex "path" LINE COL X Y)

Zero third-party dependencies. The interesting entry point is `--selftest`,
which spawns a freshly built `oxipresso` binary (stub engine by default),
drives an initialize + hot-rebuild cycle over the real stdin/stdout wire and
asserts the message shapes, so the shipped binary is validated from OUTSIDE
the Rust process — exactly what an editor plugin does.

Usage:
  python oxipresso_editor_client.py --selftest [--binary PATH]
  python oxipresso_editor_client.py --binary PATH document.tex
      (interactive: reads commands on stdin, prints messages on stdout)
"""

from __future__ import annotations

import argparse
import base64
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time

# --------------------------------------------------------------------------
# s-expression wire parsing (the subset the protocol uses)


def parse_sexpr(line: str):
    """Parse one protocol line into Python lists / str / int. Raises
    ValueError on unbalanced input; the caller treats that as a bad line."""
    tokens = []
    i, n = 0, len(line)
    while i < n:
        c = line[i]
        if c in "()":
            tokens.append(c)
            i += 1
        elif c == '"':
            i += 1
            buf = []
            while i < n and line[i] != '"':
                if line[i] == "\\" and i + 1 < n:
                    esc = line[i + 1]
                    buf.append({"n": "\n", "t": "\t", '"': '"', "\\": "\\"}.get(esc, esc))
                    i += 2
                else:
                    buf.append(line[i])
                    i += 1
            if i >= n:
                raise ValueError("unterminated string literal")
            i += 1  # closing quote
            tokens.append(("str", "".join(buf)))
        elif c.isspace():
            i += 1
        else:
            j = i
            while j < n and not line[j].isspace() and line[j] not in '()"':
                j += 1
            tokens.append(("atom", line[i:j]))
            i = j

    def take(pos):
        if pos >= len(tokens):
            raise ValueError("unexpected end of input")
        tok = tokens[pos]
        if tok == "(":
            pos += 1
            items = []
            while True:
                if pos >= len(tokens):
                    raise ValueError("unclosed list")
                if tokens[pos] == ")":
                    return items, pos + 1
                item, pos = take(pos)
                items.append(item)
        if tok == ")":
            raise ValueError("unexpected )")
        kind, value = tok
        if kind == "str":
            return value, pos + 1
        try:
            return int(value), pos + 1
        except ValueError:
            return value, pos + 1

    value, pos = take(0)
    if pos != len(tokens):
        raise ValueError("trailing tokens after expression")
    return value


def escape_string(text: str) -> str:
    out = []
    for ch in text:
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        else:
            out.append(ch)
    return '"' + "".join(out) + '"'


def serialize(message) -> str:
    """Serialize a parsed message back to the wire (used by --selftest)."""
    parts = []
    for item in message:
        if isinstance(item, list):
            parts.append(serialize(item))
        elif isinstance(item, str) and not isinstance(item, int):
            parts.append(escape_string(item))
        else:
            parts.append(str(item))
    return "(" + " ".join(parts) + ")"


# --------------------------------------------------------------------------
# the client


class OxipressoEditorClient:
    """Spawns an `oxipresso` binary and speaks the editor wire with it."""

    def __init__(self, binary: str, root_file: str, protocol: str = "sexp",
                 extra_args=None):
        self.binary = binary
        self.root_file = os.path.abspath(root_file)
        self.protocol = protocol
        self.extra_args = list(extra_args or [])
        self.proc: subprocess.Popen | None = None
        self.messages: "queue.Queue[list]" = queue.Queue()
        self.transcript: list[str] = []
        self._stderr_lines: list[str] = []
        self._alive = True

    # -- lifecycle ---------------------------------------------------------

    def start(self) -> None:
        args = [self.binary]
        if self.protocol == "json":
            args.append("-json")
        args += self.extra_args
        args.append(self.root_file)
        self.proc = subprocess.Popen(
            args,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        threading.Thread(target=self._pump_stdout, daemon=True).start()
        threading.Thread(target=self._pump_stderr, daemon=True).start()

    def close(self) -> None:
        self._alive = False
        if self.proc and self.proc.stdin:
            try:
                self.proc.stdin.close()
            except OSError:
                pass
        if self.proc:
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def stderr_text(self) -> str:
        return "".join(self._stderr_lines)

    # -- wire --------------------------------------------------------------

    def _pump_stdout(self) -> None:
        assert self.proc and self.proc.stdout
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            self.transcript.append(line)
            try:
                message = parse_sexpr(line)
            except ValueError as error:
                self._stderr_lines.append(f"[client] unparseable wire line: {error}\n")
                continue
            self.messages.put(message)
        self._alive = False

    def _pump_stderr(self) -> None:
        assert self.proc and self.proc.stderr
        for line in self.proc.stderr:
            self._stderr_lines.append(line)

    def send(self, command: str) -> None:
        assert self.proc and self.proc.stdin, "client not started"
        self.proc.stdin.write(command + "\n")
        self.proc.stdin.flush()

    # -- editor operations (the plugin-facing surface) ---------------------

    def open_document(self, path: str, content: bytes) -> None:
        payload = base64.b64encode(content).decode("ascii")
        self.send(f'(open-base64 "{path}" "{payload}")')

    def change(self, path: str, offset: int, remove: int, insert: str) -> None:
        self.send(f'(change "{path}" {offset} {remove} {escape_string(insert)})')

    def pause(self) -> None:
        self.send("(pause)")

    def resume(self) -> None:
        self.send("(resume)")

    def rescan(self) -> None:
        self.send("(rescan)")

    # -- message consumption -----------------------------------------------

    def next_message(self, timeout: float = 30.0):
        """Blocks for the next parsed message; None on timeout or exit."""
        try:
            return self.messages.get(timeout=timeout)
        except queue.Empty:
            return None

    def wait_for(self, predicate, timeout: float = 30.0):
        """Collects messages until one satisfies `predicate` (which is also
        returned); None on timeout. Everything seen stays on the transcript."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            message = self.next_message(timeout=max(0.05, deadline - time.time()))
            if message is None:
                return None
            if predicate(message):
                return message
        return None


def message_name(message) -> str:
    return message[0] if isinstance(message, list) and message else "?"


def buffer_name(item) -> str:
    return item if isinstance(item, str) else "?"


# --------------------------------------------------------------------------
# selftest against the shipped binary


def locate_default_binary() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    candidates = [
        os.path.join(here, "..", "..", "target", "debug", "oxipresso.exe"),
        os.path.join(here, "..", "..", "target", "debug", "oxipresso"),
        os.path.join(here, "..", "..", "target", "release", "oxipresso.exe"),
        os.path.join(here, "..", "..", "target", "release", "oxipresso"),
    ]
    for path in candidates:
        if os.path.isfile(path):
            return os.path.abspath(path)
    raise SystemExit(
        "oxipresso binary not found under target/{debug,release}; "
        "build it first (cargo build -p oxipresso-cli) or pass --binary"
    )


def run_selftest(binary: str) -> int:
    failures: list[str] = []

    def check(condition, description: str) -> None:
        if condition:
            print(f"  ok   {description}")
        else:
            failures.append(description)
            print(f"  FAIL {description}")

    temp_dir = tempfile.mkdtemp(prefix="oxi-client-selftest-")
    doc_path = os.path.join(temp_dir, "main.tex")
    doc = "\\documentclass{article}\n\\begin{document}\nClient\n\\end{document}\n"
    with open(doc_path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(doc)

    print(f"selftest: spawning {binary}")
    client = OxipressoEditorClient(binary, doc_path)
    client.start()
    try:
        # -- initialization cycle: truncates, appends, flush, input-file ----
        first = client.next_message(timeout=60)
        check(
            first is not None
            and message_name(first) == "truncate"
            and buffer_name(first[1]) == "out"
            and first[2] == 0,
            f"first message truncates the out buffer: {first}",
        )
        flush = client.wait_for(lambda m: message_name(m) == "flush", timeout=60)
        check(flush is not None, "initialization stream ends with (flush)")
        input_file = client.wait_for(
            lambda m: message_name(m) == "input-file", timeout=60
        )
        check(
            input_file is not None and input_file[2].endswith("main.tex"),
            f"input-file notification names the root: {input_file}",
        )

        # -- hot rebuild: an editor change produces a second full stream ----
        offset = doc.index("Client")
        client.change("main.tex", offset, len("Client"), "Edit0r")
        rebuild_truncate = client.wait_for(
            lambda m: message_name(m) == "truncate"
            and buffer_name(m[1]) == "out",
            timeout=60,
        )
        check(rebuild_truncate is not None, "a change triggers a fresh out truncate")
        flush2 = client.wait_for(lambda m: message_name(m) == "flush", timeout=60)
        check(flush2 is not None, "the rebuild stream ends with (flush)")

        # -- pause/resume keeps the session alive ---------------------------
        client.pause()
        client.resume()
        resumed = client.wait_for(
            lambda m: message_name(m) == "truncate" and buffer_name(m[1]) == "out",
            timeout=60,
        )
        check(resumed is not None, "resume rebuilds after a paused span")
    finally:
        client.close()
        print("---- wire transcript (first 12 lines) ----")
        for line in client.transcript[:12]:
            print("  " + line[:160])
        if client.stderr_text().strip():
            print("---- engine stderr (first 6 lines) ----")
            for line in client.stderr_text().splitlines()[:6]:
                print("  " + line[:160])
        try:
            os.remove(doc_path)
            os.rmdir(temp_dir)
        except OSError:
            pass

    if failures:
        print(f"selftest FAILED ({len(failures)} check(s))")
        return 1
    print("selftest PASSED")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Reference editor client for Oxipresso")
    parser.add_argument("document", nargs="?", help="root .tex document to open")
    parser.add_argument("--binary", default=None, help="oxipresso binary to spawn")
    parser.add_argument("--json", action="store_true", help="speak the JSON wire form")
    parser.add_argument("--stream", action="store_true", help="start in stream mode")
    parser.add_argument(
        "--selftest", action="store_true",
        help="spawn the binary and validate initialize + hot rebuild end to end",
    )
    args = parser.parse_args()

    if args.selftest:
        binary = args.binary or locate_default_binary()
        return run_selftest(binary)

    binary = args.binary or locate_default_binary()
    if not args.document:
        parser.error("a document is required without --selftest")
    client = OxipressoEditorClient(
        binary, args.document, protocol="json" if args.json else "sexp",
        extra_args=["-stream"] if args.stream else [],
    )
    client.start()
    print("[client] session started; type editor commands, Ctrl-D to exit")
    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            client.send(line)
            while True:
                message = client.next_message(timeout=5)
                if message is None:
                    break
                print(serialize(message))
    except KeyboardInterrupt:
        pass
    finally:
        client.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
