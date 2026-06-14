# Specification: Consumer-Side Subset Derive (`#[derive(SubsetOf)]`)

**Document ID:** 0015 (CLOUD-174)
**Status:** Proposed
**Created:** 2026-06-14
**Author:** AI Agent
**Crate:** `epoch_derive`
**Motivation:** The existing producer-side `#[subset_enum]` attribute macro requires modifying the *original* (superset) enum to declare every subset. This does not work when the superset enum lives in a crate the consumer does not own (or does not want to edit). As a result `#[subset_enum]` has been effectively abandoned: catacloud now hand-writes mirror enums plus manual `TryFrom`/closure boilerplate at ~10 `SagaAdapter::new` call sites. This spec proposes a *consumer-side* derive macro that lets a downstream crate declare a subset enum in its own crate and have the conversions generated automatically.

---

## Problem Statement

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

## Goals

1. Allow a consumer to declare a subset enum **in their own crate** and derive all conversions, without touching the superset crate.
2. Generate the same conversion surface `#[subset_enum]` produces, so the result drops directly into `Saga::EventType` / `Event::to_subset_event_ref` and `SagaAdapter::new`.
3. Provide a compile-time guarantee that the subset structurally matches the superset (variant names and field shapes), so drift is caught by the compiler rather than at runtime.
4. Keep `#[subset_enum]` working (no breaking changes); the new macro is additive.

## Non-Goals

- Generating the subset enum *definition* from the superset (impossible consumer-side — see "Key Design Constraint"). The consumer writes the enum body themselves.
- Replacing or deprecating `#[subset_enum]` in this change.
- Cross-crate variant *discovery* / reflection. Proc macros cannot read another type's definition.

## Key Design Constraint

A consumer-side derive runs only on the consumer's own enum tokens. **It cannot see the superset enum's variant list or field types.** This is the inverse of `#[subset_enum]`, which had the superset in hand.

The consequence: the macro generates conversions purely from the variants the consumer declares, and **structural compatibility is enforced by the Rust compiler** when it type-checks the generated `From`/`TryFrom` bodies against the real superset definition:

- If the consumer declares `MachineReleased { machine_id: Uuid }` but the superset's variant is `MachineReleased { id: Uuid }`, the generated `Original::MachineReleased { machine_id }` pattern fails to compile (no field `machine_id`).
- If the consumer names a variant that doesn't exist on the superset, `Original::NoSuchVariant` fails to compile.
- Field *type* mismatches surface as ordinary type errors in the generated body.

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
- `EventData` for the subset is **not** generated by this macro; the consumer adds `#[derive(EventData)]` (already available). This keeps responsibilities separated and mirrors how `#[subset_enum]` interacts with `EventData`.

### Variant field shapes

All three field kinds are supported, exactly as in `subset.rs`:

| Variant kind | Pattern | From / Ok body |
|---|---|---|
| Unit (`A`) | `Super::A` | `Sub::A` |
| Tuple (`A(T, U)`) | `Super::A(__field0, __field1)` | tuple with positional binds (ref impl clones each) |
| Struct (`A { x, y }`) | `Super::A { x, y }` | struct with named binds (ref impl clones each) |

Tuple binders use the `__field{i}` naming scheme already used in `subset.rs` to avoid collisions.

## Implementation Plan (TDD)

All new code lives in `epoch_derive`. Follows the structure of the existing `subset.rs` and `projection.rs`.

### Step 1 — New module skeleton
- Add `mod subset_of;` to `epoch_derive/src/lib.rs`.
- Export a derive macro:
  ```rust
  #[proc_macro_derive(SubsetOf, attributes(subset_of))]
  pub fn subset_of(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
      subset_of::subset_of_impl(item)
  }
  ```
- Create `epoch_derive/src/subset_of.rs` with `subset_of_impl` (public, `proc_macro::TokenStream`) delegating to an internal `subset_of_impl_internal(proc_macro2::TokenStream) -> proc_macro2::TokenStream` so it can be unit-tested with the `prettyplease` round-trip harness used in `subset.rs`.

