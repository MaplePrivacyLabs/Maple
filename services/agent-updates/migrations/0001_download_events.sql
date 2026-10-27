-- Response starts, not unique users, completed transfers, or installations.
-- No path, query, IP, raw User-Agent, cookies, or client identifiers are stored.
CREATE TABLE download_events (
  event_id TEXT PRIMARY KEY NOT NULL,
  observed_at TEXT NOT NULL,
  product TEXT NOT NULL CHECK (product IN ('agent', 'research')),
  host TEXT NOT NULL CHECK (host IN ('updates.maple.ai', 'updates.trymaple.ai')),
  version TEXT,
  platform TEXT NOT NULL CHECK (platform IN ('macos', 'windows', 'linux', 'android', 'all')),
  artifact TEXT NOT NULL CHECK (artifact IN ('macos-dmg', 'macos-updater', 'windows-installer', 'linux-bundle', 'linux-appimage', 'linux-deb', 'linux-rpm', 'android-apk', 'latest', 'manifest')),
  kind TEXT NOT NULL CHECK (kind IN ('artifact', 'alias', 'metadata')),
  method TEXT NOT NULL CHECK (method IN ('GET', 'HEAD')),
  status INTEGER NOT NULL CHECK (status BETWEEN 200 AND 599),
  range TEXT NOT NULL CHECK (range IN ('none', 'requested', 'partial', 'unsatisfiable')),
  result TEXT NOT NULL CHECK (result IN ('success', 'redirect', 'not_modified', 'client_error', 'server_error')),
  response_length INTEGER CHECK (response_length BETWEEN 0 AND 4294967296),
  client_kind TEXT NOT NULL CHECK (client_kind IN ('browser', 'maple', 'cli', 'other')),
  is_probe INTEGER NOT NULL CHECK (is_probe IN (0, 1))
);

CREATE INDEX download_events_observed_at ON download_events(observed_at);
CREATE INDEX download_events_trends ON download_events(product, kind, observed_at);
