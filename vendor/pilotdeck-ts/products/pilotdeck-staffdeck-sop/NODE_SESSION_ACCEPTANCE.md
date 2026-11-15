# SOP Node Session Acceptance

## Contract

The public field is `contextMode`, with values `new_session` and `inherit`.
The runtime default is `new_session`. An explicit owner step value or a local
node value overrides the module default. Invalid values are rejected before
the prepared SOP state is persisted.

Each isolated node has a persistent `AgentSession`, its own event recorder and
a sidechain transcript. Identity is derived from parent session, SOP and node,
without a turn ID. The child records `parentSessionId`; retries and subsequent
turns reuse it. `inherit` runs in the parent with the available preceding
messages. A fresh isolated node receives only the current human input and the
SOP owner's slot/instruction projection.

| Concern | Session owner |
| --- | --- |
| Context preparation, cache, memory capture, compaction | Internal node for `new_session`; parent for `inherit` |
| Internal model/tool admissions and transcript | Internal node for `new_session` |
| Sticky routing, aggregate usage, turn/task budgets | Parent |
| SOP state, revision, wait, resume and reply delivery | Parent |
| Gateway events, normal session list and visible replies | Parent |

## Coverage

- Config, local definitions and HTTP wire: default, both valid modes, invalid values and node overrides.
- SOP loop: both override directions, exclusion of old user/assistant/tool history, full inherited messages, four ending states, tool failure, model retry and bounded protocol correction.
- Real DefaultContext runtime: separate cache identities, memory capture identities, durable child compaction, exclusion of previous-node summaries and parent usage totals.
- Budget regression: monetary turn/task budgets and `maxTurns` remain shared across node switches.
- Gateway integration: real child events and parent metadata, A/B identity separation, same-node reuse, main-only listing, visible transcript without internal duplication, restart after awaiting-user and handoff. Runs with native and TCP sidecar AgentLoop.
- External sidecar composition: both context modes with external Model, Tools, Context, Skills and Knowledge, compaction and authenticated approval.
- Real StaffDeck HTTP: mixed node modes, portable owner and formal StaffDeck process restarts plus Gateway recreation, stable wait/revision, stale and wrong-parent rejection, duplicate resume and one final visible delivery.

## Commands

Use Node 22 and unset `NODE_OPTIONS`. Install the worktree dependencies before
running these commands. Build commands must finish before dist-based tests run.

```bash
env -u NODE_OPTIONS npm run build
env -u NODE_OPTIONS npm run test:sop:core
env -u NODE_OPTIONS node --test --test-force-exit dist/tests/pilot/config/modules-config.spec.js
env -u NODE_OPTIONS node --test --test-force-exit dist/tests/composition/sop-node-session-e2e.spec.js
env -u NODE_OPTIONS node --test --test-force-exit --test-name-pattern='routing once per turn|sticky routing|binds host routing' dist/tests/agent/modules/sidecar-client.spec.js
```

The external composition and real HTTP tests require a StaffDeck checkout with
`portable_sop` and `app.public_api.pilotdeck_domain_host`, Python dependencies,
and an installed Harness runtime. Set:

```bash
export STAFFDECK_SOP_ROOT=/path/to/StaffDeck
export STAFFDECK_PYTHON=/path/to/python
export STAFFDECK_HARNESS_ROOT=/path/to/deepseek-harness
env -u NODE_OPTIONS node --test --test-force-exit dist/tests/composition/external-sidecar-sop-session-e2e.spec.js
STAFFDECK_SOP_E2E_ENDPOINT=http://127.0.0.1:8091 env -u NODE_OPTIONS npm run test:sop:http-e2e
```

The supplied HTTP endpoint must run `staffdeck_sop_runtime.api:app`. The restart
case starts an additional isolated portable owner and a formal StaffDeck API
with a file-backed SQLite database. It restarts only its own processes.
Missing runtime dependencies are blocked checks, not acceptance passes.
Synthetic Context IDs alone are not evidence of a real node session.

## Verified Run

On 2026-10-07, Node 22.23.1 passed the build and all checks below:

| Check | Result |
| --- | --- |
| SOP core (loop, wire and definitions) | 83/83 |
| Module configuration | 24/24 |
| Real node Gateway sessions, native and TCP sidecar | 4/4 |
| External sidecar modules, both context modes | 4/4 |
| Real StaffDeck HTTP, including service and Gateway restart | 11/11 |
| Parent sticky routing with child recording execution | 3/3 |
| Diff whitespace validation | Pass |

The HTTP suite used an isolated portable owner at `127.0.0.1:18091` because
the requested `8091` endpoint was unavailable. StaffDeck came from the
`StaffDeck-thread-approval-g7-20261004` checkout; Harness was
`deepseek-harness-dsh-v0.1.2-alpha.2`. Test-owned services and temporary
dependency links were removed after validation. No deployment was performed.
