import { randomFillSync } from "node:crypto";
import { fireEvent, render, screen } from "@testing-library/react";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import App from "./App";
import type { FoundationSnapshot } from "./types";

const fixtureSnapshot: FoundationSnapshot = {
  project: "MAX Research Ground",
  current_phase: "RG-R00",
  phase_lifecycle: "ACTIVE",
  builder_status: "IMPLEMENTED",
  automated_status: "AUTOMATED_PASS",
  owner_ui_status: "PENDING",
  owner_acceptance: "PENDING",
  ci_status: "NOT_CONFIGURED",
  next_phase_unlocked: false,
  deliverables: {
    foundation: "VERIFIED",
    research_contracts: "TESTED",
    desktop_shell: "IMPLEMENTED",
    persistence: "TESTED",
    governance: "VERIFIED",
    acceptance_evidence: "IMPLEMENTED",
  },
  pages: [
    { id: "overview", status: "IMPLEMENTED" },
    { id: "data-ground", status: "PLANNED" },
    { id: "feature-studio", status: "PLANNED" },
    { id: "strategy-studio", status: "PLANNED" },
    { id: "model-lab", status: "PLANNED" },
    { id: "policy-studio", status: "PLANNED" },
    { id: "experiments", status: "PLANNED" },
    { id: "results", status: "PLANNED" },
  ],
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
    expect(screen.getByText("AUTOMATED PASS")).toBeTruthy();
    expect(screen.getByText("LOCKED")).toBeTruthy();
    expect(screen.getByText("Research contracts")).toBeTruthy();
  });

  it("navigates to a clearly planned module without showing fake actions", async () => {
    render(<App />);
    await screen.findByRole("heading", { name: "MAX Research Ground" });
    fireEvent.click(screen.getByRole("button", { name: /Data Ground/ }));

    expect(screen.getByRole("heading", { name: "Data Ground" })).toBeTruthy();
    expect(screen.getByText("PLANNED")).toBeTruthy();
    expect(screen.getAllByText("RG-R01")).toHaveLength(2);
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
    expect(screen.queryByText("AUTOMATED PASS")).toBeNull();
  });
});