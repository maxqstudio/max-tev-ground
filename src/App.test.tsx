import { randomFillSync } from "node:crypto";
import { fireEvent, render, screen } from "@testing-library/react";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import App from "./App";
import type { FoundationSnapshot } from "./types";

const fixtureSnapshot: FoundationSnapshot = {
  project: "MAX Research Ground",
  current_phase: "RG-R01",
  phase_lifecycle: "ACTIVE",
  builder_status: "IN_PROGRESS",
  automated_status: "NOT_RUN",
  owner_ui_status: "PENDING",
  owner_acceptance: "PENDING",
  next_phase_unlocked: false,
  deliverables: {
    source_adapters: "PLANNED",
    import_wizard: "PLANNED",
    quality_audit: "PLANNED",
    temporal_contract: "PLANNED",
    snapshots: "PLANNED",
    dataset_registry: "PLANNED",
    persistence: "PLANNED",
    data_ground_ui: "PLANNED",
    acceptance_evidence: "PLANNED",
  },
  pages: [
    { id: "overview", status: "IMPLEMENTED" },
    { id: "data-ground", status: "IN_PROGRESS" },
    { id: "feature-studio", status: "PLANNED" },
    { id: "strategy-studio", status: "PLANNED" },
    { id: "model-lab", status: "PLANNED" },
    { id: "policy-studio", status: "PLANNED" },
    { id: "experiments", status: "PLANNED" },
    { id: "results", status: "PLANNED" },
  ],
  accepted_phases: [{
    phase: "RG-R00",
    owner_acceptance: "OWNER_ACCEPTED",
    owner_ui_status: "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED",
    source_sha: "a1f7ada003d57a26dcd4dfa41e86ba62a5890bed",
    ci_run_url: "https://github.com/maxqstudio/max-tev-ground/actions/runs/39037310698",
    owner_issue_url: "https://github.com/maxqstudio/max-tev-ground/issues/1",
  }],
};

describe("desktop shell navigation", () => {
  beforeAll(() => {
    Object.defineProperty(window, "crypto", {
      configurable: true,
      value: {
        getRandomValues: (buffer: Uint8Array) => randomFillSync(buffer),
      },
    });
  });

  beforeEach(() => {
    mockIPC((command) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      return undefined;
    });
  });

  it("loads the governance snapshot through the Tauri IPC boundary", async () => {
    render(<App />);
    expect(await screen.findByRole("heading", { name: "MAX Research Ground" })).toBeTruthy();
    expect(screen.getByText("NOT RUN")).toBeTruthy();
    expect(screen.getAllByText("LOCKED")).toHaveLength(2);
    expect(screen.getByText("Source adapters")).toBeTruthy();
    expect(screen.getByText(/waived, not tested/)).toBeTruthy();
  });

  it("shows the authorized Data Ground phase as in progress without fake actions", async () => {
    render(<App />);
    await screen.findByRole("heading", { name: "MAX Research Ground" });
    fireEvent.click(screen.getByRole("button", { name: /Data Ground/ }));

    expect(screen.getByRole("heading", { name: "Data Ground" })).toBeTruthy();
    expect(screen.getByText("IN PROGRESS")).toBeTruthy();
    expect(screen.getByText("Data Ground is in progress")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Import|Run|Create/ })).not.toBeTruthy();
  });

  it("discloses an IPC failure instead of fabricating a status", async () => {
    mockIPC((command) => {
      if (command === "get_foundation_status") {
        return Promise.reject(new Error("fixture IPC failure"));
      }
      return undefined;
    });
    render(<App />);

    expect((await screen.findByRole("alert")).textContent).toContain("fixture IPC failure");
    expect(screen.queryByText("NOT RUN")).toBeNull();
  });
});
