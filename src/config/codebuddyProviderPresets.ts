import { piProviderPresets } from "./piProviderPresets";
import type { McodeProviderPreset } from "./mcodeProviderPresets";

/** CodeBuddy / WorkBuddy `models.json` only speaks these two OpenAI protocols. */
export const CODEBUDDY_API_FORMATS = [
  "openai-completions",
  "openai-responses",
] as const;

const SUPPORTED_APIS: ReadonlySet<string> = new Set(CODEBUDDY_API_FORMATS);

/**
 * A self-hosted OpenAI-compatible gateway (for example workbuddy2api-hub, or a sub2api group
 * in front of it) that serves the CodeBuddy models to other machines. Fill in the host, the
 * key and fetch the model list from the gateway.
 */
const SELF_HOSTED_GATEWAY: McodeProviderPreset = {
  name: "Self-hosted Gateway",
  nameKey: "codebuddy.presets.selfHostedGateway",
  providerKey: "gateway",
  websiteUrl: "",
  category: "third_party",
  icon: "tencent",
  settingsConfig: {
    name: "Self-hosted Gateway",
    kind: "custom",
    enabled: true,
    api: "openai-completions",
    options: { baseURL: "http://127.0.0.1:8788/v1", apiKey: "" },
    models: {},
  },
};

/** Pi's catalog, minus endpoints that need protocols or request headers models.json lacks. */
const CATALOG_PRESETS: McodeProviderPreset[] = piProviderPresets.flatMap(
  (preset) => {
    const config = preset.settingsConfig;
    if (!SUPPORTED_APIS.has(config.api) || config.headers) return [];
    return [
      {
        ...preset,
        settingsConfig: {
          name: preset.name,
          kind: "custom",
          enabled: true,
          api: config.api,
          options: { baseURL: config.baseUrl, apiKey: config.apiKey },
          models: Object.fromEntries(
            config.models.map((model) => [
              model.id,
              {
                name: model.name,
                modalities: { input: model.input, output: ["text"] },
                limit: {
                  context: model.contextWindow,
                  output: model.maxTokens,
                },
                ...(model.reasoning ? { reasoning: true } : {}),
              },
            ]),
          ),
        },
      },
    ];
  },
);

export const codebuddyProviderPresets: McodeProviderPreset[] = [
  SELF_HOSTED_GATEWAY,
  ...CATALOG_PRESETS,
];
