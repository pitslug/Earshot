// Generated from `storage_settings::retention_options` by
// `EARSHOT_UPDATE_BINDINGS=1 cargo test -p earshot-desktop bindings`; do not edit.

import type { RetentionOption } from "./bindings";

/** The retention choices as the app sends them (for the mock). */
export const RETENTION_CHOICES: RetentionOption[] = [
  {
    "value": "1h",
    "label": "1 hour",
    "keep_ms": 3600000
  },
  {
    "value": "4h",
    "label": "4 hours",
    "keep_ms": 14400000
  },
  {
    "value": "12h",
    "label": "12 hours",
    "keep_ms": 43200000
  },
  {
    "value": "1d",
    "label": "1 day",
    "keep_ms": 86400000
  },
  {
    "value": "3d",
    "label": "3 days",
    "keep_ms": 259200000
  },
  {
    "value": "7d",
    "label": "7 days",
    "keep_ms": 604800000
  },
  {
    "value": "14d",
    "label": "14 days",
    "keep_ms": 1209600000
  },
  {
    "value": "28d",
    "label": "28 days",
    "keep_ms": 2419200000
  },
  {
    "value": "90d",
    "label": "3 months",
    "keep_ms": 7776000000
  },
  {
    "value": "180d",
    "label": "6 months",
    "keep_ms": 15552000000
  },
  {
    "value": "365d",
    "label": "1 year",
    "keep_ms": 31536000000
  },
  {
    "value": "forever",
    "label": "Forever",
    "keep_ms": null
  }
];
