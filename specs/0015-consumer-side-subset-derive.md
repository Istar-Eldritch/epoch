# Spec 0015 — Consumer-Side Subset Derive `#[derive(SubsetOf)]` (CLOUD-174)

**Status:** Proposed
**Created:** 2026-06-14
**Timestamp:** 2606140000
**Crate:** `epoch_derive`
**Commit scope:** `feat(derive)` / concept scope `aggregate`

---

## Problem Statement

> The existing producer-side `#[subset_enum]` attribute macro requires modifying the *original* (superset) enum to declare every subset. This does not work when the superset enum lives in a crate the consumer does not own. As a result `#[subset_enum]` has been effectively abandoned: catacloud now hand-writes mirror enums plus manual `TryFrom`/closure boilerplate at ~10 `SagaAdapter::new` call sites. This spec proposes a *consumer-side* derive macro that lets a downstream crate declare a subset enum in its own crate and have the conversions generated automatically.

### How `#[subset_enum]` works today (producer-side)

`epoch_derive/src/subset.rs` implements `#[subset_enum(SubsetName, VariantA, VariantB)]` as an **attribute macro placed on the original enum**:

```rust
#[subset_enum(UserEvent, UserCreated, UserNameUpdated)]
pub enum AppEvent {
    UserCreated { id: String, name: String, age: u8 },
    UserNameUpdated { id: String, name: String },
    NewSession { user_id: String },
}
```

Because the macro expands on the original enum, it has the original's full variant list (names *and* field shapes) in hand. It therefore generates:

- A brand-new `UserEvent` enum containing only the named variants (copied verbatim, including fields).
- `impl From<UserEvent> for AppEvent` (total).
- `impl TryFrom<AppEvent> for UserEvent` and `impl TryFrom<&AppEvent> for UserEvent`, with an exhaustive `match` over every original variant: included variants map to `Ok(..)`, excluded variants map to `Err(EnumConversionError::new(..))`.
- The reference conversion clones only the matched variant's fields.

The generated `TryFrom<&Original, Error = EnumConversionError>` is exactly the bound required by the `Saga::EventType` associated type (`epoch_core/src/saga.rs:98-100`) and consumed by `Event::to_subset_event_ref` (`epoch_core/src/event.rs:155`).

### Why it was abandoned

`#[subset_enum]` is fundamentally **producer-side**: the subset is declared *on* the superset. This fails for the common real-world topology where:

