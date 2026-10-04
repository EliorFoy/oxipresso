# Resident-pass engine loop — design note (round 205 investigation)

Goal: TeXpresso's interactive model — a *parked parent* at a read fence, each
rebuild "forks" a fresh typesetting pass that skips format loading (~300-400ms
of the ~600ms warm rebuild). We have no fork; every checkpoint mechanism for
the in-process model is already landed and oracle-tested (capture, restore,
park, inject, mirror rollback, multi-cycle chains — AGENTS "Not Done Yet",
fence LANDED chain through `f175a16`). The only missing piece is **pass
re-entry after a completed run**, since a completed run unwinds the fence
frame (b1 pinned frame-lifetime; `final_cleanup`/`close_files_and_terminate`
also free the pools — r178).

## Source facts (texpresso fork, verified round 205)

- `src/engine/engine/xetex-xetex0.c`: the typesetting loop is its own
  function, `main_control(void)` (L16921) — NOT inlined into a monolithic
  `main`. (This removes the r172 structural fear.)
- `src/engine/engine/xetex-ini.c` L3876 region — the host function
  (`tt_engine_xetex_main`, exactly what our shim calls,
  `oxipresso_xetex_real.c` L1002) ends:
  ```c
  random_seed = (microseconds * 1000) + (epochseconds % 1000000L);
  init_randoms(random_seed);
  ... selector from interaction ...
  if (semantic_pagination_enabled) INTPAR(xetex_generate_actual_text) = 1;
  pdf_files_init();
  synctex_init_command();
  start_input(input_file_name);
  history = HISTORY_SPOTLESS;
  main_control();
  final_cleanup();
  close_files_and_terminate();
  tt_cleanup();
  return history;
  ```
- The web2c functions above take no live-state arguments that aren't pool
  globals — `input_file_name` and friends are engine globals or locals
  re-derivable at pass time.

## Proposed refactor (small, in OUR fork — the split the cross-process plan
needed, now repurposed for the resident loop)

1. `xetex-ini.c`: extract the pre-fmt phase and the tail into
   `xi_fmt_load(roomless...)` / pass body / `xi_teardown()`, keeping
   `tt_engine_xetex_main` as: `fmt_load; [pass loop hook]; teardown`.
   Pass body = the snippet above minus `final_cleanup/close_files/tt_cleanup`
   (those run ONCE at true shutdown, so pools stay live between passes).
2. Shim: after the first pass returns, enter a **pass loop**: park (condvar /
   poll of a Rust command flag); on REPLAY command → `oxi_fence_restore(S0)`
   then call the pass body directly; on STOP → run the deferred teardown and
   return like today. `S0` = the fence capture at the root's first read.
3. Wrapper/CLI: `XetexEngine` gains a resident mode — worker thread owns the
   run; the CLI event loop submits edits via `FenceControl::submit_edit`
   WITHOUT ending the run; each pass emits artifact+mirrors as now (mirror
   rollback already per-pass; `take_output_events` semantics unchanged).

## Open question the implementation must resolve (round 206+)

**The minimal re-callable frame at the fence.** S0 is captured *during*
`start_input`'s first read — restoring pools and calling `main_control()`
fresh would skip `start_input` (its root-open records live in the pools, but
the FIRST document line was read into the input buffer *after* S0: the fresh
call sees S0's pool = pre-read state only if start_input's open-side-effects
are also inside S0 — they are, since open precedes read — but re-running
`main_control` alone means the root's first line is fetched by
`main_control`'s own machinery? No: `start_input` performs the first
get-line; skipping it leaves the input stack with a file record whose buffer
state expects `main_control` to continue *from inside* start_input).
Candidate resolutions, in order of preference:
  a. Fence-capture S1 at **`main_control` ENTRY** (one added capture call in
     our fork right before `main_control()` at L3876, AFTER start_input's
     first read): a fresh `main_control()` call then re-enters at exactly
     S1's semantic point. Rebuilding edits to the root's FIRST line would be
     invisible (already buffered) — acceptable? TeXpresso has the same
     property per-fence: it forks at each read, so a start_input-time edit is
     only re-read via an earlier fence. For our pass loop we keep BOTH:
     edits arriving between passes restore S1 and re-call main_control;
     mid-run edits still chain via S0 fences (already proven r204).
  b. Or make the re-entry unit = `start_input; main_control` (re-call
     start_input after restoring S0) — but S0 is mid-start_input; the
     open-side-effects would double-apply (input-stack re-push). Would
     require capturing at `start_input` ENTRY instead (before the push) —
     the push state then rebuilds cleanly per pass. This is the *truest*
     per-pass semantics (full re-run of the document open with the current
     buffer) and only needs the capture armed one step earlier — likely the
     right answer; verify per-pass idempotence of `pdf_files_init`,
     `synctex_init_command`, `init_randoms`, `history` reset at pass entry.
