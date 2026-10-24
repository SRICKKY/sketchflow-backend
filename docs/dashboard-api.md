# Dashboard REST API

These routes use the Rust backend's **lowercase SQL schema**, not the Next.js
Prisma database. Startup runs migrations, including `0003_dashboard.sql` for
saved templates. Do not point a test run at Neon or a production database.

All endpoints require the `sketchflow_session` cookie. JSON requests use
`Content-Type: application/json`; browser requests must include credentials.
Configure the frontend origin in `CORS_ALLOWED_ORIGINS`. CORS allows PATCH.
DTO property names are camelCase, role values are uppercase, and dates are UTC
RFC 3339 strings. Unless specified otherwise, successful requests return 200.

## DTOs

```ts
type BoardRole = "OWNER" | "EDITOR" | "VIEWER";
type WorkspaceRole = "OWNER" | "ADMIN" | "MEMBER";

interface BoardSummary {
  id: string;
  name: string;
  ownerId: string;
  workspaceId: string | null;
  role: BoardRole; // current caller's effective role
  createdAt: string;
  updatedAt: string;
  elementCount: number;
}

interface WorkspaceSummary {
  id: string;
  name: string;
  slug: string;
  role: WorkspaceRole; // current caller's role
}

interface Member<Role = BoardRole | WorkspaceRole> {
  id: string; // MEMBERSHIP ID: use this in member mutation URLs
  userId: string;
  name: string | null;
  email: string;
  image: string | null;
  role: Role;
}

interface TemplateSummary {
  id: string;
  name: string;
  description: string | null;
  category: string;
  builtin: boolean;
  elementCount: number;
}
```

## Dashboard and capabilities

`GET /api/dashboard`:

```json
{
  "capabilities": {
    "isPro": false,
    "ownedBoards": 0,
    "maxOwnedBoards": 5,
    "canCreateBoard": true,
    "canSaveTemplates": false
  },
  "workspaces": []
}
```

`workspaces` contains `WorkspaceSummary` rows. `maxOwnedBoards` is `null` for
Pro (unlimited). Free/expired users receive an empty workspace list.

Active Pro is resolved by the existing billing service: a live subscription
or `plan=pro` with a future `plan_expires_at`; a bare Pro label is insufficient.
Free users may own **five personal boards**. Workspace boards do not count.
Free board owners may have **two total board members, including the owner**.
An active Pro owner has no board/member count cap.

Board creation locks the owner row before checking the cap. Membership
mutations lock the resource before checking authorization, uniqueness and caps,
preventing concurrent requests from bypassing limits or owner protections.

## Boards

| Method / endpoint | Request | Response / permission |
| --- | --- | --- |
| `GET /api/boards` | Query below | `{items: BoardSummary[]}` |
| `POST /api/boards` | `{name?, workspaceId?, templateId?}` | 201 `BoardSummary` |
| `GET /api/boards/{id}` | — | `BoardSummary`; viewer or higher |
| `PATCH /api/boards/{id}` | `{name}` | `BoardSummary`; owner only |
| `DELETE /api/boards/{id}` | — | `{success:true}`; owner only |
| `POST /api/boards/{id}/duplicate` | `{name?, workspaceId?}` | 201 `BoardSummary`; source viewer or higher |

List query parameters:

- `scope=owned` (default): caller-owned personal boards.
- `scope=shared`: personal boards directly shared with the caller, excluding
  caller-owned boards.
- `workspaceId=<id>`: list all boards in that workspace instead of the personal
  scope. Requires an active Pro workspace membership.
- `q=<text>`: case-insensitive literal substring of the name, trimmed, at most
  200 characters. `%` and `_` are literal characters, not SQL wildcards.

Lists are ordered by `updatedAt` descending, then ID. There is no pagination or
`nextCursor` in this version. Category filtering and template search are
client-side.

Board names are trimmed and must contain 1–80 characters. The default name is
`Untitled board`, or the selected template's name. Workspace IDs must identify
an existing workspace in which the caller has an active Pro membership
(MEMBER or higher). Send `{}` for a default create/duplicate, not an empty body.
Duplicating without `workspaceId` creates a **personal** board, even when the
source is a workspace board. Templates are private to their creator.

Creating from a template and duplicating copy the persisted elements, mint
fresh element IDs, and remap grouping, frame and binding references. The copied
document starts at collaboration revision **0**, with no source operation log.
They enforce the recipient owner's image cap (10 free, 100 Pro). They do not
mutate the source board or its revision.

Deleting a board cascades its members, elements, collaboration state and
operation history. It does not delete unrelated/shared uploaded assets.

### Board sharing

| Method / endpoint | Request | Response / permission |
| --- | --- | --- |
| `GET /api/boards/{id}/members` | — | `{items: Member<BoardRole>[]}`; viewer or higher |
| `POST /api/boards/{id}/members` | `{email, role:"EDITOR"\|"VIEWER"}` | 201 `Member`; owner only |
| `PATCH /api/boards/{id}/members/{memberId}` | `{role:"EDITOR"\|"VIEWER"}` | `Member`; owner only |
| `DELETE /api/boards/{id}/members/{memberId}` | — | `{success:true}`; owner only |

Email is trimmed and lowercased. The invitee must already have a SketchFlow
account. Adding an existing member returns 409. The owner cannot be removed,
demoted, or assigned via these endpoints. A membership ID from another board
returns 404. Workspace-derived access is not a direct board membership:
removing a direct board grant does not remove an independent workspace grant.

