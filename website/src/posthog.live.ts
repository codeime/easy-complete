/**
 * Live PostHog loader — not imported while this fork has telemetry off.
 * Enable by pointing `posthog.tsx` at this file and setting a project token.
 *
 * The SDK import is written so Vite does not resolve it unless this module
 * is actually imported.
 */
export {};
