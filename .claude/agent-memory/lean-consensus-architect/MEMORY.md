## Core Architecture Principles

### Service State Ownership (CRITICAL)

- A mutable reference to a service's state MUST NEVER be passed to another service.
- State transitions are the ONLY place that mutates state.
- To mutate state of Service A, you can ONLY ask Service A to make the change via a Message.
- You cannot take the internals of Service A and change them from outside.
- This ensures each Service is a proper deterministic state machine with clear boundaries.

### Service Communication Pattern

- Services take `Input` (Event | Message) and produce `Output` (Effect | Message)
- Events come from EventSources (non-deterministic external world)
- Messages are inter-Service communication
- Effects go from Services to EventSources
- The Node/dispatcher routes Messages between Services in rounds until no more pending

### ChainService Owns the Store

- ChainService is the sole owner of the fork choice Store
- ValidatorService communicates with ChainService through Messages
- Block production is requested via Message, not by passing `&mut Store`
- Attestation processing is requested via Message to ChainService

### Service is a Concept, Not a Type

- **No abstract Service trait or base types** — each concrete service (ChainService, ValidatorService, etc.) defines its own specific Input/Output/Message types
- Services are conceptual state machines, not implementations of a common interface
- This prevents unnecessary abstraction and keeps types precise

### Event/Effect Boundary (event.rs)

The `runtime/src/event.rs` module defines ONLY the EventSource ↔ Service boundary:

- **Event** — messages FROM EventSources INTO Services (entry point of non-determinism)
- **Effect** — messages FROM Services TO EventSources (how deterministic logic affects the world)
- This module should NOT contain Service-to-Service messages

### Per-Service Message Types

Each service defines its own message architecture in its module:

```rust
// In chain.rs:
pub enum ChainMessage {
    ProduceBlock { slot: Slot },
    ProcessAttestation { attestation: Attestation },
}

pub enum ChainInput {
    Event(Event),
    Message(ChainMessage),
}

pub enum ChainOutput {
    Effect(Effect),
    Message(Message), // compound routing type
}
```

Key principles:

- **ServiceMessage** = public enum of messages the service RECEIVES
- **ServiceInput** = Event | ServiceMessage (what the service handler takes)
- **ServiceOutput** = what the service emits (Effects + Messages to other services)
- All service message types are **public** (only services send messages to other services)

### No Message Duplication — Reuse Receiver's Types

When ServiceA sends to ServiceB:

- ServiceA outputs `ServiceBMessage` variants directly
- **Never duplicate** message definitions
- Example: `ValidatorOutput` contains `Message(ChainMessage)` not `Message(ProduceBlock)`

### Compound Message Enum for Routing

At the Node level, create a routing wrapper:

```rust
// In node.rs or lib.rs:
pub enum Message {
    ToChain(ChainMessage),
    ToValidator(ValidatorMessage),
    // ... other services
}
```

This allows the runtime to route messages between services without duplicating message definitions.

## Related Docs

- See `runtime-services.md` for service implementation patterns (when created)
- See `eventsource-patterns.md` for EventSource design guidelines (when created)
