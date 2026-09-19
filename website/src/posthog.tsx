/**
 * Analytics is **disabled** in this fork (empty token), matching the app.
 * The live loader lives in `posthog.live.ts` and is not imported, so Vite
 * never resolves `posthog-js`. Enable later by swapping the import in
 * `__root.tsx`.
 */

export function initPostHog() {}

export function captureEvent(
  _event: string,
  _properties?: Record<string, unknown>,
) {}

export function PostHogAnalytics() {
  return null;
}
