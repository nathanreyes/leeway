# Sync Device Identity Labels

## Context

Folder-sync leases identify their owner with a stable device ID, a per-process session ID,
and a human-readable device label. The initial implementation falls back to `This computer`
when common hostname environment variables are unavailable. That fallback can be written by
more than one device identity, producing the contradictory-looking status `Read-only — This
computer is editing`.

## Decision

Leeway will resolve a cross-platform operating-system device name automatically and persist
it in `device.json`. Existing generic `This computer` labels will be upgraded when the device
record is loaded; stable device IDs will not change.

Lease status wording will continue to use IDs for decisions and labels only for display:

- A lease with the local device ID and another session ID is described as `Another Leeway
  session on this computer`.
- A lease with another device ID uses that device's persisted OS-derived label.
- If the operating-system name cannot be resolved, Leeway uses `Unnamed device`, avoiding a
  false claim that the lease belongs to the current computer.

## Implementation

Use a small cross-platform Rust dependency that reads the operating system's device name
without invoking shell commands. Centralize owner-display wording in the sync module so
startup, lease retry, stale takeover, and Storage & Sync all use the same result. Loading a
legacy generic device label rewrites only the label in `device.json` atomically.

## Failure Behavior

Device-name lookup failure does not prevent Leeway from starting or synchronizing. The stable
device ID remains authoritative, and the display falls back to `Unnamed device`. Invalid or
unreadable device metadata remains an error rather than silently generating a new identity.

## Testing

Tests cover OS-name fallback, generic-label upgrade without changing the device ID,
same-device/different-session wording, different-device wording, and the existing active,
stale, and released lease decisions.
