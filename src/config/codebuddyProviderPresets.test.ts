import { describe, expect, it } from "vitest";
import {
  CODEBUDDY_API_FORMATS,
  codebuddyProviderPresets,
} from "./codebuddyProviderPresets";

describe("codebuddyProviderPresets", () => {
  it("starts with the self-hosted gateway preset", () => {
    const [gateway] = codebuddyProviderPresets;
    expect(gateway.providerKey).toBe("gateway");
    expect(gateway.settingsConfig.api).toBe("openai-completions");
    expect(gateway.settingsConfig.options.baseURL).toMatch(/\/v1$/);
    expect(gateway.settingsConfig.options.apiKey).toBe("");
  });

  it("only offers protocols and options models.json can express", () => {
    for (const preset of codebuddyProviderPresets) {
      expect(CODEBUDDY_API_FORMATS).toContain(preset.settingsConfig.api);
      expect(preset.settingsConfig.options.headers).toBeUndefined();
    }
  });

  it("maps catalog models to limits the backend writes as token caps", () => {
    const catalog = codebuddyProviderPresets.slice(1);
    expect(catalog.length).toBeGreaterThan(0);
    for (const preset of catalog) {
      const models = Object.values(preset.settingsConfig.models);
      expect(models.length).toBeGreaterThan(0);
      for (const model of models) {
        expect(typeof model.limit?.context).toBe("number");
      }
    }
  });

  it("keeps provider keys unique", () => {
    const keys = codebuddyProviderPresets.map((preset) => preset.providerKey);
    expect(new Set(keys).size).toBe(keys.length);
  });
});