- The superset event enum is defined in a different crate (e.g. an aggregate's published event enum), and
- A consumer (e.g. a saga in catacloud) only needs a handful of those variants.

The consumer cannot add `#[subset_enum(...)]` lines to a type they don't own, and even if they could, piling every downstream subset onto one upstream enum couples unrelated consumers together and bloats the producer.

The workaround in catacloud is to hand-write a mirror enum plus a manual conversion closure for every saga:

```rust
// Hand-written today, repeated ~10x at SagaAdapter::new call sites.
#[derive(Debug, Clone, EventData)]
enum MachineSagaEvent {
    MachineProvisioned { machine_id: Uuid, pool_id: Uuid },
    MachineReleased { machine_id: Uuid },
}

impl TryFrom<&AppEvent> for MachineSagaEvent {
    type Error = EnumConversionError;
    fn try_from(value: &AppEvent) -> Result<Self, Self::Error> {
        match value {
            AppEvent::MachineProvisioned { machine_id, pool_id } =>
                Ok(Self::MachineProvisioned { machine_id: *machine_id, pool_id: *pool_id }),
            AppEvent::MachineReleased { machine_id } =>
                Ok(Self::MachineReleased { machine_id: *machine_id }),
            other => Err(EnumConversionError::new(
                other.event_type().to_string(),
                "MachineSagaEvent".to_string(),
            )),
        }
    }
}
```

This is pure boilerplate, error-prone (field-by-field cloning, wrong error strings), and must be kept in sync with the superset by hand.

## Requirements

- **R1.** A consumer crate must be able to declare a subset enum with `#[derive(SubsetOf)]` + `#[subset_of(path::to::Super)]` without modifying the superset crate.
- **R2.** The macro must generate `From<Sub> for Super`, `TryFrom<Super> for Sub`, and `TryFrom<&Super> for Sub` — the same three impls produced by `#[subset_enum]` — so the subset satisfies the `Saga::EventType` bound (`for<'a> TryFrom<&'a ED, Error = EnumConversionError>`).
- **R3.** Structural mismatches between the declared subset and the real superset (wrong variant name, wrong field name, wrong field type) must be caught at compile time, not at runtime.
- **R4.** All three field kinds must be supported: unit, tuple, and struct variants.
- **R5.** `#[subset_enum]` must continue to work unchanged (purely additive).
- **R6.** No new runtime `Cargo.toml` dependencies are required. `trybuild` may be added as a dev-dependency for compile-fail tests.
- **R7.** All public items must have rustdoc comments. `cargo clippy -- -D warnings` must pass.
- **R8.** Applying `#[derive(SubsetOf)]` to a non-enum item (struct, union, etc.) must produce a clear compile-time error.

## Success Criteria

- [ ] `#[derive(SubsetOf)]` on a consumer enum generates all three `From`/`TryFrom` impls.
- [ ] The generated `TryFrom<&Super>` satisfies the `Saga::EventType` bound (verified by a generic function in the integration test).
- [ ] Excluded variants produce `Err(EnumConversionError)` with the variant's `event_type()` name.
- [ ] Structural mismatches (bad variant name, bad field) produce a compile error (trybuild or compile-fail test).
- [ ] Applying `#[derive(SubsetOf)]` to a non-enum item produces a clear compile error (compile-fail test).
- [ ] Missing or malformed `#[subset_of(...)]` attribute produces a clear compile error (compile-fail test).
- [ ] `cargo test -p epoch_derive` passes with zero failures.
- [ ] `cargo clippy -p epoch_derive -- -D warnings` passes with zero warnings.

## Scope & Boundaries

**In scope:**
- New `SubsetOf` derive macro in `epoch_derive`.
- Unit tests (prettyplease round-trip) and integration tests for the derive.
- A usage example `epoch_derive/examples/subset_of.rs`.

**Out of scope:**
- Generating the subset enum body from the superset (impossible consumer-side; the consumer writes the enum body themselves).
- Replacing or deprecating `#[subset_enum]`.
- Cross-crate variant discovery / reflection.
- Any changes to `epoch_core`.

## Key Design Constraint

A consumer-side derive runs only on the consumer's own enum tokens. **It cannot see the superset enum's variant list or field types.** This is the inverse of `#[subset_enum]`, which had the superset in hand.

The consequence: the macro generates conversions purely from the variants the consumer declares, and **structural compatibility is enforced by the Rust compiler** when it type-checks the generated `From`/`TryFrom` bodies against the real superset definition:

- If the consumer declares `MachineReleased { machine_id: Uuid }` but the superset's variant is `MachineReleased { id: Uuid }`, the generated `Original::MachineReleased { machine_id }` pattern fails to compile (no field `machine_id`).
- If the consumer names a variant that doesn't exist on the superset, `Original::NoSuchVariant` fails to compile.
- Field *type* mismatches surface as ordinary type errors in the generated body.
- **An included variant must declare exactly the same fields (by name, order, and type) as the corresponding superset variant.** Partial-field subsetting (keeping only some fields of a variant) is not supported — this matches `#[subset_enum]`'s behaviour, because `From<Sub> for Super` constructs `Super::Variant { all_declared_fields }` and missing fields would fail to compile.

This gives the same safety as producer-side generation, just enforced at the consumer's compile step.

Because the macro doesn't know the superset's *full* variant set, the generated `TryFrom` cannot list excluded variants explicitly. Instead it uses a **wildcard arm** that produces an `EnumConversionError`, deriving the offending variant's name from `EventData::event_type()` (which the superset must implement — it already does, since it's used as an `EventData` event enum).

