# Redis usage and API cache policy

`REDIS_URL` is used for cross-instance collaboration Pub/Sub and optional
server-side read caches. Use a secret manager or an untracked environment file
for credentials; never commit the connection URL.

## Read caches

| Data | Key scope | TTL | Freshness behavior |
| --- | --- | --- | --- |
| Dashboard counts | User | 1 hour | Board create, rename, trash, restore, permanent delete, favorite, and collaborator-membership changes invalidate affected users' caches; the next request repopulates them from PostgreSQL. Billing state and workspace membership are always read from PostgreSQL. |
| Owned board list first page (`scope=owned&limit=12`) | User | 5 seconds | Only the unfiltered first page is cached; create, duplicate, rename, delete, restore, and favorite changes invalidate it. The next GET refills it from PostgreSQL. |
| Saved-template list | User | 60 seconds | The cache is deleted after save, import, and delete operations. |
| Collaboration snapshot elements | Board and current revision | 1 hour | Authorization and current revision are checked before a cache hit is used; older revisions have separate keys and expire. |

Values larger than 1 MiB are not cached. If Redis cache reads or writes fail
after startup, the handler logs a warning and continues with PostgreSQL. With
`REDIS_URL` configured, Redis initialization at startup remains required by
the existing collaboration Pub/Sub setup.

Other board-list query variants, notifications, public-link responses,
authentication, permissions, billing state, and mutations are not
response-cached. In particular, public responses retain their existing
`no-store` behavior.

## Local Compose

Compose always connects the backend to its local Redis service at
`redis://redis:6379`, even if `REDIS_URL` is set in the host environment or
`.env` file. When running the backend outside Compose, set `REDIS_URL` directly
to select a Redis instance. For hosted Redis, use a TLS URL (`rediss://`) and a
rotated credential.
