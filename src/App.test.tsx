import { randomFillSync } from "node:crypto";
import { fireEvent, render, screen } from "@testing-library/react";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import App from "./App";
import type { FoundationSnapshot } from "./types";
import type { DatasetCatalogEntry, ImportPreviewSession, ImportRequest, SourceInspection } from "./data-ground-types";

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
      if (command === "list_datasets" || command === "list_import_attempts") return [];
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
      if (command === "list_datasets" || command === "list_import_attempts") return [];
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
    const previewRequests: ImportRequest[] = [];
    const sourceName = "synthetic-fixture-eurusd.csv";
    const inspection: SourceInspection = {
      file_name: sourceName,
      extension: "csv",
      size_bytes: 302,
      headers: ["timestamp", "symbol", "timeframe", "open", "high", "low", "close", "volume"],
      sample_rows: [{ timestamp: "2024-01-08T00:00:00Z", symbol: "EURUSD.m", timeframe: "H1", open: "1.1", high: "1.2", low: "1.0", close: "1.15", volume: "4" }],
    };
    const preview: ImportPreviewSession = {
      jobId: "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c61",
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
      if (command === "list_datasets") return imported ? [dataset] : [];
      if (command === "list_import_attempts") return imported ? [{ attempt_id: "attempt", started_at_utc: "2026-10-17T00:00:00Z", completed_at_utc: "2026-10-17T00:00:01Z", status: "IMPORTED", source_file_name: sourceName, diagnostic_code: null, diagnostic_message: null }] : [];
      if (command === "select_data_source") return { selectionId: "opaque-fixture-token", fileName: sourceName };
      if (command === "inspect_selected_source") return { selectionId: "opaque-fixture-token", inspection };
      if (command === "preview_data_import") {
        if (typeof payload === "object" && payload !== null && !Array.isArray(payload) && "request" in payload) {
          previewRequests.push((payload as Record<string, unknown>).request as ImportRequest);
        }
        return preview;
      }
      if (command === "confirm_data_import") {
        imported = true;
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

    fireEvent.click(screen.getByRole("button", { name: "Freeze and register dataset" }));
    expect(await screen.findByText(/Dataset frozen and registered/)).toBeTruthy();
    expect(screen.getByText("READY")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Inspect" })).toBeTruthy();
    expect(screen.getByText("IMPORTED")).toBeTruthy();
  });

  it("keeps an explicit preview cancel path before dataset registration", async () => {
    const inspection: SourceInspection = {
      file_name: "synthetic-fixture.csv", extension: "csv", size_bytes: 200,
      headers: ["time", "open", "high", "low", "close", "volume"],
      sample_rows: [{ time: "2024-01-08T00:00:00Z", open: "1", high: "2", low: "0.5", close: "1.5", volume: "1" }],
    };
    mockIPC((command) => {
      if (command === "get_foundation_status") return fixtureSnapshot;
      if (command === "list_datasets" || command === "list_import_attempts") return [];
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
      if (command === "list_datasets" || command === "list_import_attempts") return [];
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
      return undefined;
    });
    render(<App />);

    expect((await screen.findByRole("alert")).textContent).toContain("fixture IPC failure");
    expect(screen.queryByText("NOT RUN")).toBeNull();
  });
});
