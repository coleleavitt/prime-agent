/**
 * Upstream-compatible `/compat` entrypoint.
 *
 * Upstream pi-ai ships a `/compat` subpath whose API-provider registry is a
 * module-local Map. When an extension imports that subpath and module
 * resolution lands on a second pi-ai copy (its own node_modules), every
 * host-registered custom provider disappears and calls fail with
 * "No API provider registered for api: <api>".
 *
 * This shim re-exports the primary module instance, so extensions importing
 * "@earendil-works/pi-ai/compat" share the exact registry the host registers
 * custom providers into.
 */
export * from "./index.js";