## Proposed API

A new derive macro `SubsetOf` with a `#[subset_of(...)]` helper attribute, placed on the consumer's enum:

```rust
use epoch_derive::{EventData, SubsetOf};
use epoch_core::event::EventData;

#[derive(Debug, Clone, EventData, SubsetOf)]
#[subset_of(some_upstream_crate::events::AppEvent)]
enum MachineSagaEvent {
    MachineProvisioned { machine_id: Uuid, pool_id: Uuid },
    MachineReleased { machine_id: Uuid },
}
```

### Generated code

For superset path `Super` (`AppEvent` above) and subset `Sub` (`MachineSagaEvent`):

```rust
// 1. Total: every subset variant exists on the superset.
impl ::core::convert::From<MachineSagaEvent> for AppEvent {
    fn from(value: MachineSagaEvent) -> Self {
        match value {
            MachineSagaEvent::MachineProvisioned { machine_id, pool_id } =>
                AppEvent::MachineProvisioned { machine_id, pool_id },
            MachineSagaEvent::MachineReleased { machine_id } =>
                AppEvent::MachineReleased { machine_id },
        }
    }
}

// 2. Owned conversion (parity with #[subset_enum]).
#[allow(unreachable_patterns)]
impl ::core::convert::TryFrom<AppEvent> for MachineSagaEvent {
    type Error = ::epoch_core::event::EnumConversionError;
    fn try_from(value: AppEvent) -> ::core::result::Result<Self, Self::Error> {
        match value {
            AppEvent::MachineProvisioned { machine_id, pool_id } =>
                Ok(MachineSagaEvent::MachineProvisioned { machine_id, pool_id }),
            AppEvent::MachineReleased { machine_id } =>
                Ok(MachineSagaEvent::MachineReleased { machine_id }),
            other => Err(::epoch_core::event::EnumConversionError::new(
                ::epoch_core::event::EventData::event_type(&other).to_string(),
                "MachineSagaEvent".to_string(),
            )),
        }
    }
}

// 3. Reference conversion — the one Saga::EventType requires.
//    Clones only the matched variant's fields.
#[allow(unreachable_patterns)]
impl ::core::convert::TryFrom<&AppEvent> for MachineSagaEvent {
    type Error = ::epoch_core::event::EnumConversionError;
    fn try_from(value: &AppEvent) -> ::core::result::Result<Self, Self::Error> {
        match value {
            AppEvent::MachineProvisioned { machine_id, pool_id } =>
                Ok(MachineSagaEvent::MachineProvisioned {
                    machine_id: machine_id.clone(),
                    pool_id: pool_id.clone(),
                }),
            AppEvent::MachineReleased { machine_id } =>
                Ok(MachineSagaEvent::MachineReleased { machine_id: machine_id.clone() }),
            other => Err(::epoch_core::event::EnumConversionError::new(
                ::epoch_core::event::EventData::event_type(other).to_string(),
                "MachineSagaEvent".to_string(),
            )),
        }
    }
}
```

Notes:
- `EnumConversionError` is referenced fully-qualified (`::epoch_core::event::EnumConversionError`) so the consumer is not required to `use` it — matching the `::epoch_core::SubscriberId` convention in `epoch_derive/src/projection.rs`. (The legacy `#[subset_enum]` relies on a bare import; the new macro intentionally improves on this.)
- The reference impl clones matched fields (`.clone()`), identical to `#[subset_enum]`'s behaviour. Field types must therefore be `Clone` — already guaranteed since they are `EventData` fields.
- The error message variant name is obtained via `EventData::event_type()` rather than a literal, because the wildcard arm has no static knowledge of which excluded variant matched. This is a deliberate, minor difference from `#[subset_enum]`, whose error string is e.g. `"AppEvent::NewSession"`; the consumer-side message is `"NewSession"` (the event type). This is acceptable for a "wrong subset" diagnostic.
- Because the macro does not know the superset's full variant set, both `TryFrom` impls are annotated with `#[allow(unreachable_patterns)]`. If the consumer happens to include every superset variant, the wildcard arm becomes unreachable; without the allow, `cargo clippy -- -D warnings` would fail. The attribute is harmless when the wildcard is reachable.
- The superset path (`AppEvent` in the example, or `some_upstream_crate::events::AppEvent`) is emitted **verbatim** in the generated `impl` blocks. It may be a concrete path or a generic path such as `some_upstream_crate::events::Event<Domain>`; the macro does not inspect or rewrite it. Generic supersets are therefore supported as long as the resulting path resolves in the consumer crate.
- `EventData` for the subset is **not** generated by this macro; the consumer adds `#[derive(EventData)]` (already available). This keeps responsibilities separated and mirrors how `#[subset_enum]` interacts with `EventData`.

