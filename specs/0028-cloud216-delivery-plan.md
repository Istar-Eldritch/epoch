# Delivery Plan: Spec 0028 — Per-Subscription Fail-Closed Delivery Semantics

**Spec (normative):** `specs/0028-cloud216-fail-closed-subscriber-semantics.md` — rules R1–R14 live there.
**Status:** Fully implemented. Base: `a19d001`. Final state: `epoch_pg` consolidated suite 258 passed / 0 failed; `clippy -- -D warnings` clean; `cargo doc` 0 warnings.

This document is a post-implementation reference. The *how* (TDD step order, line anchors, per-phase test recipes) is recorded in the commits; the anchors have drifted and are not reproduced here. Enduring behavioural rules live in the spec (R1–R14) and are encoded in the code and its tests.

## Phase → commit map

| Phase | What it delivered | Commit |
|---|---|---|
| — | Delivery plan committed | `1488dc5` |
| P1 | `FailureMode` trait surface in `epoch_core` (`FailOpen`/`FailClosed`, default bodies, forwarding impls) | `5e8a614` |
| P2 (config) | `on_halt` config surface, **BREAKING** | `66a0086` |
| P2 (mechanics) | Live-path halts, held-event re-attempt, panic containment | `e51b077` |
| P3 | Catch-up / drain halts + outer-loop exit fix | `fe25d09` |
| P4 | Gap refusal + `FenceCleared` recovery | `375ba7f` |
| P4b | Wedged-subscriber private fetch + `release_halt` | `bb973f1` |
| P5 | `ReplayAlways` contiguous HWM; closes CLOUD-227 | `8bebf89` |
| P6 | Regression pins + public-ready docs | `f55c1ed` |

## Design decisions and rationale

- **Halt, not limp.** A fail-closed subscriber that hits an unrecoverable event holds its checkpoint below the bad sequence rather than skipping ahead. Consequence (R12): the read model freezes at the last good event instead of silently diverging. This is the whole point of the mode.

- **`held_event` is the unified halted-at marker.** Both the deserialize wedge and the observer-failure wedge set `state.held_event = Some(seq)`. Using one marker for both is what lets P4b's wedged predicate treat a deser wedge and an observer wedge identically, and lets the held-event re-attempt path re-run the single-event path (deserialize once, one observer invocation, no retry ladder) regardless of which failure caused the hold. `on_halt` fires on halt *entry* and on release only, never per re-attempt, to avoid alert spam.

- **Plain field addition, not `#[non_exhaustive]`, for the config break.** Marking `ReliableDeliveryConfig` `#[non_exhaustive]` would forbid all out-of-crate struct expressions including functional-update syntax (E0639/E0640), breaking the ~40 `ReliableDeliveryConfig { … }` literals in the test crates and making "existing suite green" unsatisfiable. Repo precedent (CHANGELOG.md) ships field additions as plain source-compat notes. So the `on_halt` field is a plain addition shipped as `feat(pg)!` with a CHANGELOG semver note.

- **Rollback, not commit, for T5 gap recovery.** Committing the held sequence makes it *visible*, so it delivers as a normal event and no `SkippedGap`/`FenceCleared` is ever produced (the fence branch only runs when the sequence is not visible). `FenceCleared` requires the writer to abort — so recovery rolls back the held transaction, the sequence provably never existed, the fence clears, and the checkpoint advances.

- **Floor exclusion + private fetch, not shared-floor starvation.** A wedged fail-closed subscriber is excluded from the bus-wide shared floor (`min` over non-wedged states only) so it cannot pin peers below it. Each wedged subscriber is instead driven by its own private fetch (own cursor, own `visible_seqs`, own txid snapshot under the existing fencing gate) so a healthy peer advances past the wedge point (R13) while the wedged one holds. State (contiguous position, `processed_ahead`, `gap_first_seen` incl. first-captured `fence_xmax`, `held_event`) is preserved across re-seeds — the re-seed moves only the fetch cursor — so no `processed_ahead` entry above a refused gap is re-delivered (R6/R14).

- **`ReplayAlways` intra-pipeline window (P2 → P5), resolved in P5.** A fail-closed `ReplayAlways` subscriber acquires a floor-pinning hold on the live path as early as P2 (the live batch maintains `contiguous_checkpoint` identically for both modes and the floor has no mode filter). P4b's exclusion is `Checkpointed`-scoped, so between P2 and P5 a wedged `ReplayAlways` subscriber would starve peers. This window never shipped: P5 lands in the same pipeline before any release and extends the floor exclusion to held `ReplayAlways` HWMs (no release API — its remedy is a fresh `subscribe()` full replay per R9b).

- **CLOUD-227 fix is mode-agnostic.** `ReplayAlways` previously advanced its HWM to each event's sequence before the contiguity guard, so a hole would not hold. The fix routes the `ReplayAlways` sink through the same contiguous-prefix hold logic as checkpoints: the HWM advances only across the unbroken prefix. This is the same contiguity invariant already true on the live path, applied to catch-up.

## Behaviour changes owned

- **`ReliableDeliveryConfig` gains `on_halt`** — breaking, shipped as a plain field addition (`feat(pg)!`, CHANGELOG semver note). See rationale above.
- **Fail-open panic → DLQ** instead of listener death: `on_event` is wrapped in `catch_unwind`, so a panicking observer DLQs and delivery continues rather than killing the listener task.
- **Fail-open `ReplayAlways` HWM fix** (CLOUD-227): HWM now advances only across the contiguous prefix, both modes.
- **`release_halt(subscriber_id, past_sequence)`** — new, purely additive public API. Forward-only vs. the persisted checkpoint; a backward `past_sequence` is rejected. Accepts a skip *past* a sequence the subscriber never finished (R14) — not a rewind.
- No other fail-open behaviour changes; the R10 regression pins guard this.

## Pointers

- Normative rules R1–R14: `specs/0028-cloud216-fail-closed-subscriber-semantics.md`.
- Breaking-change / semver note: `CHANGELOG.md`.
- Per-phase implementation detail: the commits in the table above (`git show <sha>`).
