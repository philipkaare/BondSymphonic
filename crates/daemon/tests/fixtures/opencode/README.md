# OpenCode fixtures

`documented-acp-v1.json` is hand-authored, sanitized, synthetic fake-peer input.
It is **not captured** and proves no working executable, model, sandbox, or MCP connection.
The examples are independent scenarios, not a single sequential conversation.
`newMinimalResponse` retains an empty minimal configuration example. Concrete new/load/resume
responses use a tagged-builder-derived flat model selector with two disposable models.
The selection request chooses a returned value; its response and update reflect that value.
The tagged builder emits flat options. Grouped options are allowed by ACP but are not
claimed as this release's output. Treat returned values as opaque despite the candidate's
provider/model format.
The terminal rejection pair is a defensive protocol case, not an observed OpenCode event.
`nativeConfigurations` contains proposed native Ask/YOLO configs, not ACP messages.
Global/project/agent precedence and effective permission behavior remain live checks.

Wire framing: serialize each envelope as compact UTF-8 JSON followed by LF.
Do not send this wrapper object as an ACP message.
The initialized agent version is a source-derived candidate; no binary was tested.
See `docs/superpowers/plans/notes/2026-09-30-opencode-compatibility.md` for evidence and limitations.
