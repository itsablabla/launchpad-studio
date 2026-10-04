// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { isVersionAtLeast, CLI_TEMPLATES } from "../formPrimitives";

describe("isVersionAtLeast", () => {
    it("accepts equal and newer versions", () => {
        expect(isVersionAtLeast("0.200.0", "0.200.0")).toBe(true);
        expect(isVersionAtLeast("0.232.0", "0.200.0")).toBe(true);
        expect(isVersionAtLeast("1.0.0", "0.200.0")).toBe(true);
    });

    it("rejects older versions", () => {
        // The stale-shadow case this exists for: Homebrew droid 0.129.0.
        expect(isVersionAtLeast("0.129.0", "0.200.0")).toBe(false);
        expect(isVersionAtLeast("0.199.9", "0.200.0")).toBe(false);
    });

    it("pads missing components with zeros", () => {
        expect(isVersionAtLeast("0.200", "0.200.0")).toBe(true);
        expect(isVersionAtLeast("0.199", "0.200.0")).toBe(false);
    });

    it("treats unparseable pieces as zero rather than crashing", () => {
        expect(isVersionAtLeast("beta", "0.200.0")).toBe(false);
        expect(isVersionAtLeast("0.232.0-beta.1", "0.200.0")).toBe(true);
    });
});

describe("CLI_TEMPLATES", () => {
    it("pins a minimum version only for droid", () => {
        const withMin = CLI_TEMPLATES.filter((t) => t.minVersion);
        expect(withMin.map((t) => t.id)).toEqual(["droid"]);
    });
});