### Step 2 — Parse input
- Parse the item as `syn::ItemEnum` (reject structs/unions with `syn::Error::new_spanned(..).to_compile_error()`, mirroring `projection.rs`).
- Locate the `#[subset_of(...)]` attribute and parse its single argument as a `syn::Path` (the superset type path). Error clearly if the attribute is missing or malformed (`expected #[subset_of(PathToSupersetEnum)]`).

### Step 3 — Generate `From<Sub> for Super` (failing test first)
- Write a unit test asserting the generated `From` body for a mixed unit/tuple/struct subset.
- Implement variant iteration reusing the field-binding logic patterns from `subset.rs` (unit / `Unnamed` / `Named`).

### Step 4 — Generate `TryFrom<Super>` and `TryFrom<&Super>`
- Unit tests asserting both impls, including the wildcard `other => Err(..)` arm and per-field `.clone()` in the reference impl.
- Implement using `EventData::event_type(&other)` for the error's first argument and the subset enum name string for the second.

### Step 5 — Generics & visibility
- Thread the enum's generics through the `impl` blocks via `split_for_impl()` (as in `projection.rs`) so generic subset enums work.
- The macro emits only `impl` blocks (it does not re-emit the enum), so the consumer's own `vis`/attributes are untouched.

### Step 6 — Integration tests
- Add `epoch_derive/tests/subset_of_tests.rs` (pattern: `epoch_derive/tests/subscriber_id_tests.rs`) defining a real superset enum (with `#[derive(EventData)]`) in one module and a `#[derive(SubsetOf)]` subset in another, then assert:
  - `AppEvent::from(sub)` round-trips.
  - `MachineSagaEvent::try_from(&app_event)` returns `Ok` for matched variants and `Err(EnumConversionError)` for excluded ones.
  - Owned `try_from` parity.
  - The subset satisfies `for<'a> TryFrom<&'a AppEvent, Error = EnumConversionError>` (compile-time assertion via a generic function bound) so it is usable as `Saga::EventType`.
- Add an end-to-end test wiring the derived subset into a `Saga` + `SagaHandler`/`SagaAdapter` to confirm it drops in where the hand-written mirror enums currently sit.

### Step 7 — Example & docs
- Add `epoch_derive/examples/subset_of.rs` mirroring `examples/subset.rs` but consumer-side (superset defined separately, subset deriving `SubsetOf`).
- Rustdoc on the `SubsetOf` proc-macro export (matching the `SubscriberId` doc style), explicitly stating: superset must implement `EventData`; field types must be `Clone`; structural mismatches are compile errors.

## Files Affected

| File | Change |
|---|---|
| `epoch_derive/src/lib.rs` | Add `mod subset_of;` and the `#[proc_macro_derive(SubsetOf, attributes(subset_of))]` export. |
| `epoch_derive/src/subset_of.rs` | **New.** Macro implementation + unit tests. |
| `epoch_derive/tests/subset_of_tests.rs` | **New.** Integration tests. |
| `epoch_derive/examples/subset_of.rs` | **New.** Consumer-side usage example. |
| `epoch_derive/src/subset.rs` | Unchanged (kept for back-compat). |
| `epoch_core/*` | No changes required — `EnumConversionError`, `EventData::event_type`, `to_subset_event_ref`, and the `Saga::EventType` bound already exist. |

No new dependencies (reuses `syn`, `quote`, `proc-macro2`; `prettyplease` already a dev-dependency).

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

## Open Questions

1. **Error string parity:** Is `"<event_type>"` an acceptable error variant name, or should the macro accept an optional `#[subset_of(Super, error_name = "...")]`? (Default: ship the simpler `event_type()`-based message.)
2. **Optional explicit-variant escape hatch:** Should `#[subset_of(Super)]` support an attribute to skip generating the owned `TryFrom<Super>` (some consumers only need the `&` form)? (Default: always generate all three for parity.)
