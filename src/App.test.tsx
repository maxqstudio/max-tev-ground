import { randomFillSync } from "node:crypto";
import { fireEvent, render, screen } from "@testing-library/react";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import App from "./App";
import type { FoundationSnapshot } from "./types";
import type { ActiveImportState, DatasetCatalogEntry, DatasetCatalogProgress, ImportPreviewSession, ImportRequest, SourceInspection } from "./data-ground-types";

const fixtureSnapshot: FoundationSnapshot = {
  project: "MAX Research Ground",
  current_phase: "RG-R04",
  phase_lifecycle: "ACTIVE",
  builder_status: "NOT_STARTED",
  automated_status: "NOT_RUN",
  owner_ui_status: "PENDING",
  owner_acceptance: "PENDING",
  next_phase_unlocked: false,
  deliverables: {
    local_runtime_boundary: "PLANNED",
    model_discovery: "PLANNED",
    model_identity_registry: "PLANNED",
    runtime_adapters: "PLANNED",
    verified_model_input: "PLANNED",
    immutable_inference_records: "PLANNED",
    model_comparison: "PLANNED",
    resource_controls: "PLANNED",
    model_lab_ui: "PLANNED",
    acceptance_evidence: "PLANNED",
  },
  pages: [
    { id: "overview", status: "IMPLEMENTED" },
    { id: "data-ground", status: "OWNER_ACCEPTED" },
    { id: "feature-studio", status: "OWNER_ACCEPTED" },
    { id: "strategy-studio", status: "OWNER_ACCEPTED" },
    { id: "model-lab", status: "ACTIVE_NOT_STARTED" },
    { id: "policy-studio", status: "PLANNED" },
    { id: "experiments", status: "PLANNED" },
    { id: "results", status: "PLANNED" },
  ],
  accepted_phases: [
    {
      phase: "RG-R00",
      owner_acceptance: "OWNER_ACCEPTED",
      owner_ui_status: "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED",
      source_sha: "a1f7ada003d57a26dcd4dfa41e86ba62a5890bed",
      ci_run_url: "https://github.com/maxqstudio/max-tev-ground/actions/runs/39037310698",
      owner_issue_url: "https://github.com/maxqstudio/max-tev-ground/issues/1",
    },
    {
      phase: "RG-R01",
      owner_acceptance: "OWNER_ACCEPTED",
      owner_ui_status: "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED",
      source_sha: "7f50b398ca272c4c84b5e5c01866945e9eda2a2a",
      source_tree_sha: "46c8969bc31faa7f3efaa6474d5ced2322270623",
      ci_run_url: "https://github.com/maxqstudio/max-tev-ground/actions/runs/39285471335",
      owner_issue_url: "https://github.com/maxqstudio/max-tev-ground/issues/2#issuecomment-6269930193",
      known_debts: ["cancelPreview may report a generic error when cancellation succeeds but registry refresh fails; unresolved UX debt, not fixed or manually tested."],
    },
    {
      phase: "RG-R02",
      owner_acceptance: "OWNER_ACCEPTED",
      owner_ui_status: "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED",
      source_sha: "59e0f2e7bd5a875b661bb608d289b3b21b595bd2",
      source_tree_sha: "326b01e72b915746be2134d00a2d0d1fa737d853",
      ci_run_url: "https://github.com/maxqstudio/max-tev-ground/actions/runs/39405958946",
      owner_issue_url: "https://github.com/maxqstudio/max-tev-ground/issues/4",
      known_debts: [
        "Manual desktop click-through was NOT_RUN; the Owner waived UI review (UI_REVIEW_WAIVED), so interactive use remains unverified.",
        "The Owner finds the UI confusing and visually unsatisfactory; visual redesign is deferred, not accepted or waived.",
      ],
    },
    {
      phase: "RG-R03",
      owner_acceptance: "OWNER_ACCEPTED",
      owner_ui_status: "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED",
      source_sha: "3d3ceeb34958130f1e3f67309d0a213d10c971c3",
      source_tree_sha: "4e8f1542a1d6a3872219c1dad9747dde5841bd88",
      ci_run_url: "https://github.com/maxqstudio/max-tev-ground/actions/runs/39453334794",
      owner_issue_url: "https://github.com/maxqstudio/max-tev-ground/issues/7#issuecomment-6295197683",
      known_debts: [
        "Manual desktop click-through of Strategy Studio was NOT_RUN; the Owner accepted the deliverable without manual UI review (UI_REVIEW_WAIVED), so interactive use remains unverified.",
        "The RG-R03 build upgrades an existing research database from schema 6 to 7 on first start; no Owner-profile runtime test exists, and upgrading the Owner's research.sqlite3 is NOT AUTHORIZED by this acceptance.",
        "RG-R02 maintenance debt: the Feature Studio feature-output quota is checked before the Parquet footer is written; recorded on Issue #4 for an Owner-approved targeted repair and unresolved.",
      ],
    },
  ],
};