### Variant field shapes

All three field kinds are supported, exactly as in `subset.rs`:

| Variant kind | Pattern | From / Ok body |
|---|---|---|
| Unit (`A`) | `Super::A` | `Sub::A` |
| Tuple (`A(T, U)`) | `Super::A(__field0, __field1)` | tuple with positional binds (ref impl clones each) |
| Struct (`A { x, y }`) | `Super::A { x, y }` | struct with named binds (ref impl clones each) |

Tuple binders use the `__field{i}` naming scheme already used in `subset.rs` to avoid collisions.

## Codebase Map

| Location | Symbol | Role in this work |
|----------|--------|-------------------|
| `epoch_derive/src/lib.rs:1` | module list + exports | Add `mod subset_of;` and `#[proc_macro_derive(SubsetOf, attributes(subset_of))]` |
| `epoch_derive/src/subset.rs:1` | `subset_enum_impl_internal` | Reference pattern for field-binding logic, `prettyplease` test harness |
| `epoch_derive/src/projection.rs:1` | `subscriber_id_impl` | Reference for `split_for_impl`, struct rejection, fully-qualified `::epoch_core::` paths |
| `epoch_core/src/event.rs:431` | `EnumConversionError` | Error type emitted by generated `TryFrom` impls |
| `epoch_core/src/event.rs:155` | `Event::to_subset_event_ref` | Consumer of `TryFrom<&ED, Error = EnumConversionError>` — ensures contract is correct |
| `epoch_core/src/saga.rs:98` | `Saga::EventType` assoc. type | The bound the generated `TryFrom<&Super>` must satisfy |
| `epoch_derive/tests/subscriber_id_tests.rs:1` | integration test module | Pattern for `epoch_derive` integration tests |
| `epoch_derive/examples/subset.rs:1` | producer-side example | Companion for the new consumer-side example |

**Files to create:**
- `epoch_derive/src/subset_of.rs` — macro implementation + unit tests
- `epoch_derive/tests/subset_of_tests.rs` — integration tests
- `epoch_derive/examples/subset_of.rs` — usage example

---

## Implementation Plan

All new code lives in `epoch_derive`. Follows the structure of `subset.rs` and `projection.rs`. TDD throughout: write a failing test, implement to pass, refactor.

### Phase 1: Skeleton, attribute parser, and `From<Sub> for Super`

Boot the module, parse `#[subset_of(Path)]`, reject non-enum items and malformed attributes with a clear `syn::Error`, and generate the `From` direction.

**Scope:**
- Modify: `epoch_derive/src/lib.rs:1` (`mod` list + proc-macro exports) — add `mod subset_of;` and the `SubsetOf` derive export
- Create: `epoch_derive/src/subset_of.rs` — `subset_of_impl` / `subset_of_impl_internal`, attribute parser, `From<Sub> for Super` codegen, and prettyplease unit tests for all three field kinds (unit / tuple / struct)
- Create: `epoch_derive/tests/compile_pass.rs` (or a doc-test in `subset_of.rs`) — minimal compile-check test that verifies the generated `From` body compiles against a real superset enum
  - Parser: parse the input as `syn::DeriveInput` and match on `Data::Enum`; if the input is a struct or union, emit a clear `syn::Error` with a message like `expected enum type, found struct/union` (following the `projection.rs` pattern).
  - Attribute extraction: find the single `#[subset_of(...)]` attribute; if missing or malformed, emit `syn::Error::new_spanned` with a clear message such as `expected #[subset_of(PathToSupersetEnum)]`.
