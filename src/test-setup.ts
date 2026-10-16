import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";
import { clearMocks } from "@tauri-apps/api/mocks";

afterEach(() => {
  cleanup();
  clearMocks();
});