- Determinism oracle stays: each pass's artifact/mirrors must equal a fresh
  full-run of the current document set (byte-identical, SOURCE_DATE_EPOCH
  pinned), which is the same theorem chain already automated — extend the
  gated tests with a "resident two-pass" case once the loop exists.

## Cost/benefit

Benefit: rebuild ≈ typeset-only (~200-300ms saved of the ~600ms; matches
TeXpresso's child-spawn economics without any process spawning).
Cost: fork-local refactor (≈ dozens of lines), one C pass-loop, Rust drive
API, and the idempotence audit above. NOT required: eqtb graph (in-process
heap pointers stay valid across passes exactly as across replays — proven by
r204 chains).

## Round-208 status — loop landed, one missing layer identified (precise)

Implemented and COMMITTED as verified infrastructure:
- host patch `patches/resident-passes-xetex-ini.patch` (against
  `tt_run_engine`, the real host name; `input_file_name` is a parameter —
  the loop's re-`start_input(input_file_name)` is well-scoped).
- Shim: `oxipresso_resident_capture/park` + park-kind channel (0 = mid-run
  fence, 1 = pass boundary) + `enable_resident_passes` (stub no-op mirrored);
  finish/timeout/disabled automatically falls back to single-run mode.
- Restore upgrade: grown-prefix acceptance (bump pools + realloc keep the
  prefix; captured-size copy + cursor rewind) with shrink always refused;
  AND the previously-missing CURSOR WRITE-BACK (`mem_end/str_ptr/pool_ptr/
  save_ptr/fmem_ptr`, hdr slots 6,8..11) — mid-run fences never needed it
  (capture→restore window is frozen), pass rewinds are impossible without.
- Rust: `arm_resident_passes`, `FenceControl::finish()`, `base_lens`
  (pass-boundary mirrors roll back to the PRE-PASS-1 capture, per-park lens
  only for mid-run fences). Park-1 (inside `resident_capture`) must NOT
  restore — it sits exactly AT S0 and a write-back double-patches.
- All existing suites green (real 17/17 + 1 ignored; stub 146/0).

Discovered requirement — THE SCALAR REWIND SET. With pools+cursors rewound,
pass 2 now truly starts from S0 — and HANGS: engine scalar globals OUTSIDE
the five pools advanced during pass 1 and now disagree with the rewound
pools. Prime suspects (r160 inventory's "dozens of scalar globals", never
captured because frozen-window replays didn't need them): `eqtb_top` + the
`hash[]` array (pass-1 font entries point at str numbers the rewound string
pool no longer holds), run-total counters (`pages_total` etc.),
`param_type`/font-loading bookkeeping. Next increment: enumerate the
mutated-scalar set (diff globals across a completed pass in a debug build,
or start from the web2c `@<globals@>` list minus const-ish ones), extend
the capture header/blocks, and flip the ignored test
`real_engine_resident_pass_rebuild_matches_fresh_run` back to active.
This is the true, in-process form of what AGENTS called the "eqtb pointer
graph" item — and per the r204 finding it is RAW BYTES only in-process
(no pointer re-derivation; segments/faces stay live).

## Round-256 status — three blockers found & fixed, one live hang

Since r212 the scalar-rewind went through three MEASURED fixes (each a
real bug, not a guess):
1. **Silent table truncation** (r212): `OXI_SCALAR_MAX=8192` dropped the
   fmt tail without signal (`regs=8192` exactly on the cap). Fixed: 65536
   cap + adjacent-run coalescing in `oxipresso_undump_record` + explicit
   `g_scalar_overflow` making `oxi_scalars_restore` REFUSE the whole
   rewind rather than half-apply.
2. **Stale-address writes of realloc'd pools** (r243+): the undump stream
   records `str_pool`/`mem` at fmt-load addresses; those arrays GROW and
   MOVE during pass 1, so replaying the recorded addresses wrote freed
   heap (corruption → hang). Fixed: `oxi_reg_overlaps_pool` compaction at
   capture removes regions intersecting the five live pools (the fence
   machinery restores those via LIVE bases); S0 payload 22.0MB → 11.9MB.
3. **Test timing** (r243): `wait_parks` window 30s → 300s (pass-1 in
   debug takes ~40s and parks only land between passes).

**LIVE HANG (current investigation).** With pools+cursors+scalars all
rewound the flow is measurably: S0 capture (regs≈4506, 11.9MB) → park #1
→ pass-1 completes (read counter EXACTLY the plain run's 73845+32) →
boundary park → edit → pass-2 **body enters, `start_input` returns**, then
a 100%-CPU, zero-read, zero-append, flat-memory spin *inside*
`main_control` before its first `big_switch` heartbeat (16M-token
threshold — caveat: a healthy pass only does ~1e5 tokens, so silence
alone doesn't prove the cycle dead). Probe trail committed as temporary
`fprintf` markers in the patched host (`[oxi] S0 capture / resident_park
entry / pass body enter / start_input returned / big_switch iter= /
main_loop iter= / appends=`) plus `XetexEngine::resident_debug_counts()`
and the watchdog thread in the ignored test. The spin starts after the
first input line's first 32 chars (read counter frozen mid-line).
**Next hypotheses, in order:** (a) line/buffer indices (`first`/`last`,
`buf_size`-driven state) stale at the re-read of line 1; (b) input-stack
top marker corrupted despite `start_input` returning; (c) a silent
error-recovery spin with no output (check `history`/`interaction`).
**Fastest discriminator:** drop the big_switch heartbeat to 1M and print
`history, interaction, line, buf` there + once at `start_input returned`
— scalars cycling = loop running on bad scalars; scalars frozen = spin
outside the token loop.

## Round-256b — hang NARROWED to the `\immediate\write` dispatch, via the
second missed rewind set (all measured, in tree)

The input-stack rewind DID land and DID work: S0 payload +120KB
(input_stack = 5000 x 24B exactly), `param_ptr` provably rewound 1 -> 0
across passes, and pass-2 now walks the token stream with a
primitive-call sequence IDENTICAL to pass-1 down to the digit
(`LaTeXReleaseInfo -> show@release@info -> immediate -> write` — the
names were recovered by printing `gettexstring(hash[cs].s1)` in the
csresolve probe). An interim ACCESS VIOLATION was a mis-sized
registration (eof_seen/grp_stack/if_stack are allocated at
`max_in_open`, NOT `stack_size`) — fixed.

Pass-2 STILL stalls at the same point: the 10th primitive call is the CS
token `write` (cmd=59), get_next RETURNS cleanly (a return probe
fires), and then the engine spins (100% CPU, zero reads/writes, flat
memory) inside main_control's dispatch of `\write` — the case handler
loops WITHOUT calling any token primitive, which is why every
entry/restart probe is silent. Pass-1 executes the IDENTICAL command
sequence successfully (call counters match to the digit), so the
divergent input is a pass-boundary value ONE LEVEL BELOW the token
primitives. Remaining candidates: `align_state` (scan_toks' brace-balance
loop), `scanner_status`, and `write_file[]/write_open[]`. Next
discriminator: print `align_state` + `scanner_status` at the dispatch of
cmd=59 in pass 1 vs pass 2, then walk do_extension's \write branch. All
probes are gated by `oxi_debug_pass2` (cost: nothing outside resident
mode). Regression gate: real suite 17/17 + 1 ignored (39.02s) with the
full probe trail and the expanded rewind set — byte-stable.


## r268 - THE RESIDENT PASS THEOREM IS PROVEN (commit 0ea737c)

`real_engine_resident_pass_rebuild_matches_fresh_run` is ACTIVE and GREEN:
pass 2, restored from the S0 checkpoint inside one process, produces an XDV
byte-identical (3665 bytes) to a fresh full run of the final document. The
checkpoint-incremental rebuild core theorem holds on the real XeTeX engine.

### How the last four defects were found

The differential method: run the oracle (fresh, edit2) and the resident
chain (pass-1 on edit1, restore, pass-2 on edit2) in ONE process; dump both
XDVs on mismatch and diff them byte-by-byte. Each fix moved the first
divergence and re-measured:

1. **Hash-table chain links (macro_call divergence 1963 -> 28383).** The
   macro_call0/macro_call2 dual-channel probes showed the SAME CS name
   (`__hook_next class/article/before`) resolving to DIFFERENT eqtb indices
   (8962232 vs 8962234). Root cause: the CS collision chains live in the
   yhash heap allocation, NOT in mem and NOT in the five pools; pass-1's new
   CS entries left stale chain links above the format-load undump range, and
   pass-2 lookups walked ghost chains. Fix: register
   `hash[HASH_BASE..hash_top]` in the rewind set (oxipresso_xetex_real.c).
   (An earlier attempt to register `yhash` failed to compile - the shim's TU
   does not see it; `hash`, the offset-adjusted alias declared in
   xetex-xetexd.h, is the right symbol.)

2. **Rollback must retain output_paths (macro_call divergence 28383 -> pass
   completes, artifact None).** The mirror rollback removed handle->path
   mappings for paths not in the base lens. But the shipout code reuses the
   pass-1 dvi_file handle (dvi_file != NULL), so every pass-2 DVI write went
   through a handle whose path mapping had been deleted; the append callback
   returned "handle not found" and the shim silently dropped the data
   (output_bytes had no simple.xdv at all). Fix: rollback_to_fence no longer
   touches output_paths - only output_bytes content is truncated; the append
   callback's or_default() recreates stale entries on demand. The rollback
   unit test was updated to pin the new contract.

3. **Shipout state reset (artifact 11708 B -> 3516 B of real content).**
   With writes landing again, the pass-2 XDV had an 8 KiB ZERO PREFIX plus
   garbage at offset ~916: my first hand-rolled reset set dvi_limit=0 while
   keeping the pass-1 buffer, breaking the half-buffer swap invariant
   (dvi_limit alternates DVI_BUF_SIZE / HALF_BUF; dvi_offset increments by
   the OTHER size). Fix: the pass body calls oxipresso_shipout_reset()
   (xetex-shipout.c) which closes the stale dvi_file and runs the engine's
   OWN deinitialize_shipout_variables + initialize_shipout_variables pair -
   exactly restoring every invariant. Lesson: never hand-roll resets over
   engine invariants; reuse the engine's init/deinit pairs.

4. **font_used[] full reset (3516 B -> 3538 B -> 3665 B == oracle).** The
   pass-2 XDV was missing the cmmi12 fnt_def1 (exactly -22 bytes): shipout
   emits a font definition only when font_used[f] is false, and font_used[]
   (heap bool array, NOT in the scalar rewind) still held pass-1's true
   values. Bounding the reset by the rewound font_ptr missed exactly the
   DOCUMENT fonts (they live above the S0 font_ptr - they get defined during
   the pass); the first fix returned +22 bytes for a FORMAT font only. Fix:
   sweep the whole array (f < font_max). After this, oracle == pass-2
   byte-for-byte.

### Probe-architecture traps (third and final round)

- Cumulative counters buried under pass-1 traffic (three instances: get_next
  entry, csresolve window, macro_call): pass-2 probes must be gated by a
  pass-2-only counter, never a process-lifetime one.
- Window conditions with the increment INSIDE the window (oxi_tlf++): the
  counter stalls at the window edge forever; increment must be unconditional
  under the flag.
- Block-scope statics are invisible to sibling probes in the same function:
  shared indices must be declared at function scope.

### Final shape of the resident pass loop (xetex-ini.c)

```
while (oxipresso_resident_park() == 1) {
    oxipresso_shipout_reset();      /* dvi_file close + deinit/init pair + font_used */
    start_input(input_file_name);
    history = HISTORY_SPOTLESS;
    main_control();
}
```

All triage probes removed from the engine tree; the shim keeps the one-shot
S0 capture diagnostics and the parks/reads debug export.

### Verification

- stub: 147 passed / 0 failed / 0 ignored (resident test active, env-gated
  self-skip in stub mode)
- clippy: clean; fmt: clean
- real: 18 passed / 0 failed / 0 ignored in 96s (the resident theorem test
  runs as part of the default real suite)

### What remains for P0

- P0.3: wire the resident loop into the CLI rebuild path (OxipressoApp still
  full-restarts; the engine wrapper exposes arm_resident_passes /
  FenceControl::submit_edit / finish).
- P0.4: measure hot-reload vs full-run latency end to end; record in
  AGENTS.md.
