import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { loadStaffDeckSopDefinitions } from "../../src/sop/staffdeck/StaffDeckSopDefinitions.js";
import { createStaffDeckSopAgentLoop } from "../../src/sop/staffdeck/SopAgentLoop.js";

test("SOP definition loader rejects malformed and duplicate definitions", () => {
  const root = mkdtempSync(join(tmpdir(), "pilotdeck-sop-definitions-"));
  try {
    const malformed = join(root, "malformed.yaml");
    writeFileSync(malformed, "sops: [\n");
    assert.throws(() => loadStaffDeckSopDefinitions(malformed), /Invalid StaffDeck SOP YAML/);

    const duplicates = join(root, "duplicates.yaml");
    writeFileSync(duplicates, "sops:\n  - id: duplicate\n  - id: duplicate\n");
    assert.throws(() => loadStaffDeckSopDefinitions(duplicates), /duplicate id/);

    const invalidContextMode = join(root, "invalid-context-mode.yaml");
    writeFileSync(invalidContextMode, "sops:\n  - id: invalid\n    content:\n      nodes:\n        - node_id: start\n          contextMode: shared\n");
    assert.throws(() => loadStaffDeckSopDefinitions(invalidContextMode), /invalid contextMode/);

    for (const [name, value] of [["empty", "''"], ["missing-separator", "model"], ["missing-provider", "/model"],
      ["missing-model", "provider/"], ["null", "null"], ["number", "42"], ["object", "{provider: model}"],
      ["whitespace", "'provider/ model'"], ["structure", "'provider//model'"]]) {
      const invalidModel = join(root, `${name}-model.yaml`);
      writeFileSync(invalidModel, `sops:\n  - id: invalid\n    content:\n      nodes:\n        - node_id: start\n          model: ${value}\n`);
      assert.throws(() => loadStaffDeckSopDefinitions(invalidModel), /invalid model/);
    }

    const nonStringModel = join(root, "non-string-model.yaml");
    writeFileSync(nonStringModel, "sops:\n  - id: invalid\n    nodes:\n      - node_id: start\n        model: [provider, model]\n");
    assert.throws(() => loadStaffDeckSopDefinitions(nonStringModel), /invalid model/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("SOP definition loader preserves valid node models in root and content nodes", () => {
  const root = mkdtempSync(join(tmpdir(), "pilotdeck-sop-node-models-"));
  try {
    const definitions = join(root, "models.yaml");
    writeFileSync(definitions, `sops:
  - id: root_nodes
    nodes:
      - node_id: classify
        model: openai/gpt-4.1-mini
  - id: content_nodes
    content:
      nodes:
        - node_id: summarize
          model: provider/organization/model
`);
    const loaded = loadStaffDeckSopDefinitions(definitions);
    assert.equal((loaded.sops[0]?.nodes as Array<Record<string, unknown>>)[0]?.model, "openai/gpt-4.1-mini");
    assert.equal(((loaded.sops[1]?.content as Record<string, unknown>).nodes as Array<Record<string, unknown>>)[0]?.model, "provider/organization/model");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("SOP definition loader accepts an exact published management response", () => {
  const root = mkdtempSync(join(tmpdir(), "pilotdeck-sop-published-"));
  try {
    const published = join(root, "published.json");
    writeFileSync(published, JSON.stringify({
      id: "agentbranchver_project_delivery_plan_1_2_1",
      skill_id: "project_delivery_plan",
      version: "1.2.1",
      name: "Project delivery plan",
      description: "Published page response",
      content: {
        skill_id: "project_delivery_plan",
        version: "1.2.1",
        start_node_id: "collect",
        nodes: [{ node_id: "collect", type: "collect_info", contextMode: "new_session" },
          { node_id: "reply", type: "response", contextMode: "inherit" }],
        edges: [],
        terminal_node_ids: ["collect"],
      },
    }));

    const loaded = loadStaffDeckSopDefinitions(published);
    assert.equal(loaded.sops.length, 1);
    assert.equal(loaded.sops[0]?.id, "project_delivery_plan");
    assert.equal(loaded.sops[0]?.version, "1.2.1");
    assert.deepEqual(loaded.sops[0]?.content, {
      skill_id: "project_delivery_plan",
      version: "1.2.1",
      start_node_id: "collect",
      nodes: [{ node_id: "collect", type: "collect_info", contextMode: "new_session" },
        { node_id: "reply", type: "response", contextMode: "inherit" }],
      edges: [],
      terminal_node_ids: ["collect"],
    });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("SOP startup rejects a missing definition file and an absent default binding", () => {
  const root = mkdtempSync(join(tmpdir(), "pilotdeck-sop-definition-boundary-"));
  try {
    const profile = {
      provider: "staffdeck",
      endpoint: "http://unused.test",
      definitionsPath: join(root, "missing.json"),
      defaultSopId: "missing",
      stateRoot: root,
    };
    assert.throws(
      () => createStaffDeckSopAgentLoop({} as never, profile as never),
      /StaffDeck SOP definitions file does not exist/,
    );

    const definitions = join(root, "published.json");
    writeFileSync(definitions, JSON.stringify({
      sops: [{ id: "published", version: "1.0.0", content: { nodes: [{ node_id: "start" }] } }],
    }));
    assert.throws(
      () => createStaffDeckSopAgentLoop({} as never, {
        ...profile,
        definitionsPath: definitions,
      } as never),
      /defaultSopId 'missing' is not present/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
