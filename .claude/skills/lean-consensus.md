---
name: lean-consensus
description: "Guidance for implementing, reviewing, or refactoring consensus node logic in the lean client Rust codebase. Covers Services, EventSources, Events, Messages, Effects, and runtime architecture patterns."
---

# Lean Consensus Architecture Guide

You're working with a strict deterministic simulation-testable consensus client architecture.

## Core Architecture Rules

### Runtime Crate Components

**EventSources** — Non-determinism isolation
- Minimal API surface (easier to simulate)
- No business logic — thin adapters only
- Produce Events, handle Effects
- Production: own tokio threads | Testing: test doubles

**Services** — Deterministic state machines
- ALL business logic lives here
- Input: `Event | Message` → Output: `Effect | Message`
- ZERO non-determinism (no random, time, I/O)
- Production: own tokio thread | Testing: sequential + deterministic shuffle

**Events** — EventSource outputs, entry point of non-determinism

**Messages** — Inter-Service communication

**Effects** — Service outputs to specific EventSource

### Decision Priority Order

1. **Correctness**: Matches specification?
2. **Determinism**: Non-determinism isolated in EventSources?
3. **Testability**: Works in deterministic simulation?
4. **Architecture**: Components in correct places?
5. **Type safety**: Type system prevents misuse?
6. **Performance**: Efficient hot paths, minimal allocations?
7. **Observability**: Appropriate tracing without overhead?
8. **Readability**: Clear to other engineers?

## Rust Code Standards

### Type System
- Make illegal states unrepresentable
- Newtypes for domain concepts (not raw `u64` for heights/slots)
- `enum` over booleans or strings
- `#[must_use]` where appropriate
- No `unwrap()`/`expect()` unless provable

### Performance
- `&[T]` over `Vec<T>` in signatures where possible
- `SmallVec`/`ArrayVec` for small bounded collections
- Avoid unnecessary cloning
- Hot path optimization, cold path readability

### Error Handling
- Domain-specific errors with `thiserror`
- Informative, actionable messages
- `Result` for recoverable, `debug_assert!` for invariants

## Tracing Guidelines

- **Hot paths**: `trace!`/`debug!` sparingly, use `tracing::enabled!` guards
- **Cold/error paths**: `warn!`/`error!` freely with rich context
- **State transitions**: `info!` with before/after state
- **Service boundaries**: `debug!` for Input/Output
- **EventSource boundaries**: `debug!` for Event/Effect
- `#[instrument]` on Service handlers
- NEVER log private keys/seeds

## Component Checklists

### New Service
- [ ] In `runtime` crate, correct module
- [ ] Takes only `Input`, produces only `Output`
- [ ] Zero non-determinism
- [ ] All state transitions deterministic + documented
- [ ] Tracing instrumentation
- [ ] Associated Event/Message/Effect types defined

### New EventSource
- [ ] Minimal API surface
- [ ] No business logic
- [ ] Produces well-typed Events
- [ ] Handles Effects
- [ ] Easy to test double
- [ ] Document abstraction level choice

### Before Finalizing
1. Check `lean_client/spec` for relevant specification
2. Verify no non-determinism in Services
3. Verify EventSources minimal and simulatable
4. Check `make generate-test-vectors` coverage
5. Verify error path handling
6. Verify naming/placement conventions

## Specification Alignment

- Spec location: `lean_client/spec`
- Implementation may diverge for production needs
- Document intentional divergences with rationale
- Spec prioritizes clarity, implementation adds robustness + performance