const emptyCatalog: DatasetCatalogProgress = {
  jobId: "catalog-fixture-job",
  status: "COMPLETED",
  completed: 0,
  total: 0,
  datasets: [],
  error: null,
};

const freezeJobId = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c61";

function sourceInspectionFixture(fileName: string): SourceInspection {
  return {
    file_name: fileName,
    extension: "csv",
    size_bytes: 302,
    headers: ["timestamp", "symbol", "timeframe", "open", "high", "low", "close", "volume"],
    sample_rows: [{ timestamp: "2024-01-08T00:00:00Z", symbol: "EURUSD.m", timeframe: "H1", open: "1.1", high: "1.2", low: "1.0", close: "1.15", volume: "4" }],
  };
}

function previewSessionFixture(sourceName: string): ImportPreviewSession {
  return {
    jobId: freezeJobId,
    preview: {
      dataset_id: `ds_${"a".repeat(64)}`,
      fingerprint: "a".repeat(64),
      status: "READY",
      source_file_name: sourceName,
      source_format: "csv",
      source_size_bytes: 302,
      source_sha256: "b".repeat(64),
      raw_sha256: "b".repeat(64),
      canonical_bars_sha256: "c".repeat(64),
      canonical_bars_fingerprint: "d".repeat(64),
      parquet_sha256: "c".repeat(64),
      manifest_sha256: "e".repeat(64),
      bar_count: 1,
      series_count: 1,
      series: [{ symbol: "EURUSD.m", timeframe: "H1", market_role: "TARGET", bar_count: 1, first_open_utc: "2024-01-08T00:00:00+00:00", last_open_utc: "2024-01-08T00:00:00+00:00", missing_bars: 0, closed_session_bars: 0, unclassified_gap_bars: 0 }],
      quality_audit: { duplicate_bars: 0, descending_bars: 0, missing_bars: 0, closed_session_bars: 0, unclassified_gap_bars: 0, gaps: [] },
    },
  };
}

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
      if (command === "start_dataset_catalog_verification") return emptyCatalog;
      if (command === "get_dataset_catalog_progress" || command === "get_active_data_import") return null;
      if (command === "list_import_attempts") return [];
      return undefined;
    });
  });

  it("loads the governance snapshot through the Tauri IPC boundary", async () => {
    render(<App />);
    expect(await screen.findByRole("heading", { name: "MAX Research Ground" })).toBeTruthy();
    expect(screen.getByText("Builder status").nextElementSibling?.textContent).toBe("NOT STARTED");
    expect(screen.getByText("Automated gates").nextElementSibling?.textContent).toBe("NOT RUN");
    expect(screen.getByText("Owner UI review").nextElementSibling?.textContent).toBe("PENDING");
    expect(screen.getAllByText("LOCKED")).toHaveLength(2);
    expect(screen.getByText("Model identity registry")).toBeTruthy();
    expect(screen.getByText(/RG-R02: Owner accepted\. UI review waived, not tested\./)).toBeTruthy();
    expect(screen.getByText(/RG-R03: Owner accepted\. UI review waived, not tested\./)).toBeTruthy();
    expect(screen.getByText(/visual redesign is deferred, not accepted or waived/)).toBeTruthy();
    expect(screen.getByText(/research\.sqlite3 is NOT AUTHORIZED/)).toBeTruthy();
  });

  it("keeps accepted Feature and Strategy Studios usable and shows Model Lab as authorized but not started", async () => {
    mockIPC((command) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      if (command === "get_feature_registry") {
        return {
          engineId: "rg-native-features",
          engineVersion: "1.0.0",
          temporalSemantics: { version: 1, decision_boundary: "", current_bar: "", window_basis: "", warmup: "", reference_join: "", reference_staleness: "", gaps: "" },
          indicators: [],
          limits: { maxFeatures: 32, maxPeriod: 500, maxStalenessSeconds: 1, maxAvailabilityLagSeconds: 0, previewRows: 20 },
          researchNotice: "Derived research features; not trading signals.",
        };
      }
      if (command === "list_feature_inputs" || command === "list_feature_sets" || command === "list_feature_attempts") return [];
      if (command === "get_strategy_registry") {
        return {
          engineId: "rg-native-strategies",
          engineVersion: "1.0.0",
          decisionSemantics: { version: 1, decision_rows: "", causality: "", context: "", skip_precedence: "", equality: "", conflict: "" },
          strategies: [],
          previewRows: 20,
          researchNotice: "Research decisions; they are not orders.",
        };
      }
      if (command === "list_decision_inputs" || command === "list_decision_sets" || command === "list_decision_attempts") return [];
      if (command === "get_active_decision_job") return null;
      if (command === "get_active_feature_job") return null;
      return undefined;
    });
    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /Feature Studio/ }));
    expect(await screen.findByRole("heading", { level: 1, name: "Feature Studio" })).toBeTruthy();
    expect(await screen.findByText(/No READY verified dataset is registered/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Strategy Studio/ }));
    expect(await screen.findByRole("heading", { level: 1, name: "Strategy Studio" })).toBeTruthy();
    expect(await screen.findByText(/No READY feature set is registered/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Model Lab/ }));
    expect(await screen.findByRole("heading", { name: "Model Lab implementation has not started" })).toBeTruthy();
    expect(screen.getByText("ACTIVE NOT STARTED")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Policy Studio/ }));
    expect(await screen.findByRole("heading", { name: "Policy Studio is not implemented yet" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Overview/ }));
    expect(await screen.findByText(/Desktop UI and CSV import review were waived, NOT TESTED/)).toBeTruthy();
    expect(screen.getAllByText(/Known debt \(unresolved\)/)).toHaveLength(6);
  });

  it("shows bounded catalog verification progress and cancellation", async () => {
    let cancelled = false;
    const running: DatasetCatalogProgress = {
      jobId: "catalog-large-fixture",
      status: "RUNNING",
      completed: 0,
      total: 2,
      datasets: [],
      error: null,
    };
    mockIPC((command) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      if (command === "start_dataset_catalog_verification") return running;
      if (command === "get_dataset_catalog_progress") return cancelled ? { ...running, status: "CANCELLED" } : running;
      if (command === "cancel_dataset_catalog_verification") {
        cancelled = true;
        return "CANCELLATION_REQUESTED";
      }
      if (command === "get_active_data_import") return null;
      if (command === "list_import_attempts") return [];
      return undefined;
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /Data Ground/ }));
    expect(await screen.findByText(/Verifying snapshot integrity/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel verification" }));
    expect(await screen.findByText(/Verification cancelled · 0 \/ 2 snapshots checked/)).toBeTruthy();
  });

  it("shows the authorized Data Ground import flow", async () => {
    render(<App />);
    await screen.findByRole("heading", { name: "MAX Research Ground" });
    fireEvent.click(screen.getByRole("button", { name: /Data Ground/ }));

    expect(screen.getByRole("heading", { name: "Data Ground" })).toBeTruthy();
    expect(await screen.findByText("No datasets registered yet.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Choose CSV or Parquet" })).toBeTruthy();
  });

  it("uses the selected CSV delimiter for initial and repeated header inspection", async () => {
    const inspectionDelimiters: unknown[] = [];
    const fileName = "synthetic-delimited.csv";
    mockIPC((command, payload) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      if (command === "start_dataset_catalog_verification") return emptyCatalog;
      if (command === "get_dataset_catalog_progress" || command === "get_active_data_import") return null;
      if (command === "list_import_attempts") return [];
      if (command === "select_data_source") return { selectionId: "delimiter-fixture-token", fileName };
      if (command === "inspect_selected_source") {
        const request = typeof payload === "object" && payload !== null && !Array.isArray(payload)
          ? payload as Record<string, unknown>
          : {};
        const delimiter = request.delimiter;
        inspectionDelimiters.push(delimiter);
        return {
          selectionId: "delimiter-fixture-token",
          inspection: {
            file_name: fileName,
            extension: "csv",
            size_bytes: 120,
            headers: delimiter === "COMMA" ? ["timestamp;open;high;low;close;volume"] : ["timestamp", "open", "high", "low", "close", "volume"],
            sample_rows: [{ timestamp: "2024-01-08T00:00:00Z", open: "1", high: "2", low: "0.5", close: "1.5", volume: "1" }],
          },
        };
      }
      return undefined;
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /Data Ground/ }));
    await screen.findByText("No datasets registered yet.");
    fireEvent.change(screen.getByLabelText("CSV delimiter"), { target: { value: "SEMICOLON" } });
    fireEvent.click(screen.getByRole("button", { name: "Choose CSV or Parquet" }));
    expect(await screen.findByText(fileName)).toBeTruthy();
    expect(inspectionDelimiters).toEqual(["SEMICOLON"]);
    expect((screen.getByLabelText("CSV delimiter") as HTMLSelectElement).value).toBe("SEMICOLON");
    expect(Array.from((screen.getByLabelText("Open") as HTMLSelectElement).options).map((option) => option.value)).toContain("open");

    fireEvent.change(screen.getByLabelText("CSV delimiter"), { target: { value: "TAB" } });
    expect(await screen.findByText(/headers re-inspected/)).toBeTruthy();
    expect(inspectionDelimiters).toEqual(["SEMICOLON", "TAB"]);
  });

  it("previews a labeled fixture before registration, then freezes it after explicit confirmation", async () => {
    let imported = false;
    let activeImport: ActiveImportState | null = null;
    const previewRequests: ImportRequest[] = [];
    const sourceName = "synthetic-fixture-eurusd.csv";
    const inspection = sourceInspectionFixture(sourceName);
    const preview = previewSessionFixture(sourceName);
    const dataset: DatasetCatalogEntry = {
      dataset_id: preview.preview.dataset_id,
      fingerprint: preview.preview.fingerprint,
      manifest_sha256: preview.preview.manifest_sha256,
      status: "READY",
      manifest: {
        schema_version: 1,
        dataset_id: preview.preview.dataset_id,
        fingerprint: preview.preview.fingerprint,
        created_at_utc: "2026-10-17T00:00:00Z",
        provenance: { source_id: "local-file-import", broker: "Fixture", server: "Fixture", source_file_name: sourceName, source_format: "csv", source_size_bytes: 302, raw_sha256: preview.preview.raw_sha256 },
        import_request: {
          mapping: { timestamp: "timestamp", open: "open", high: "high", low: "low", close: "close", volume: "volume", symbol: "symbol", timeframe: "timeframe" },
          timestamp_semantics: "BAR_OPENS_AT", naive_timestamp_offset_minutes: 0, default_symbol: "EURUSD.m", default_timeframe: "H1", default_market_role: "TARGET", market_roles: {}, source_id: "local-file-import", broker: "Fixture", server: "Fixture", delimiter: "COMMA", calendar: { kind: "CONTINUOUS" }, quotas: { max_source_bytes: 1000000, max_records: 100 },
        },
        canonical_bars_file: "bars.parquet", canonical_bars_sha256: preview.preview.canonical_bars_sha256, canonical_bars_fingerprint: preview.preview.canonical_bars_fingerprint, bar_count: 1, status: "READY", series: preview.preview.series, quality_audit: preview.preview.quality_audit,
      },
      integrity: { manifest_valid: true, raw_source_valid: true, canonical_parquet_valid: true },
      diagnostic: null,
    };
    mockIPC((command, payload) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      if (command === "start_dataset_catalog_verification") return { ...emptyCatalog, datasets: imported ? [dataset] : [], completed: imported ? 1 : 0, total: imported ? 1 : 0 };
      if (command === "get_dataset_catalog_progress") return null;
      if (command === "get_active_data_import") return activeImport;
      if (command === "list_import_attempts") return imported ? [{ attempt_id: "attempt", started_at_utc: "2026-10-17T00:00:00Z", completed_at_utc: "2026-10-17T00:00:01Z", status: "IMPORTED", source_file_name: sourceName, diagnostic_code: null, diagnostic_message: null }] : [];
      if (command === "select_data_source") return { selectionId: "opaque-fixture-token", fileName: sourceName };
      if (command === "inspect_selected_source") return { selectionId: "opaque-fixture-token", inspection };
      if (command === "preview_data_import") {
        if (typeof payload === "object" && payload !== null && !Array.isArray(payload) && "request" in payload) {
          previewRequests.push((payload as Record<string, unknown>).request as ImportRequest);
        }
        activeImport = { state: "PREPARED", session: preview };
        return preview;
      }
      if (command === "confirm_data_import") {
        imported = true;
        activeImport = null;
        return { dataset_id: dataset.dataset_id, fingerprint: dataset.fingerprint, status: "READY", disposition: "IMPORTED" };
      }
      return undefined;
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /Data Ground/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Choose CSV or Parquet" }));
    expect(await screen.findByText(sourceName)).toBeTruthy();
    fireEvent.change(screen.getByLabelText("Broker"), { target: { value: "Fixture Broker" } });
    fireEvent.change(screen.getByLabelText("Server"), { target: { value: "Fixture Server" } });
    fireEvent.change(screen.getByLabelText("Trading calendar assumption"), { target: { value: "CUSTOM" } });
    fireEvent.change(screen.getByLabelText("Custom closed intervals (UTC)"), { target: { value: "2024-12-25T00:00:00Z,2024-12-26T00:00:00Z" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview" }));
    expect(await screen.findByRole("heading", { name: /Audit complete/ })).toBeTruthy();
    expect(previewRequests[0]?.calendar).toEqual({ kind: "CUSTOM_CLOSED_INTERVALS", intervals: [{ start_utc: "2024-12-25T00:00:00Z", end_utc: "2024-12-26T00:00:00Z" }] });
    expect(screen.getByRole("button", { name: "Freeze and register dataset" })).toBeTruthy();
    expect(screen.getByText("No datasets registered yet.")).toBeTruthy();
    expect(screen.getByText(/Canonical Parquet SHA-256/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /Overview/ }));
    fireEvent.click(screen.getByRole("button", { name: /Data Ground/ }));
    expect(await screen.findByRole("heading", { name: /Audit complete/ })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Freeze and register dataset" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Freeze and register dataset" }));
    expect(await screen.findByText(/Dataset frozen and registered/)).toBeTruthy();
    expect(screen.getByText("READY")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Inspect" })).toBeTruthy();
    expect(screen.getByText("IMPORTED")).toBeTruthy();
  });

  it("releases a failed confirmation so the stale preview and its controls are not offered", async () => {
    const sourceName = "synthetic-fixture-failed-commit.csv";
    const inspection = sourceInspectionFixture(sourceName);
    const preview = previewSessionFixture(sourceName);
    let activeImport: ActiveImportState | null = null;
    mockIPC((command) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      if (command === "start_dataset_catalog_verification") return emptyCatalog;
      if (command === "get_dataset_catalog_progress") return null;
      if (command === "get_active_data_import") return activeImport;
      if (command === "list_import_attempts") return [];
      if (command === "select_data_source") return { selectionId: "failed-commit-token", fileName: sourceName };
      if (command === "inspect_selected_source") return { selectionId: "failed-commit-token", inspection };
      if (command === "preview_data_import") {
        activeImport = { state: "PREPARED", session: preview };
        return preview;
      }
      if (command === "confirm_data_import") {
        // The backend removes the prepared session before committing, so a failed commit leaves nothing to confirm or cancel.
        activeImport = null;
        return Promise.reject("dataset commit failed: fixture persistence failure");
      }
      return undefined;
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /Data Ground/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Choose CSV or Parquet" }));
    await screen.findByText(sourceName);
    fireEvent.change(screen.getByLabelText("Default symbol"), { target: { value: "EURUSD.m" } });
    fireEvent.change(screen.getByLabelText("Broker"), { target: { value: "Fixture Broker" } });
    fireEvent.change(screen.getByLabelText("Server"), { target: { value: "Fixture Server" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview" }));
    await screen.findByRole("heading", { name: /Audit complete/ });
    fireEvent.click(screen.getByRole("button", { name: "Freeze and register dataset" }));

    expect(await screen.findByText(/dataset commit failed/)).toBeTruthy();
    expect(screen.queryByRole("heading", { name: /Audit complete/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Freeze and register dataset" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Cancel preview" })).toBeNull();
    expect((screen.getByRole("button", { name: "Choose CSV or Parquet" }) as HTMLButtonElement).disabled).toBe(false);
    expect((screen.getByRole("button", { name: "Validate and preview" }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("reports a committed import as registered when only the catalogue refresh fails afterwards", async () => {
    const sourceName = "synthetic-fixture-refresh-failure.csv";
    const inspection = sourceInspectionFixture(sourceName);
    const preview = previewSessionFixture(sourceName);
    let committed = false;
    let activeImport: ActiveImportState | null = null;
    mockIPC((command) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      if (command === "start_dataset_catalog_verification") {
        return committed ? Promise.reject("catalog verifier could not start: fixture refresh failure") : emptyCatalog;
      }
      if (command === "get_dataset_catalog_progress") return null;
      if (command === "get_active_data_import") return activeImport;
      if (command === "list_import_attempts") return [];
      if (command === "select_data_source") return { selectionId: "refresh-failure-token", fileName: sourceName };
      if (command === "inspect_selected_source") return { selectionId: "refresh-failure-token", inspection };
      if (command === "preview_data_import") {
        activeImport = { state: "PREPARED", session: preview };
        return preview;
      }
      if (command === "confirm_data_import") {
        committed = true;
        activeImport = null;
        return { dataset_id: preview.preview.dataset_id, fingerprint: preview.preview.fingerprint, status: "READY", disposition: "IMPORTED" };
      }
      return undefined;
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /Data Ground/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Choose CSV or Parquet" }));
    await screen.findByText(sourceName);
    fireEvent.change(screen.getByLabelText("Default symbol"), { target: { value: "EURUSD.m" } });
    fireEvent.change(screen.getByLabelText("Broker"), { target: { value: "Fixture Broker" } });
    fireEvent.change(screen.getByLabelText("Server"), { target: { value: "Fixture Server" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview" }));
    await screen.findByRole("heading", { name: /Audit complete/ });
    fireEvent.click(screen.getByRole("button", { name: "Freeze and register dataset" }));

    expect(await screen.findByText(/Dataset frozen and registered/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Freeze and register dataset" })).toBeNull();
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("committed, but the dataset registry did not refresh");
    expect(alert.textContent).toContain("catalog verifier could not start");
  });

  it("keeps an explicit preview cancel path before dataset registration", async () => {
    const inspection: SourceInspection = {
      file_name: "synthetic-fixture.csv", extension: "csv", size_bytes: 200,
      headers: ["time", "open", "high", "low", "close", "volume"],
      sample_rows: [{ time: "2024-01-08T00:00:00Z", open: "1", high: "2", low: "0.5", close: "1.5", volume: "1" }],
    };
    mockIPC((command) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      if (command === "start_dataset_catalog_verification") return emptyCatalog;
      if (command === "get_dataset_catalog_progress" || command === "get_active_data_import") return null;
      if (command === "list_import_attempts") return [];
      if (command === "select_data_source") return { selectionId: "fixture-token", fileName: inspection.file_name };
      if (command === "inspect_selected_source") return { selectionId: "fixture-token", inspection };
      if (command === "preview_data_import") return {
        jobId: "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c61",
        preview: {
          dataset_id: `ds_${"a".repeat(64)}`, fingerprint: "a".repeat(64), status: "READY", source_file_name: inspection.file_name, source_format: "csv", source_size_bytes: 200, source_sha256: "b".repeat(64), raw_sha256: "b".repeat(64), canonical_bars_sha256: "c".repeat(64), canonical_bars_fingerprint: "d".repeat(64), parquet_sha256: "c".repeat(64), manifest_sha256: "e".repeat(64), bar_count: 1, series_count: 1,
          series: [{ symbol: "EURUSD.m", timeframe: "H1", market_role: "TARGET", bar_count: 1, first_open_utc: "2024-01-08T00:00:00+00:00", last_open_utc: "2024-01-08T00:00:00+00:00", missing_bars: 0, closed_session_bars: 0, unclassified_gap_bars: 0 }],
          quality_audit: { duplicate_bars: 0, descending_bars: 0, missing_bars: 0, closed_session_bars: 0, unclassified_gap_bars: 0, gaps: [] },
        },
      };
      if (command === "cancel_data_import") return "CANCELLED";
      return undefined;
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /Data Ground/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Choose CSV or Parquet" }));
    await screen.findByText(inspection.file_name);
    fireEvent.change(screen.getByLabelText("Default symbol"), { target: { value: "EURUSD.m" } });
    fireEvent.change(screen.getByLabelText("Broker"), { target: { value: "Fixture Broker" } });
    fireEvent.change(screen.getByLabelText("Server"), { target: { value: "Fixture Server" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview" }));
    await screen.findByRole("heading", { name: /Audit complete/ });
    fireEvent.click(screen.getByRole("button", { name: "Cancel preview" }));
    expect(await screen.findByText("Preview cancelled. No dataset was added to the registry.")).toBeTruthy();
    expect(screen.getByText("No datasets registered yet.")).toBeTruthy();
  });

  it("requests cancellation while the source scan is still running", async () => {
    const previewControl: { reject?: (reason?: unknown) => void } = {};
    const inspection: SourceInspection = {
      file_name: "synthetic-fixture-slow.csv", extension: "csv", size_bytes: 200,
      headers: ["time", "open", "high", "low", "close", "volume"],
      sample_rows: [{ time: "2024-01-08T00:00:00Z", open: "1", high: "2", low: "0.5", close: "1.5", volume: "1" }],
    };
    mockIPC((command) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      if (command === "start_dataset_catalog_verification") return emptyCatalog;
      if (command === "get_dataset_catalog_progress" || command === "get_active_data_import") return null;
      if (command === "list_import_attempts") return [];
      if (command === "select_data_source") return { selectionId: "slow-fixture-token", fileName: inspection.file_name };
      if (command === "inspect_selected_source") return { selectionId: "slow-fixture-token", inspection };
      if (command === "preview_data_import") return new Promise((_resolve, reject) => { previewControl.reject = reject; });
      if (command === "cancel_data_import") return "CANCELLATION_REQUESTED";
      return undefined;
    });

    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /Data Ground/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Choose CSV or Parquet" }));
    await screen.findByText(inspection.file_name);
    fireEvent.change(screen.getByLabelText("Default symbol"), { target: { value: "EURUSD.m" } });
    fireEvent.change(screen.getByLabelText("Broker"), { target: { value: "Fixture Broker" } });
    fireEvent.change(screen.getByLabelText("Server"), { target: { value: "Fixture Server" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview" }));
    fireEvent.click(await screen.findByRole("button", { name: "Cancel import" }));
    expect(await screen.findByText("Cancellation requested. The importer will stop at its next safe checkpoint.")).toBeTruthy();
    previewControl.reject?.(new Error("import cancelled"));
    expect(await screen.findByText("Import cancelled. No dataset was added to the registry.")).toBeTruthy();
    expect(screen.getByText("No datasets registered yet.")).toBeTruthy();
  });

  it("discloses an IPC failure instead of fabricating a status", async () => {
    mockIPC((command) => {
      if (command === "get_foundation_status") {
        return Promise.reject(new Error("fixture IPC failure"));
      }
      if (command === "start_dataset_catalog_verification") return emptyCatalog;
      if (command === "get_active_data_import") return null;
      return undefined;
    });
    render(<App />);

    expect((await screen.findByRole("alert")).textContent).toContain("fixture IPC failure");
    expect(screen.queryByText("NOT RUN")).toBeNull();
  });
});
