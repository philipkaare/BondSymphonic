# Codex 0.158.0 live protocol fixture

`live-0.158.0.ndjson` was captured by `scripts/probe-codex.py` from the official
Linux CLI and matching Code Mode helper inside the actual BondSymphonic bwrap
sandbox and proxy. Entries contain a direction and the original protocol frame.
Credentials are redacted, stderr omitted, and the account plan is anonymized.

The disposable test writes `command-proof.txt` using a shell, creates `proof.txt`
using apply_patch, resumes the thread, steers a second turn, and interrupts it.
Read-only inner policy was used to elicit both approval request types; a
separate live run verified danger-full-access inside the outer sandbox.

These are observed messages, not synthetic protocol examples. They do not prove
cross-process resume, API-key authentication, or every possible error shape.
