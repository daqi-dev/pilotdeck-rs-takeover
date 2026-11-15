# Non-Knowledge replacement fixture v1

This local read-only acceptance fixture replaces the **Skills** slot through the existing `pilotdeck.skills/v1` Port. It is not Knowledge search or a new module framework. Version/identity/operations/frontend binding are pinned in `manifest.json` and `profile.yaml`. No model or external credential is required for list/read.

## Run focused checks

From the PilotDeck root with locked dependencies installed:

```sh
env -u NODE_OPTIONS PATH=/Users/a1/.nvm/versions/node/v22.23.1/bin:$PATH node --import tsx --test products/pilotdeck-staffdeck-sop/fixtures/replacement-skills-v1/checks.mjs
```

This starts a separate runtime process on an ephemeral port and closes it. It checks the actual runtime manifest/correlation, runtime SkillModulePort and management SkillManagementPort list/read, missing-read error, undeclared operation refusal, incompatible manifest refusal, generated frontend selected/absent imports and production `/api/modules/runtime` sanitized profile. It does not claim a browser or complete G3 matrix PASS.

## Independent normal UI verification

Use your own isolated checkout, home/auth DB, gateway/UI ports and raw directory. Let `FIXTURE` be this directory's absolute path. Start the separate service:

```sh
env -u NODE_OPTIONS REPLACEMENT_SKILLS_PORT=19643 REPLACEMENT_SKILLS_EVIDENCE_PATH="$RAW/skills-calls.jsonl" node "$FIXTURE/runtime.mjs"
```

If19643 is occupied, change both the runtime env and **your isolated copy** of profile.yaml's endpoint to the same free port. Keep manifestPath/callPath/identity/contract/methods unchanged. Generate/build the normal frontend with the original profile-aware command, from your own checkout:

```sh
env -u NODE_OPTIONS PILOTDECK_CONFIG_PATH="$FIXTURE/profile.yaml" node scripts/generate-frontend-modules.mjs
env -u NODE_OPTIONS PILOTDECK_CONFIG_PATH="$FIXTURE/profile.yaml" pnpm --dir ui build
```

For normal host startup use the existing gateway/UI startup command with `PILOTDECK_CONFIG_PATH` selecting the same fixture profile and `PILOT_HOME`/DB/ports set to your own isolation. Export **`PILOTDECK_BUNDLED_SKILLS_DIR="$FIXTURE/skills"` to the UI server process** so the existing `/api/skills/read` path classifier recognizes the fixture's read-only builtin path; this is the existing supported path configuration, not a bypass. The gateway's configured external Skills Port owns list/read. The profile has no configured model and does not advertise chat/model acceptance; if your startup requires a selected model, merge the Skills binding and frontend.businessModules={} into your existing lawful reference-only profile, preserving its model/admission config. Do not place key values in the fixture/profile.

Expected browser `/skills`: existing versioned `pilotdeck.skills` frontend renders exactly one readonly builtin **Replacement guide**, version1.0.0; opening it reads `REPLACEMENT-SKILLS-V1-4826` from the separate process. Build marker is `pilotdeck.skills.ui/v1`. This intentionally reuses the registered compatible public frontend adapter (`ui/src/composition/modules/pilotdeck-skills.tsx`), as supported by the original generator; no new frontend registry entry or core strategy is needed. `/api/modules/runtime` must agree on enabled/implementationId/frontendModule/contract/transport/methods and omit endpoint/private config.

Expected negative boundaries:

- Read nonexistent slug: runtime404 `SKILL_NOT_FOUND`; existing Port preserves machine code.
- Direct module create:409 `MODULE_CAPABILITY_UNAVAILABLE`; management Port rejects undeclared create before dispatch with `MODULE_PROTOCOL_INCOMPATIBLE`. No file/state mutation.
- Wrong manifest identity/contract: Port rejects `MODULE_PROTOCOL_INCOMPATIBLE` before operation.
- In an isolated profile set `modules.skills.enabled=false`, regenerate/rebuild/restart: Skills frontend import is absent and host follows its existing generic unavailable/disabled behavior. Restore/rebuild/restart before positive browser checks.
- Knowledge and SOP remain disabled; no Knowledge request may be used to prove this fixture's replacement.

The default fixture is trusted local loopback, not a production remote authorization service. No create/write/delete/import/validate/scan capability is declared. Full editor CRUD, model turns, all seven slots, browser interactions and alternate provider deployment require their own verification. Preserve first failures and capture the exact SHA/profile/frontend build/runtime request evidence; never transfer old PASS to a new HEAD.
