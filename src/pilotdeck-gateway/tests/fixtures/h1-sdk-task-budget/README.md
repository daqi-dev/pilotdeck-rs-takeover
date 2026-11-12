# SDK taskBudget scoped baseline probe

`oracle.cjs` transpiles the complete fixed `cc7c4e6` ProjectRuntimeRegistry and DialogGatewayError modules in memory, without changing budget methods or fs. Only runtime project resolution is the provider seam. It executes real JSONL IO under its own temporary directory inside the isolated PILOT_HOME, and removes only that directory.

The oracle covers session/project isolation, eight concurrent duplicate run settlements, distinct project roots, restart and project retention, cancellation before configuration publication, and the baseline's observed false success after append IO failure. The IO defect is preserved as evidence, not converted into a successful Rust expectation; see `specs/gateway-parity/diffs/A7-SDK-TASK-BUDGET-IO-001.md`.

Two baseline captures and scoped verification completed on 2026-11-13. Original evidence: `_tmp/20261113-sdk-task-budget/`. Each capture records full module hashes. `identity.json` records fixed baseline, Node, driver/oracle/validator hashes and two negative controls. Positive validator passes; injected double-charge and hidden false-success both exit1. The error path replaces only its exact exclusive temporary-root prefix with `<ROOT>` for stable self comparison.

Run from repository root with isolated HOME/PILOT_HOME, both directories created in advance:

```sh
HOME="$PWD/_tmp/20261113-sdk-task-budget/home" PILOT_HOME="$PWD/_tmp/20261113-sdk-task-budget/home/pilot" node src/pilotdeck-gateway/tests/fixtures/h1-sdk-task-budget/oracle.cjs
python3 -I src/pilotdeck-gateway/tests/fixtures/h1-sdk-task-budget/verify.py _tmp/20261113-sdk-task-budget/oracle-first.json
```

Rust owner tests and the actual priced local-gateway test live in `native_process/task_budget.rs`. They are separately compiled and run by the root's build queue; a TS probe or a green validator is not Rust validation.
