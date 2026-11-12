---
name: pilotdeck-skills-migration
description: "Move explicitly selected skill directories into a PilotDeck skills location."
---

# pilotdeck-skills-migration

Inventory the selected source directories and inspect SKILL.md plus scripts before migration. Preserve relative references and permissions. Choose project .pilotdeck/skills or the explicitly selected global skills directory; never infer permission to replace a same-name skill. Compare source and destination inventories, then reload and query the real catalog. The bundled migration script refuses existing destinations and links; it copies only the specifically supplied source and does not search private home directories.

Copy a reviewed source into a new, explicit destination:

```sh
python3 {{SKILL_ROOT_SHELL}}/scripts/migrate.py "$SOURCE_SKILL" "$DESTINATION_SKILL"
```

The helper checks byte inventories; it does not execute a real PilotDeck reload. Keep the source stable during migration.