- Out of bounds: `TryFrom` generation, integration tests, generics, docs

**Entry conditions:** None.
**Exit criteria:** `cargo test -p epoch_derive` passes; the `From` unit tests cover unit, tuple, and struct variants. A minimal compile-check test (e.g. in `tests/compile_pass.rs` or as a doc-test) proves the generated `From` body actually compiles against a real superset enum, catching orphan-rule and path-resolution errors early. Add `trybuild = "1.0"` to `epoch_derive/Cargo.toml` dev-dependencies to support the compile-fail tests scheduled in Phase 3.

### Phase 2: `TryFrom<Super>` and `TryFrom<&Super>` with wildcard error arm

Generate the two `TryFrom` impls (owned and reference), including the wildcard excluded-variant arm using `EventData::event_type()` and per-field `.clone()` in the reference impl. This is the core contract that satisfies `Saga::EventType`. Emit `#[allow(unreachable_patterns)]` on both `TryFrom` impls so that a subset which happens to cover every superset variant still passes `cargo clippy -- -D warnings`.

**Scope:**
- Modify: `epoch_derive/src/subset_of.rs` — extend `subset_of_impl_internal` with `TryFrom<Super>` and `TryFrom<&Super>` codegen; add prettyplease unit tests for both impls including wildcard arm and clone behaviour; include an all-variants-included unit test to verify the `#[allow(unreachable_patterns)]` is present
- Out of bounds: generics, integration tests, example, docs

**Entry conditions:** Phase 1 complete and passing.
**Exit criteria:** `cargo test -p epoch_derive` passes; unit tests cover owned + reference conversions for all field kinds and the wildcard arm error message; the all-variants unit test confirms `#[allow(unreachable_patterns)]` is emitted in the generated tokens.

### Phase 3: Generics, integration tests, example, and rustdoc

Thread the *subset's* generics through the `impl` blocks via `split_for_impl()`; use the superset path as-is (it may be concrete or generic). Wire up integration tests including the `Saga::EventType` compile-time assertion; add example and rustdoc.

**Scope:**
- Modify: `epoch_derive/src/subset_of.rs` — add `split_for_impl()` support for the subset's generics; add rustdoc on `subset_of_impl`
- Modify: `epoch_derive/src/lib.rs` — add rustdoc on the `SubsetOf` export (mirror `SubscriberId` style)
- Create: `epoch_derive/tests/subset_of_tests.rs` — integration test with `#[derive(EventData)]` superset + `#[derive(SubsetOf)]` subset; `Saga::EventType` bound assertion via generic function; round-trip `From`/`TryFrom` assertions; an all-variants-included smoke test; a generic subset enum test; compile-fail tests for non-enum input and missing/malformed `#[subset_of(...)]` attribute
- Create: `epoch_derive/examples/subset_of.rs` — consumer-side usage example (superset in separate module, subset deriving `SubsetOf`)
- Out of bounds: any `epoch_core` changes; `#[subset_enum]` modifications

**Entry conditions:** Phase 2 complete and passing.
**Exit criteria:** `cargo test -p epoch_derive` passes; `cargo clippy -p epoch_derive -- -D warnings` passes; generic subset enums compile; a generic superset path compiles; the `Saga::EventType` assertion compiles.

## Files Affected

| File | Change |
|---|---|
| `epoch_derive/src/lib.rs` | Add `mod subset_of;` and the `#[proc_macro_derive(SubsetOf, attributes(subset_of))]` export + rustdoc. |
| `epoch_derive/src/subset_of.rs` | **New.** Macro implementation + unit tests. |
| `epoch_derive/tests/subset_of_tests.rs` | **New.** Integration tests. |
| `epoch_derive/examples/subset_of.rs` | **New.** Consumer-side usage example. |
| `epoch_derive/src/subset.rs` | Unchanged (kept for back-compat). |
| `epoch_derive/Cargo.toml` | Add `trybuild` to dev-dependencies for compile-fail tests. |
| `epoch_core/*` | No changes required. |

