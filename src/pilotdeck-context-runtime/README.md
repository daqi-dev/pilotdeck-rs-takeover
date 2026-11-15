# Context runtime extraction

**Not wired and not tested.** This crate is not a workspace member. The five
operation methods are drafted but not integrated with Gateway or Kernel history. Original Gateway source
snapshots are in `_tmp/context-runtime-migration-20261115/migration-source` for
comparison, outside the crate.

The current module tree contains the original multimedia stripping projection,
canonical memory-message projection and retrieval orchestration, prompt assembly,
cache planning, instruction discovery with injected scoped read/list access,
operation and artifact/history ports, the original prepare and compaction
orchestration algorithms, recovery and policy. The history and artifact ports
have no implementation.
The memory projection requires the caller to supply the existing JS display
semantics. Instruction roots must be absolute and host supplied; `list` must
provide actual directory entry names and lstat-style regular-file status.

The remaining migration must supply the summary engine, concrete tool budget
and managed adapters, wire authoritative history, and verify all five methods
without changing JS/UTF-16, fixture or cache semantics. Inject model, full budget, scoped fs,
exclusive artifact publication, instructions and memory capabilities. History
commits require an owner-provided atomic revision check, input-consumption
references, facts and receipt. Old persistence followed by CAS is not atomic.
The existing journal's 32 MiB materialized state limit requires bounded data or
staged immutable artifacts; a resident session lock is not multiwriter CAS.
Retain original fixtures and expected values and validate in the coordinated
verification slot. Kernel Context/History public APIs and Gateway adapters are
future work; this crate does not establish TC-02, TC-03 or TC-05 completion.

Current prompt locale and ISO formatting reuse the original shared agent and
transport functions; these broad dependencies must be narrowed to shared pure
functions or injected equivalents before the extraction is complete.

MemoryControl must be created by the host for each operation and linked to its
actual parent cancellation; no host adapter is present. Before model invocation
prepare must commit its materialization through the future history owner; the
current standalone method is not authorized as an integrated execution path.
