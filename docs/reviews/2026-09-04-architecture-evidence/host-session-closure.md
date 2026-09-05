# Host session closure

Create, fresh preparation, resume and fork now share one identity reservation and owned composition path. The factory task survives the requesting future. The host keeps each accepted opening charged until composition, validation and any required cleanup finish. Wrong identities, descriptor projection failures and sessions returned after shutdown close the actual returned actor before releasing the slot. Factory panics retain the factory and close admission.

Shutdown closes host session admission under the registry lock, wakes waiting openers, and owns its cleanup independently of the requester. Every loaded actor is closed and every accepted opener is awaited. Session closure is concurrent across the bounded registry. Shared factory services close only after all dependent actors/openers settle successfully, because final accounting receipts and SessionEnd hooks still use those services. A failed dependent proof retains the factory instead of shutting it down underneath cleanup.

The shared shutdown proof is sticky. A 30-second proof deadline reports failure while retaining the cleanup task and host owners. No HostShutdown event or Accepted outcome is emitted on failed proof. SessionFactory::shutdown is required rather than an inherited success default.

Validation on native macOS ARM64:

- Complete core unit suite: 368 passed, 1 existing ignored test.
- Final focused host suite after lint corrections: 44 passed.
- Strict all-target, all-feature core Clippy passed.
- New coverage proves dropped create/resume/fork callers retain their opening slots; shutdown survives caller drop; all loaded actors are attempted; failure remains sticky; shared services stay available while sessions close; late fork cleanup precedes shutdown acknowledgement; wrong identities close before capacity release; factory panic retains the actual factory without hanging shutdown.
- Existing late-create/resume races now require actual opening cleanup before shutdown acknowledgement. Readiness and concurrent reconnect tests still pass.

This closes session/factory ownership. The broader host control-command barrier, including already-admitted credential-store mutations, remains part of A09. Runtime consumer and integrated qualification follow the required session-resource interface migration.