No new runtime dependencies (reuses `syn`, `quote`, `proc-macro2`; `prettyplease` already a dev-dependency). `trybuild` is added as a dev-dependency for compile-fail tests.

## Backward Compatibility

- Purely additive. `#[subset_enum]` and all existing generated impls are untouched.
- Consumers migrate at their own pace: replace a hand-written mirror enum + manual `TryFrom` with `#[derive(EventData, SubsetOf)] #[subset_of(Super)]`.

## Comparison: `#[subset_enum]` vs `#[derive(SubsetOf)]`

| Aspect | `#[subset_enum]` (producer-side) | `#[derive(SubsetOf)]` (consumer-side) |
|---|---|---|
| Placed on | The superset enum | The subset enum |
| Owns superset crate? | Required | Not required |
| Subset enum body | Generated by macro | Written by consumer |
| Variant field source | Copied from superset | Declared by consumer; checked by compiler |
| `TryFrom` excluded arm | Exhaustive, literal per-variant | Single wildcard via `event_type()` |
| Error variant string | `"Super::Variant"` | `"<event_type>"` |
| `From` / `TryFrom` / `TryFrom<&>` | ✅ all three | ✅ all three |
| `EventData` impl | Separate (`#[derive(EventData)]`) | Separate (`#[derive(EventData)]`) |

## Alternatives Considered

1. **Make `#[subset_enum]` accept an external path.** Impossible — an attribute macro on the consumer's enum still cannot read the foreign enum's variant fields, which is exactly what `subset_enum` relies on.
2. **A build-time codegen step that reads the superset crate.** Heavy, fragile, and outside the proc-macro model; rejected.
3. **A blanket generic `Subset` trait with runtime matching.** Loses the compile-time structural guarantee and the zero-cost field-level clone behaviour; rejected.
4. **Do nothing.** Consumers keep hand-writing mirror enums (current catacloud state) — the boilerplate and drift risk this ticket exists to remove.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Consumer declares a variant with wrong field shape — silent `_ => Err` at runtime | `From<Sub> for Super` arm uses the declared field names directly; wrong name → compile error |
| `EventData::event_type()` error string differs from `#[subset_enum]`'s `"Super::Variant"` literal | Documented as a known difference; variant is still uniquely identified |
| Consumers with generic subset enums hit `split_for_impl` edge cases | Phase 3 includes explicit generic test coverage |

## Open Questions

- [ ] **Error string parity:** Is `"<event_type>"` an acceptable error variant name, or should the macro accept an optional `#[subset_of(Super, error_name = "...")]`? (Default: ship the simpler `event_type()`-based message.)
- [ ] **Optional escape hatch:** Should `#[subset_of(Super)]` support an attribute to skip generating the owned `TryFrom<Super>` (some consumers only need the `&` form)? (Default: always generate all three for parity.)

## References

- Linear issue: [CLOUD-174](https://linear.app/catallactical/issue/CLOUD-174)
- Existing implementation: `epoch_derive/src/subset.rs` (producer-side `#[subset_enum]`)
- Consumed by: `epoch_core/src/saga.rs:98` (`Saga::EventType` bound)

---

## Phases (JSON)

```json
{
  "phases": [
    {
      "phase": 1,
      "focus": "Skeleton, attribute parser, and From<Sub> for Super",
      "effort": "S",
      "difficulty": "standard"
    },
    {
      "phase": 2,
      "focus": "TryFrom<Super> and TryFrom<&Super> with wildcard error arm",
      "effort": "M",
      "difficulty": "hard"
    },
    {
      "phase": 3,
      "focus": "Generics, integration tests, example, and rustdoc",
      "effort": "M",
      "difficulty": "standard"
    }
  ]
}
```
