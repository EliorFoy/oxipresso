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