Existing collaboration authorization is preserved: a direct board grant or
board ownership continues to resolve independently of workspace fallback.
Workspace fallback requires active Pro; OWNER/ADMIN resolve as editor and
MEMBER as viewer. Workspace membership grants no ability to delete someone
else's board or manage its direct membership.

## Templates

| Method / endpoint | Request | Response |
| --- | --- | --- |
| `GET /api/templates` | — | `{items: TemplateSummary[]}` |
| `POST /api/templates` | `{name, description?, elements: SketchElement[]}` | 201 `TemplateSummary`; active Pro only |
| `POST /api/boards/{id}/template` | `{name, description?}` | 201 `TemplateSummary`; board owner and active Pro only |
| `DELETE /api/templates/{id}` | — | `{success:true}`; saved-template creator only |

The list contains actual bundled Next.js catalog content, followed by the
caller's saved templates (newest first). Builtin IDs:

- `builtin:system-design`
- `builtin:flowchart`
- `builtin:database-schema`
- `builtin:aws-architecture`
- `builtin:brainstorming`
- `builtin:uml`
- `builtin:sequence-diagram`

Saved templates have `builtin:false` and category `Yours`. Names are 1–80
characters; descriptions at most 200. Blank descriptions normalize to `null`.
Saved templates remain visible/usable/deletable after Pro expires; only saving
requires Pro. Accessing or deleting someone else's saved template returns 404.
Builtins cannot be deleted (400).

Document JSON import is supported by extracting its `elements` array into the
POST body. Elements require unique nonempty IDs (at most 128 bytes) and canvas
types accepted by the collaboration protocol. Templates/copies support at
most 5,000 elements; HTTP JSON bodies use Axum's default 2 MiB limit.

**Not supported:** `appState`, scene/presentation metadata, template previews,
template editing, or external document URLs. Unknown POST fields, including
`appState`, are rejected with 400 rather than silently stored or ignored.
The frontend must explicitly explain that it imports elements only.

## Workspaces

All workspace detail/mutation operations require an **active Pro caller**.
Adding a member also requires an **active Pro invitee**, checked before
insertion. No invitation email or pending invitation record is created:
successful add immediately grants an existing account membership.

| Method / endpoint | Request | Response / minimum role |
| --- | --- | --- |
| `GET /api/workspaces` | — | `{items: WorkspaceSummary[]}`; empty when not Pro |
| `POST /api/workspaces` | `{name}` | 201 `WorkspaceSummary`; caller becomes OWNER |
| `GET /api/workspaces/{id}` | — | `WorkspaceSummary & {members: Member<WorkspaceRole>[]}`; MEMBER |
| `PATCH /api/workspaces/{id}` | `{name}` | `WorkspaceSummary`; ADMIN |
| `DELETE /api/workspaces/{id}` | — | `{success:true}`; OWNER |
| `GET /api/workspaces/{id}/members` | — | `{items: Member<WorkspaceRole>[]}`; MEMBER |
| `POST /api/workspaces/{id}/members` | `{email, role:"ADMIN"\|"MEMBER"}` | 201 `Member`; ADMIN |
| `PATCH /api/workspaces/{id}/members/{memberId}` | `{role:"ADMIN"\|"MEMBER"}` | `Member`; ADMIN |
| `DELETE /api/workspaces/{id}/members/{memberId}` | — | `{success:true}`; ADMIN |
| `POST /api/workspaces/{id}/leave` | No body | `{success:true}`; non-owner MEMBER or higher |

Workspace names are trimmed, 2–40 characters. Slugs are unique and stable on
rename. The owner cannot be removed/demoted; ownership transfer is not exposed.
An owner must delete their workspace rather than leave it. Deleting a workspace
atomically deletes all its boards (including their elements and collaboration
history), memberships, and the workspace itself. It does not convert those
boards to personal boards.

Role-management semantics match the Next.js service: ADMIN may add, update and
remove non-owner members, including other admins. MEMBER may create boards but
not rename/delete the workspace or manage its members.

## Errors

Errors are JSON `{ "error": "Human-readable explanation." }`:

- 400: malformed JSON/unsupported fields, invalid name/email/role/template data.
- 401: missing, invalid or expired session.
- 403: insufficient role, inactive Pro, plan limit, or immutable owner.
- 404: inaccessible/missing resource, unknown account, or cross-resource member.
- 409: account already a member.
- 500: unexpected database/service failure; details are not exposed.

## Existing read-only board preview

`GET /api/boards/{id}/snapshot` returns:

```ts
{ boardId: string; revision: number; elements: SketchElement[] }
```

It requires viewer access. There is **no** board name, `sceneData`, or `appState`
in this response; fetch `GET /api/boards/{id}` for summary metadata. Existing
snapshot/revision consistency and WebSocket collaboration behavior are retained.

## Tests

`cargo fmt --check`, `cargo check`, and `cargo test` require no database. The
dashboard HTTP integration test is ignored by default. Run it against a
**disposable loopback database whose name ends in `_test`**:

```sh
DATABASE_URL='postgres://USER:PASSWORD@127.0.0.1:PORT/sketchflow_dashboard_test' \
DASHBOARD_TEST_DATABASE_URL='postgres://USER:PASSWORD@127.0.0.1:PORT/sketchflow_dashboard_test' \
cargo test routes::dashboard -- --include-ignored
```

The integration test creates a unique schema, runs migrations there, exercises
authenticated HTTP flows and concurrent cap enforcement, then drops its schema.
It rejects non-loopback hosts and database names without the `_test` suffix.
It never relies on the application's configured database for SQL operations.
