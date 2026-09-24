# Interaction contract

This document defines the target user journeys for Rottweiler. Requirements here
are acceptance criteria, not claims that every surface is implemented. The
reference anatomy is the repository's full-primary theme, settings, tools, and
review screens. An external design canvas is not a build dependency.

## Vocabulary and navigation

Agent mode means Discuss, Plan, or Execute. Approval policy means Ask, Auto, or
Off. Their wire/config identifiers remain `strict`, `auto-safe`, and `yolo`.
Discuss and Plan remain read-only even with approvals Off. Auto allows audited
safe actions and workspace edits and asks about other actions; explicit denials
remain denials. Headless launches cannot silently acquire interactive authority.

Ctrl+P and slash discovery must expose the same actions, with a single entry for
each action. Sections are Conversation, Models & agents, Context & usage,
Workspace, Safety, and Settings & help. Provider connection is setup within model
selection, not a second kind of model switch. Extension actions retain their
source label. Unavailable actions remain visible with the reason and a recovery
step. State and availability originate in the engine; clients own presentation.

## First session

Launch shows workspace, branch, active instructions, available integrations, and
model selection. If no provider exists, offer connection. If a provider exists
but no usable model is selected, resume model setup. Never display an unresolved
alias as an active model. Authentication leads directly to that provider's model
list. A fresh session selects the first available tool-capable model in provider
catalog order, falling back to the first available model. Connecting another
provider preserves an existing selection.

The first concrete model selection seeds the user default only when the user has
not set one. Subsequent workspace choices stay local. Explicit launch selection,
resumed session state, workspace preference, and configured default retain their
existing precedence. Cached model lists are labeled. Unknown context limits are
shown honestly; no estimate is presented as an authoritative model limit.

A prompt without a usable model is rejected before a turn or message is appended,
with a model-selection remedy. Recovery commands remain usable.

## Turn and approval

Live and restored turns show the same terminal reason: completed, interrupted,
failed, turn limit, loop guard, or budget stop. Errors state what happened and the
next useful action. Raw provider payloads, credentials, internal queue names, and
allocation diagnostics are never product copy. Error dismissal must be explicit
or correlated to successful recovery, rather than any unrelated command result.

Approval previews size to their content and preserve access to steering. Exact
and pattern approvals clearly distinguish their scope. Patterns require review;
a generated glob must never silently authorize shell composition. Plan approval
shows every step, affected file, verification action, and open question, with
scrolling for long plans. Auto never silently denies an action merely because it
is absent from the automatic safe list.

Ctrl+C stops a running response. When idle, two consecutive Ctrl+C presses within
900 ms exit; the first displays the exit hint. Esc closes the current surface;
Esc twice retains the existing stop shortcut. `/exit` is an explicit shutdown.
Foreground shells retain their own terminal and signal handling.

## Screens

At 110×32 and 80×24, navigation screens occupy the primary content area with a
shared title, search, list, optional detail, and footer anatomy. The transcript
must not bleed around modal navigation. Narrow screens collapse details without
hiding required decisions. Footer hints reflect focus and current activity.

Models group by provider and show availability and cached/live state. Agent views
retain finished children, their results, and usage, and allow returning to the
parent without stopping a child. Sessions resume directly; rename is a separate
action. Context inspection selects items directly for pin/evict actions and shows
warnings at 70% and 85%, with proactive compaction at 80% where compatible with
the model's reserved output budget. Compaction completion reports reclaimed tokens
and keeps its summary inspectable.

## Engine and lifetime requirements

A shared available-actions projection supplies state-dependent refusals. Queued
model, mode, and compaction changes acknowledge immediately and execute at a safe
turn boundary. Client navigation commands have engine-owned descriptors without
introducing terminal types into Rust. Child start and completion are independent
of the parent's active stream; children retain session ownership until settled.

Shutdown interrupts work, settles sessions and owned processes, then exits. The
client and supervisor allow the engine's 30-second cleanup proof deadline plus
transport grace. SIGINT, SIGTERM, and SIGHUP request the same cooperative close.
Detach remains explicit. A timeout must not be presented as proven cleanup.

## Acceptance

Use an isolated storage root, never delete a user's Rottweiler directory. Exercise
fresh setup → connection → prompt → approval → edit → test → interrupt → child
start/completion → compact → resume → exit at both screen sizes. Verify failed
and interrupted live turns, configured-provider/no-model restart, multiple-model
activation, explicit model preservation, Auto approval and explicit deny,
scrollable plans, and shutdown during active work. Record native child process
retirement independently of the supervisor's exit. Render tests use the verified
OpenTUI sidecar. Network/account-dependent acceptance is separate from deterministic
fixtures and must be reported as such.
