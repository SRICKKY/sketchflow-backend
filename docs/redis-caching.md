# Redis usage and API cache policy

`REDIS_URL` is used for cross-instance collaboration Pub/Sub and optional
server-side read caches. Use a secret manager or an untracked environment file
for credentials; never commit the connection URL.

## Read caches

| Data | Key scope | TTL | Freshness behavior |
| --- | --- | --- | --- |
| Dashboard counts | User | 5 seconds | Counts may lag briefly; billing state and workspace membership are still read from PostgreSQL. |
| Owned board list first page (`scope=owned&limit=12`) | User | 5 seconds | Only the unfiltered first page is cached; create, duplicate, rename, delete, restore, and favorite changes invalidate it. The next GET refills it from PostgreSQL. |
| Saved-template list | User | 60 seconds | The cache is deleted after save, import, and delete operations. |
| Collaboration snapshot elements | Board and current revision | 30 seconds | Authorization and current revision are checked before a cache hit is used; older revisions have separate keys and expire. |

Values larger than 1 MiB are not cached. If Redis cache reads or writes fail
after startup, the handler logs a warning and continues with PostgreSQL. With
`REDIS_URL` configured, Redis initialization at startup remains required by
the existing collaboration Pub/Sub setup.

Other board-list query variants, notifications, public-link responses,
authentication, permissions, billing state, and mutations are not
response-cached. In particular, public responses retain their existing
`no-store` behavior.

## Local Compose

Compose defaults to its local Redis service. To use a different Redis instance,
set `REDIS_URL` in the local environment; Compose passes it through to the
backend. For hosted Redis, use a TLS URL (`rediss://`) and a rotated credential.
