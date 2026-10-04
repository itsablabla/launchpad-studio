import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test/setupLocalStorage.ts", "./src/test/setupFetchGuard.ts"],
  },
});
